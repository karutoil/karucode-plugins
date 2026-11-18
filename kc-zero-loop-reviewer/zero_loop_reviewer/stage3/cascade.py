"""Stage 3 — the dual-commit cascade and the grounded-vote fallback.

The cascade runs one synthesised probe twice: once on the base commit ``b`` and
once on the head commit ``b + delta`` (each checked out in its own ``git
worktree`` under ``settings.paths.work_dir``).  The pair is then classified with
the retention matrix::

    base fail_behavioral + head pass           -> fix_confirmed       (retained)
    base fail_behavioral + head fail_behavioral -> both_fail           (retained: the defect is still there)
    base pass            + head fail_behavioral -> regression_confirmed (retained)
    base pass            + head pass            -> both_pass           (suppressed)
    setup_error on either side                  -> setup_error         (discarded, never counted against the PR)
    timeout / cancelled / sandbox unavailable   -> inconclusive        (never counted against the PR)

Two invariants hold everywhere in this module:

* a generated test's **setup error never counts against the pull request**;
* a **sandbox failure is never turned into approval** — it leaves the candidate
  ``UNVERIFIED`` with a degraded reason and lets Stage 4/5 or a human decide.

``fallback_verify`` is the honest fallback when no mechanical signal exists: it
asks the grounded reviewer k=5 times and tallies accept/reject/uncertain.  Votes
alone never verify a finding: a majority ``accept`` leaves the candidate
``UNVERIFIED`` unless a real perturbation is already recorded on it (which only
Stage 4 can produce), a majority ``reject`` suppresses it, and ``uncertain``
leaves it unverified.
"""

from __future__ import annotations

import uuid
from pathlib import Path
from typing import Any, Callable

from ..config import Settings
from ..errors import ReviewError
from ..gitops import GitError, GitRepo
from ..llm.prompts import grounded_review_prompt
from ..models import (
    Candidate,
    ContextSlice,
    DiffBundle,
    FallbackVerdict,
    FindingStatus,
    ProbeClassification,
    ProbePair,
    ProbeResult,
    ProbeSpec,
    ProbeStatus,
    Verification,
    VerificationKind,
    sha256_hex,
)
from .probe import synthesize
from .sandbox import Sandbox

#: Votes the fallback asks for when no settings are supplied.
FALLBACK_VOTES = 5
FALLBACK_TEMPERATURE = 0.8

#: Pair classifications that are a mechanical signal worth keeping.
RETAINED_CLASSIFICATIONS = frozenset(
    {
        ProbeClassification.OMISSION_CONFIRMED,
        ProbeClassification.FIX_CONFIRMED,
        ProbeClassification.REGRESSION_CONFIRMED,
        ProbeClassification.BOTH_FAIL,
    }
)

#: Statuses that mean "the probe could not be trusted to run".
_INCONCLUSIVE_STATUSES = frozenset(
    {
        ProbeStatus.TIMEOUT,
        ProbeStatus.CANCELLED,
        ProbeStatus.SANDBOX_UNAVAILABLE,
        ProbeStatus.INPUT_DELIVERY_FAILED,
        ProbeStatus.MALFORMED_PROBE,
    }
)


# --------------------------------------------------------------------------- #
# Retention matrix
# --------------------------------------------------------------------------- #


def classify_pair(base: ProbeResult, head: ProbeResult) -> ProbePair:
    """Classify one ``(base, head)`` probe pair; never guesses a signal."""
    detail = f"base={base.status.value} head={head.status.value}"
    if ProbeStatus.SETUP_ERROR in (base.status, head.status):
        return ProbePair(
            base=base,
            head=head,
            classification=ProbeClassification.SETUP_ERROR,
            detail=f"{detail}; the generated probe could not run, so it says nothing about the change",
        )
    if base.status in _INCONCLUSIVE_STATUSES or head.status in _INCONCLUSIVE_STATUSES:
        return ProbePair(
            base=base,
            head=head,
            classification=ProbeClassification.INCONCLUSIVE,
            detail=f"{detail}; no mechanical signal (the probe did not complete on both sides)",
        )
    base_fails = base.status is ProbeStatus.FAIL_BEHAVIORAL
    head_fails = head.status is ProbeStatus.FAIL_BEHAVIORAL
    base_passes = base.status is ProbeStatus.PASS
    head_passes = head.status is ProbeStatus.PASS
    if base_fails and head_passes:
        return ProbePair(
            base=base,
            head=head,
            classification=ProbeClassification.FIX_CONFIRMED,
            detail=f"{detail}; the probe fails before the change and passes after it",
        )
    if base_fails and head_fails:
        return ProbePair(
            base=base,
            head=head,
            classification=ProbeClassification.BOTH_FAIL,
            detail=f"{detail}; the probed behaviour fails on both commits, so the defect is present in the reviewed code",
        )
    if base_passes and head_fails:
        return ProbePair(
            base=base,
            head=head,
            classification=ProbeClassification.REGRESSION_CONFIRMED,
            detail=f"{detail}; the probe passes before the change and fails after it",
        )
    if base_passes and head_passes:
        return ProbePair(
            base=base,
            head=head,
            classification=ProbeClassification.BOTH_PASS,
            detail=f"{detail}; the probed behaviour is unchanged, so the candidate is not reproduced",
        )
    return ProbePair(
        base=base,
        head=head,
        classification=ProbeClassification.INCONCLUSIVE,
        detail=f"{detail}; neither side produced a usable verdict",
    )


