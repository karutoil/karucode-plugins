"""Stage 1B — the exact-head gate and the branch-stack gate.

Two layers, deliberately separated:

* :func:`evaluate_gate` is pure.  It takes a :class:`GateSnapshot` the host
  already collected and decides, with no I/O, no clock and no network;
* :class:`HeadCommitGate` collects that snapshot from a Git provider
  (``pull`` / ``check_runs`` / ``approvals`` / ``bot_login`` and an optional
  ``repo`` for ancestry) and then calls the pure evaluator.

Rules enforced (each refusal reason is a human-readable string):

1. ``expected_head_sha`` must equal the pull request head.
2. The head must be an ancestor of every ``intermediate`` stacked branch; a
   stacked branch left behind at an older SHA is reported in
   ``stale_branches`` and refuses the gate.
3. Every required check must have run on the *exact* head SHA and every run of
   that name must have concluded ``SUCCESS``.  A green check on an older commit
   is a missing check.
4. An approval only counts when it targets the exact head SHA, is not
   dismissed and is not marked stale; everything else is reported in
   ``discounted_approvals``.  Requiring at least one such approval is opt-in
   (``gate.require_approval``): a reviewer bot normally runs before a human has
   approved anything, so the default records approvals without refusing on
   their absence.
5. The bot may not approve its own pull request: ``actor_login == bot_login``
   is refused, and a review authored by the bot login never counts.

A refusal is a *decision*, not an exception: the pipeline records it and
degrades to human review.  ``GateRefused`` exists for callers that cannot
continue without a passing gate.
"""

from __future__ import annotations

from typing import Iterable, Protocol, Sequence, runtime_checkable

from ..config import GateConfig
from ..gitops import GitError
from ..models import (
    ApprovalV1,
    CheckConclusion,
    CheckRunV1,
    GateDecision,
    GateResult,
    GateSnapshot,
    StackedBranch,
)


@runtime_checkable
class GateProvider(Protocol):
    """The slice of :class:`..providers.base.GitProvider` the gate needs."""

    def pull(self, repository: str, number: int): ...

    def check_runs(self, repository: str, sha: str) -> list[CheckRunV1]: ...

    def approvals(self, repository: str, number: int) -> list[ApprovalV1]: ...

    def bot_login(self) -> str: ...


# --------------------------------------------------------------------------- #
# Pure evaluation
# --------------------------------------------------------------------------- #


def _short(sha: str) -> str:
    return (sha or "")[:12] or "<unknown>"


def _unique(values: Iterable[str]) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    for value in values:
        if value and value not in seen:
            seen.add(value)
            out.append(value)
    return out


def _required_checks(snapshot: GateSnapshot, config: GateConfig) -> list[str]:
    """Configuration first, then anything the provider's branch protection added."""
    return _unique([*config.required_checks, *snapshot.required_checks])


def _discount_reason(approval: ApprovalV1, head_sha: str, bot_login: str) -> str:
    """Why one review does not count, or ``""`` when it does."""
    if approval.state != "approved":
        return f"state is {approval.state!r}"
    if approval.dismissed:
        return "dismissed"
    if approval.stale:
        return "stale"
    if bot_login and approval.reviewer == bot_login:
        return "self-approval by the review bot"
    if not approval.sha:
        return "no commit SHA recorded"
    if approval.sha != head_sha:
        return f"approved {_short(approval.sha)}, head is {_short(head_sha)}"
    return ""