def is_retained(classification: ProbeClassification) -> bool:
    return classification in RETAINED_CLASSIFICATIONS


# --------------------------------------------------------------------------- #
# Cascade
# --------------------------------------------------------------------------- #


def _project_files(repo: GitRepo, diff: DiffBundle) -> list[str]:
    try:
        return repo.list_files(diff.head_sha)[:200]
    except ReviewError:
        return []


def _alive(healthz: Callable[[], Any] | None) -> bool:
    if healthz is None:
        return True
    try:
        return bool(healthz())
    except Exception:
        return False


def _worktree(repo: GitRepo, sha: str, work_root: Path, label: str) -> Path:
    work_root = Path(work_root)
    work_root.mkdir(parents=True, exist_ok=True)
    dest = work_root / f"probe-{label}-{uuid.uuid4().hex[:12]}"
    return repo.add_worktree(sha, dest)


def _head_worktree(repo: GitRepo, diff: DiffBundle, work_root: Path) -> Path:
    """Check out the head commit, or rebuild it as base + the delta patch."""
    try:
        return _worktree(repo, diff.head_sha, work_root, "head")
    except GitError:
        tree = _worktree(repo, diff.base_sha, work_root, "head-patch")
        applied = repo.apply_patch(tree, diff.patch_text)
        if applied.code != 0:
            repo.remove_worktree(tree)
            raise GitError(
                "the head commit could not be checked out and the delta patch did not apply to the base",
                detail=(applied.stderr or applied.stdout)[:1000],
            ) from None
        return tree


def _remove(repo: GitRepo, tree: Path | None) -> None:
    if tree is None:
        return
    try:
        repo.remove_worktree(tree)
    except Exception:  # cleanup must never mask the probe outcome
        pass


def _apply_pair(candidate: Candidate, pair: ProbePair, spec: ProbeSpec | None = None) -> Candidate:
    """Turn a classified pair into status + verification on a copy of the candidate."""
    updated = candidate.model_copy(deep=True)
    note = (
        f"dual-commit probe: base={pair.base.status.value} head={pair.head.status.value} "
        f"-> {pair.classification.value}"
    )
    previous_detail = updated.verification.detail
    merged_detail = f"{previous_detail}; {note}" if previous_detail else note
    updated.verification = updated.verification.model_copy(
        update={"probe": pair, "probe_spec": spec, "detail": merged_detail}
    )
    if pair.classification is ProbeClassification.FIX_CONFIRMED:
        # A probe that fails before the change and passes after it confirms the
        # fix the PR is making.  It is evidence for approving the change, not a
        # defect to request: emitting it would block every bug-fix PR.
        updated.status = FindingStatus.SUPPRESSED
        updated.suppression_reason = (
            "the reproduction fails on the base commit and passes on the head commit, "
            "so this change fixes the probed behaviour; nothing is requested"
        )
        updated.verification = Verification(
            kind=VerificationKind.PROBE,
            detail=f"{merged_detail}; fix confirmed",
            probe=pair,
            probe_spec=spec,
            counterfactual=updated.verification.counterfactual,
            civer=updated.verification.civer,
        )
        updated.evidence = updated.evidence.model_copy(update={"kind": "probe", "probe": pair})
        return updated
    if pair.classification is ProbeClassification.BOTH_PASS:
        updated.status = FindingStatus.SUPPRESSED
        updated.suppression_reason = (
            "the synthesised probe passes on the base commit and on the head commit, "
            "so the defect the candidate describes is not reproducible in this change"
        )
        return updated
    if pair.classification in (
        ProbeClassification.SETUP_ERROR,
        ProbeClassification.INCONCLUSIVE,
    ):
        updated.status = FindingStatus.UNVERIFIED
        return updated
    updated.status = FindingStatus.VERIFIED
    updated.verification = Verification(
        kind=VerificationKind.PROBE,
        detail=merged_detail,
        probe=pair,
        probe_spec=spec,
        counterfactual=updated.verification.counterfactual,
        civer=updated.verification.civer,
    )
    updated.evidence = updated.evidence.model_copy(update={"kind": "probe", "probe": pair})
    return updated


def _pair_degraded(pair: ProbePair) -> list[str]:
    if pair.classification is ProbeClassification.SETUP_ERROR:
        side = "base" if pair.base.status is ProbeStatus.SETUP_ERROR else "head"
        result = pair.base if side == "base" else pair.head
        return [
            "probe_setup_error: the generated probe could not run on the "
            f"{side} commit ({result.failure_kind or result.status.value}); discarded, never counted against the PR"
        ]
    if pair.classification is ProbeClassification.INCONCLUSIVE:
        return [
            "probe_inconclusive: "
            f"base={pair.base.status.value} head={pair.head.status.value}; no mechanical signal, "
            "the candidate stays unverified"
        ]
    return []


def run_cascade(
    candidate: Candidate,
    context: ContextSlice,
    llm,
    sandbox: Sandbox,
    repo: GitRepo,
    diff: DiffBundle,
    settings: Settings,
    *,
    healthz: Callable[[], Any] | None = None,
) -> tuple[list[Candidate], list[ProbePair], list[str]]:
    """Synthesise one probe and run it against the base and head commits."""
    pairs: list[ProbePair] = []
    degraded: list[str] = []
    if candidate.status is FindingStatus.VERIFIED and candidate.verification.kind is not VerificationKind.UNVERIFIED:
        return [candidate], pairs, degraded
    try:
        ok, detail = sandbox.available()
    except Exception as exc:  # a broken runtime must degrade, never crash the stage
        degraded.append(f"sandbox_unavailable: {type(exc).__name__}: {exc}")
        return [candidate], pairs, degraded
    if not ok:
        degraded.append(f"sandbox_unavailable: {detail}")
        return [candidate], pairs, degraded
    if not _alive(healthz):
        degraded.append("cascade stopped before synthesis: the deadline was exhausted")
        return [candidate], pairs, degraded
    expected_sha = ""
    timeout_s = float(settings.sandbox.timeout_s)
    base_tree: Path | None = None
    head_tree: Path | None = None
    try:
        base_tree = _worktree(repo, diff.base_sha, settings.paths.work_dir, "base")
        if not _alive(healthz):
            degraded.append("cascade stopped after the base worktree: the deadline was exhausted")
            return [candidate], pairs, degraded
        head_tree = _head_worktree(repo, diff, settings.paths.work_dir)

        def validate(spec: ProbeSpec) -> str:
            """Run the probe on the head tree; setup failures become feedback.

            This is the Reflexion signal: an import or fixture error is a string
            the generator can fix, while a behavioural failure on the head is
            exactly what a reproduction should produce.
            """
            result = sandbox.run_probe(
                head_tree,
                spec,
                expected_sha=sha256_hex(spec.content.encode("utf-8")),
                timeout_s=timeout_s,
            )
            if result.status is ProbeStatus.SETUP_ERROR:
                return (result.stderr or result.stdout or "the probe could not run")[-4000:]
            if result.status is ProbeStatus.MALFORMED_PROBE:
                return result.stderr or "the probe path is not usable"
            return ""

        spec, problems = synthesize(
            candidate,
            context,
            llm,
            settings,
            project_files=_project_files(repo, diff),
            validate=validate,
        )
        degraded.extend(f"probe_synthesis: {problem}" for problem in problems)
        if spec is None:
            degraded.append(
                "probe_synthesis: no probe survived validation; the candidate stays unverified and is left to Stage 4"
            )
            return [candidate], pairs, degraded
        expected_sha = sha256_hex(spec.content.encode("utf-8"))
        base_result = sandbox.run_probe(base_tree, spec, expected_sha=expected_sha, timeout_s=timeout_s)
        head_result = sandbox.run_probe(head_tree, spec, expected_sha=expected_sha, timeout_s=timeout_s)
    except ReviewError as exc:
        degraded.append(f"probe_pair: {exc}")
        return [candidate], pairs, degraded
    except OSError as exc:
        degraded.append(f"probe_pair: could not prepare a worktree: {exc}")
        return [candidate], pairs, degraded
    finally:
        _remove(repo, head_tree)
        _remove(repo, base_tree)

    pair = classify_pair(base_result, head_result)
    pairs.append(pair)
    degraded.extend(_pair_degraded(pair))
    return [_apply_pair(candidate, pair, spec)], pairs, degraded