def evaluate_gate(snapshot: GateSnapshot, config: GateConfig) -> GateResult:
    """Decide whether the head commit may be reviewed.  Pure, no I/O."""
    reasons: list[str] = []
    missing_checks: list[str] = []
    failing_checks: list[str] = []
    stale_branches: list[str] = []
    discounted: list[str] = []
    refused = False

    head_sha = (snapshot.head_sha or "").strip()
    expected_sha = (snapshot.expected_head_sha or "").strip()
    bot_login = (snapshot.bot_login or config.bot_login or "").strip()
    actor_login = (snapshot.actor_login or "").strip()

    if not head_sha:
        refused = True
        reasons.append("refused: the pull request reported no head SHA")

    if expected_sha and expected_sha != head_sha:
        refused = True
        reasons.append(
            f"refused: stale head — the event expected {_short(expected_sha)} "
            f"but the pull request head is {_short(head_sha)}; a newer push was reviewed instead"
        )

    # -- branch stack ------------------------------------------------------
    intermediates = [entry for entry in snapshot.stacked if entry.role == "intermediate"]
    branchless = [entry for entry in intermediates if not entry.sha]
    if branchless:
        refused = True
        stale_branches.extend(entry.branch for entry in branchless)
        reasons.append(
            "refused: stacked branch(es) carry no SHA and cannot be verified as descendants of "
            + ", ".join(entry.branch for entry in branchless)
        )
    if not snapshot.head_is_ancestor_of_branch:
        refused = True
        stale = [entry.branch for entry in intermediates if entry.sha != head_sha] or [
            entry.branch for entry in intermediates
        ]
        if not stale:
            stale = [head_sha]
        stale_branches.extend(stale)
        reasons.append(
            f"refused: head {_short(head_sha)} is not an ancestor of the stacked branch(es) "
            + ", ".join(stale)
            + "; the stack was rebased past this commit or is stale"
        )
    if intermediates and not config.allow_stacked_branches:
        refused = True
        reasons.append(
            "refused: stacked branches are disabled by configuration but "
            + ", ".join(entry.branch for entry in intermediates)
            + " sit on top of this pull request"
        )

    # -- required checks ---------------------------------------------------
    required = _required_checks(snapshot, config)
    for name in required:
        on_head = [run for run in snapshot.checks if run.name == name and run.sha == head_sha]
        if not on_head:
            missing_checks.append(name)
            refused = True
            reasons.append(
                f"refused: required check {name!r} did not report on head {_short(head_sha)}"
            )
            continue
        good = [
            run
            for run in on_head
            if run.status == "completed" and run.conclusion is CheckConclusion.SUCCESS
        ]
        if len(good) != len(on_head):
            failing_checks.append(name)
            refused = True
            worst = ", ".join(
                sorted(
                    f"{run.status}/{run.conclusion.value}"
                    for run in on_head
                )
            )
            reasons.append(
                f"refused: required check {name!r} did not succeed on head "
                f"{_short(head_sha)} ({worst})"
            )

    # -- required checks on every intermediate stacked branch --------------
    for entry in intermediates:
        if not entry.sha or entry.sha == head_sha:
            continue
        for name in required:
            on_branch = [run for run in snapshot.checks if run.name == name and run.sha == entry.sha]
            if not on_branch:
                refused = True
                reasons.append(
                    f"refused: required check {name!r} did not report on stacked branch "
                    f"{entry.branch} at {_short(entry.sha)}"
                )
                continue
            good = [
                run
                for run in on_branch
                if run.status == "completed" and run.conclusion is CheckConclusion.SUCCESS
            ]
            if len(good) != len(on_branch):
                refused = True
                reasons.append(
                    f"refused: required check {name!r} did not succeed on stacked branch "
                    f"{entry.branch} at {_short(entry.sha)}"
                )

    # -- approvals ---------------------------------------------------------
    effective: list[ApprovalV1] = []
    for approval in snapshot.approvals:
        reason = _discount_reason(approval, head_sha, bot_login)
        if reason:
            if approval.state == "approved":
                who = approval.reviewer or "<anonymous>"
                discounted.append(f"{who}@{_short(approval.sha) or '<no sha>'}: {reason}")
            continue
        effective.append(approval)
    if config.require_approval and not effective:
        refused = True
        if snapshot.approvals:
            reasons.append(
                "refused: no approval covers head "
                f"{_short(head_sha)}; every recorded approval was discounted ("
                + "; ".join(discounted)
                + ")"
            )
        else:
            reasons.append(
                f"refused: no approval by a non-bot reviewer covers head {_short(head_sha)}"
            )

    if actor_login and bot_login and actor_login == bot_login:
        refused = True
        reasons.append(
            f"refused: self-approval — the actor {actor_login!r} is the review bot itself; "
            "a bot may not approve its own pull request"
        )

    decision = GateDecision.REFUSE if refused else GateDecision.ALLOW
    effective_head = head_sha if head_sha and (not expected_sha or expected_sha == head_sha) else ""
    if decision is GateDecision.ALLOW:
        reasons.append(
            f"allowed: head {_short(head_sha)} verified against "
            f"{len(effective)} approval(s) and {len(required)} required check(s)"
        )
    return GateResult(
        decision=decision,
        reasons=reasons,
        effective_head=effective_head,
        discounted_approvals=discounted,
        missing_checks=_unique(missing_checks),
        failing_checks=_unique(failing_checks),
        stale_branches=_unique(stale_branches),
    )