# --------------------------------------------------------------------------- #
# Grounded-vote fallback
# --------------------------------------------------------------------------- #


def _fallback_detail(accepts: int, rejects: int, uncertain: int, k: int) -> str:
    return f"fallback votes k={k}: accept={accepts} reject={rejects} uncertain={uncertain}"


def fallback_verify(
    candidate: Candidate,
    context: ContextSlice,
    llm,
    *,
    settings: Settings | None = None,
    votes: int | None = None,
) -> Candidate:
    """Ask the grounded reviewer k times and fold the votes into a status.

    A model majority is evidence, not verification: ``accept`` only verifies a
    candidate that already carries a real perturbation, otherwise it stays
    ``UNVERIFIED`` with a degraded reason for the receipt.
    """
    k = FALLBACK_VOTES
    if settings is not None:
        k = int(getattr(settings.llm.monitor, "votes", FALLBACK_VOTES) or FALLBACK_VOTES)
    if votes is not None:
        k = int(votes)
    k = max(1, k)
    if candidate.status is FindingStatus.VERIFIED:
        return candidate

    prompt = grounded_review_prompt(candidate, context)
    accepts = rejects = uncertain = 0
    rationales: list[str] = []
    for _ in range(k):
        try:
            answer = llm.complete("fallback", prompt, FallbackVerdict, temperature=FALLBACK_TEMPERATURE)
            value = getattr(answer, "value", answer)
            verdict = value if isinstance(value, FallbackVerdict) else FallbackVerdict.model_validate(value)
        except Exception as exc:  # an abstaining vote is data, not a crash
            uncertain += 1
            rationales.append(f"abstain:{type(exc).__name__}")
            continue
        if verdict.verdict == "accept":
            accepts += 1
        elif verdict.verdict == "reject":
            rejects += 1
        else:
            uncertain += 1
        if verdict.rationale:
            rationales.append(verdict.rationale[:200])

    base_detail = _fallback_detail(accepts, rejects, uncertain, k)
    if rationales:
        base_detail = base_detail + "; " + " | ".join(rationales[:5])
    updated = candidate.model_copy(deep=True)

    if accepts > k / 2:
        if updated.verification.counterfactual is not None:
            updated.status = FindingStatus.VERIFIED
            updated.verification = Verification(
                kind=VerificationKind.PERTURBATION,
                detail=f"{base_detail}; a perturbation is recorded on this candidate",
                probe=updated.verification.probe,
                counterfactual=updated.verification.counterfactual,
                civer=updated.verification.civer,
            )
        else:
            updated.status = FindingStatus.UNVERIFIED
            updated.verification = updated.verification.model_copy(
                update={
                    "kind": VerificationKind.UNVERIFIED,
                    "detail": (
                        f"{base_detail}; degraded: fallback_accept_without_perturbation — "
                        "votes alone never verify a finding, so it stays unverified"
                    ),
                }
            )
        return updated
    if rejects > k / 2:
        updated.status = FindingStatus.SUPPRESSED
        updated.suppression_reason = (
            f"{base_detail}; a majority of grounded votes found the claimed path impossible or already guarded"
        )
        updated.verification = updated.verification.model_copy(
            update={"kind": VerificationKind.UNVERIFIED, "detail": base_detail}
        )
        return updated
    updated.status = FindingStatus.UNVERIFIED
    updated.verification = updated.verification.model_copy(
        update={
            "kind": VerificationKind.UNVERIFIED,
            "detail": f"{base_detail}; no majority: the reviewers were uncertain, so the candidate stays unverified",
        }
    )
    return updated