# --------------------------------------------------------------------------- #
# Collection from a provider
# --------------------------------------------------------------------------- #


class HeadCommitGate:
    """Collect a snapshot from a provider, then evaluate it purely."""

    def __init__(
        self,
        provider: GateProvider,
        config: GateConfig,
        *,
        stacked: Sequence[StackedBranch] | None = None,
    ):
        self.provider = provider
        self.config = config
        self.stacked: list[StackedBranch] = list(stacked or ())
        #: Filled by :meth:`collect`; diagnostic notes for the receipts.
        self.last_notes: list[str] = []

    # -- collection --------------------------------------------------------
    def _bot_login(self) -> str:
        try:
            login = self.provider.bot_login()
        except Exception:  # pragma: no cover - provider-specific failure
            login = ""
        return str(login or self.config.bot_login or "")

    def _stack(self, head_sha: str) -> tuple[list[StackedBranch], bool, list[str]]:
        """Resolve every stacked branch to its current SHA and check ancestry.

        Returns ``(entries, head_is_ancestor_of_branch, notes)``.  Without a
        repository object only the declared SHAs can be checked; the notes say
        so instead of pretending the ancestry was verified.
        """
        repo = getattr(self.provider, "repo", None)
        entries: list[StackedBranch] = []
        notes: list[str] = []
        ancestor = True
        for declared in self.stacked:
            sha = declared.sha
            if repo is not None:
                try:
                    sha = repo.resolve(declared.branch) or declared.sha
                except GitError as exc:
                    notes.append(f"{declared.branch}: cannot resolve branch ({exc.message})")
                    sha = declared.sha
            entries.append(StackedBranch(branch=declared.branch, sha=sha, role=declared.role))
            if declared.role != "intermediate":
                continue
            if not sha:
                ancestor = False
                continue
            if repo is None:
                notes.append(
                    f"{declared.branch}: no repository object; ancestry of head {_short(head_sha)} "
                    "was not verified"
                )
                continue
            try:
                if not repo.is_ancestor(head_sha, sha):
                    ancestor = False
            except GitError as exc:
                notes.append(f"{declared.branch}: cannot evaluate ancestry ({exc.message})")
                ancestor = False
        return entries, ancestor, notes

    def collect(
        self,
        repository: str,
        number: int,
        *,
        expected_head_sha: str = "",
        actor_login: str = "",
    ) -> GateSnapshot:
        pull = self.provider.pull(repository, number)
        head_sha = str(getattr(pull, "head_sha", "") or "")
        base_sha = str(getattr(pull, "base_sha", "") or "")
        checks: list[CheckRunV1] = list(self.provider.check_runs(repository, head_sha)) if head_sha else []
        approvals: list[ApprovalV1] = list(self.provider.approvals(repository, number))
        stacked, ancestor, notes = self._stack(head_sha)
        seen_check_shas = {head_sha} if head_sha else set()
        for entry in stacked:
            if entry.role != "intermediate" or not entry.sha or entry.sha in seen_check_shas:
                continue
            seen_check_shas.add(entry.sha)
            try:
                checks.extend(self.provider.check_runs(repository, entry.sha))
            except Exception as exc:
                notes.append(f"{entry.branch}: check runs unavailable ({type(exc).__name__})")
        snapshot = GateSnapshot(
            repository=repository,
            pull_number=number,
            base_sha=base_sha,
            head_sha=head_sha,
            expected_head_sha=expected_head_sha,
            head_is_ancestor_of_branch=ancestor,
            checks=checks,
            approvals=approvals,
            stacked=stacked,
            actor_login=actor_login,
            bot_login=self._bot_login(),
            required_checks=list(self.config.required_checks),
        )
        # Collection notes are diagnostic only; they never turn into a silent
        # "no findings" — ``head_is_ancestor_of_branch`` already refused when
        # the ancestry could not be established.
        self.last_notes = notes
        return snapshot

    # -- evaluation --------------------------------------------------------
    def evaluate(
        self,
        repository: str,
        number: int,
        *,
        expected_head_sha: str = "",
        actor_login: str = "",
    ) -> GateResult:
        snapshot = self.collect(
            repository, number, expected_head_sha=expected_head_sha, actor_login=actor_login
        )
        return evaluate_gate(snapshot, self.config)
