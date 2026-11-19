"""The five-stage review pipeline.

Ordering is fixed; every stage records a receipt and every unresolved state
falls through to a human rather than to an approval.  The pipeline itself does
no posting: it returns a :class:`ReviewVerdict`, which the CLI/CI layer renders
and posts exactly once.
"""

from __future__ import annotations

import concurrent.futures
import re
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

from .adapters import BUILTIN_ADAPTERS, ProbeAdapter, adapter_for
from .config import Settings, StageModel, ensure_state_dirs
from .errors import ContractViolation, LLMError, ReviewError
from .gitops import GitError, GitRepo
from .llm.client import JSONLLM, NullLLM, OpenAICompatibleClient, build_llm
from .llm.prompts import behavioral_prompt, integration_prompt, triage_prompt
from .models import (
    AnalyzerFinding,
    AttemptStatus,
    BehavioralCandidates,
    Candidate,
    Category,
    CitedSite,
    ContextSlice,
    Decision,
    DiffBundle,
    Evidence,
    FilePatch,
    Finding,
    FindingStatus,
    FusionResult,
    GateDecision,
    GateResult,
    MciResult,
    MonitorScore,
    ProbePair,
    ProbeStatus,
    RaleResult,
    Receipt,
    RiskElicitation,
    ReviewVerdict,
    StageId,
    StackedBranch,
    StageReport,
    TriagePlan,
    VerificationKind,
    canonical_json,
    sha256_hex,
)
from .providers.base import GitProvider
from .stage1 import context as context_mod
from .stage1 import pmi as pmi_mod
from .stage1.contract import ContractReport, enforce_contract, run_bounded
from .stage1.changegraph import ChangePlan, build_change_plan
from .stage1.gatekeeper import HeadCommitGate
from .stage1.receipts import Clock, ReceiptLog
from .stage2 import cpg as cpg_mod
from .stage2.executor import Stage2Outcome, run_stage2
from .stage3 import cascade as cascade_mod
from .stage3.sandbox import Sandbox, build_sandbox
from .stage4 import civer as civer_mod
from .stage4 import designlore as designlore_mod
from .stage4 import oblivul as oblivul_mod
from .stage5 import elicit as elicit_mod
from .stage5 import formatter as formatter_mod
from .stage5 import fusion as fusion_mod

#: Behavioural LLM calls are bounded so a thousand-line PR is still reviewed in
#: file-sized batches instead of silently looking at the first file.
MAX_BEHAVIOR_BATCHES = 24
EXHAUSTIVE_BEHAVIOR_BATCHES = 100
MAX_BEHAVIOR_FILES_PER_BATCH = 4
MAX_BEHAVIOR_BATCH_CHARS = 30_000
MAX_BEHAVIOR_CANDIDATES = 24
EXHAUSTIVE_BEHAVIOR_CANDIDATES = 100
BEHAVIOR_WORKERS = 4

#: Stage 3/4 are expensive and model-backed; the top candidates get the budget,
#: the rest stay unverified and therefore cannot be published silently.
MAX_PROBED_CANDIDATES = 32
MAX_CIVER_CANDIDATES = 32
from .stage5 import redaction as redaction_mod


def _now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()) + f".{int(time.time() * 1000) % 1000:03d}Z"


def _short(sha: str, n: int = 12) -> str:
    return (sha or "")[:n]


def _candidate_id(stage: str, path: str, line: int, title: str) -> str:
    return f"{stage}-{sha256_hex(f'{path}:{line}:{title}')[:10]}"


def _severity(value: int) -> int:
    return max(0, min(3, int(value)))


@dataclass(frozen=True)
class ReviewRequest:
    workspace: Path
    base_ref: str = ""
    head_ref: str = "HEAD"
    expected_head_sha: str = ""
    repository: str = ""
    pull_number: int = 0
    description: str = ""
    author: str = ""
    actor_login: str = ""
    stacked: tuple[StackedBranch, ...] = ()
    provider: GitProvider | None = None


@dataclass
class Budget:
    total_s: float
    started: float = field(default_factory=time.monotonic)

    def elapsed(self) -> float:
        return time.monotonic() - self.started

    def remaining(self) -> float:
        return max(0.0, self.total_s - self.elapsed())

    def expired(self) -> bool:
        return self.remaining() <= 1.0

    def grant(self, wanted: float) -> float:
        """Never hand a stage more time than the review has left."""
        return max(0.0, min(float(wanted), self.remaining()))


@dataclass
class _Run:
    degraded: list[str] = field(default_factory=list)
    receipts: list[Receipt] = field(default_factory=list)
    reports: list[StageReport] = field(default_factory=list)
    contract: ContractReport | None = None
    gate: GateResult | None = None
    mci: MciResult | None = None
    diff: DiffBundle | None = None
    context: ContextSlice | None = None
    stage2: Stage2Outcome | None = None
    change_plan: ChangePlan | None = None
    coverage_total: int = 0
    coverage_covered: int = 0


class Reviewer:
    def __init__(
        self,
        settings: Settings,
        *,
        environ: Mapping[str, str] | None = None,
        llm: JSONLLM | None = None,
        monitor_llm: JSONLLM | None = None,
        sandbox: Sandbox | None = None,
        receipt_root: Path | None = None,
        runner: civer_mod.CommandRunner | None = None,
    ):
        self.settings = settings
        self.environ = dict(environ or {})
        ensure_state_dirs(settings)
        self._receipt_root = Path(receipt_root or settings.paths.receipts_dir)
        self._log: ReceiptLog | None = None
        self._receipts: list[Receipt] = []
        self._provider = None
        self.llm = llm if llm is not None else build_llm(settings, self.environ, receipt_sink=self._llm_receipt)
        self.monitor_llm = monitor_llm if monitor_llm is not None else self._build_monitor()
        self._sandbox = sandbox
        self.runner = runner or civer_mod.LocalCommandRunner()
        self._clock = Clock()
        self._rules_dir = Path(__file__).resolve().parents[1] / "rules"

    # -- clients ----------------------------------------------------------
    def _cap_llm(self, budget: Budget) -> None:
        """Hand every model client the time the review has left."""
        for client in (self.llm, self.monitor_llm):
            cap = getattr(client, "cap_to", None)
            if callable(cap):
                cap(budget.remaining())

    def _build_monitor(self) -> JSONLLM:
        alt = self.settings.llm.monitor_alt
        llm_settings = self.settings
        if alt is not None:
            monitor_base = self.environ.get("ZERO_LOOP_MONITOR_BASE_URL", "").strip()
            llm_settings = replace(
                self.settings,
                llm=replace(
                    self.settings.llm,
                    monitor=StageModel(
                        model=alt.model,
                        temperature=alt.temperature,
                        max_tokens=alt.max_tokens,
                        votes=alt.votes or self.settings.llm.monitor.votes,
                    ),
                    base_url=monitor_base or self.settings.llm.base_url,
                    api_key_env=(
                        "ZERO_LOOP_MONITOR_API_KEY"
                        if monitor_base or "ZERO_LOOP_MONITOR_API_KEY" in self.environ
                        else self.settings.llm.api_key_env
                    ),
                ),
            )
        client = OpenAICompatibleClient(llm_settings, environ=self.environ, receipt_sink=self._llm_receipt)
        return client if client.configured else NullLLM()

    @property
    def sandbox(self) -> Sandbox:
        if self._sandbox is None:
            self._sandbox = build_sandbox(self.settings)
        return self._sandbox

    def _llm_receipt(self, payload: dict[str, Any]) -> None:
        if self._log is None:
            return
        status = payload.get("status", AttemptStatus.COMPLETED)
        note = self._log.record(
            stage=str(payload.get("stage", "llm")),
            status=status if isinstance(status, AttemptStatus) else AttemptStatus.MALFORMED_OUTPUT,
            started_at=str(payload.get("started_at", _now())),
            finished_at=str(payload.get("finished_at", _now())),
            model=str(payload.get("model", "")),
            input_sha256=str(payload.get("input_sha256", "")),
            input_bytes=int(payload.get("input_bytes", 0)),
            output_sha256=str(payload.get("output_sha256", "")),
            output_bytes=int(payload.get("output_bytes", 0)),
            tokens_in=int(payload.get("tokens_in", 0)),
            tokens_out=int(payload.get("tokens_out", 0)),
            detail=str(payload.get("detail", "")),
        )
        self._receipts.append(note)

    def _record(
        self,
        stage: str,
        status: AttemptStatus,
        started: str,
        detail: str = "",
        *,
        exit_code: int | None = None,
        model: str = "",
        input_sha256: str = "",
        input_bytes: int = 0,
        output_sha256: str = "",
        output_bytes: int = 0,
        cancellation_point: str = "",
    ) -> Receipt | None:
        if self._log is None:
            return None
        receipt = self._log.record(
            stage=stage,
            status=status,
            started_at=started,
            finished_at=_now(),
            exit_code=exit_code,
            model=model,
            input_sha256=input_sha256,
            input_bytes=input_bytes,
            output_sha256=output_sha256,
            output_bytes=output_bytes,
            cancellation_point=cancellation_point,
            detail=detail,
        )
        self._receipts.append(receipt)
        return receipt

    # ------------------------------------------------------------------ #
    # Entry point
    # ------------------------------------------------------------------ #
    def review(self, request: ReviewRequest) -> ReviewVerdict:
        budget = Budget(self.settings.deadline_s)
        run = _Run()
        repo = GitRepo(request.workspace)
        started = _now()
        review_id = f"{_short(request.expected_head_sha or 'head')}-{int(time.time())}"
        try:
            head_sha = repo.resolve(request.head_ref or "HEAD")
        except GitError as exc:
            return self._verdict(review_id, "", "", Decision.HUMAN_REVIEW, 0.0, [], run, ["invalid_repository: " + str(exc)])
        review_id = f"{_short(head_sha)}-{int(time.time())}"
        self._log = ReceiptLog(self._receipt_root, review_id)
        self._receipts = []
        self._record("1.contract", AttemptStatus.COMPLETED, started, "review started")
        try:
            return self._review(request, repo, head_sha, budget, run, review_id)
        except ContractViolation as exc:
            run.degraded.append(f"contract_violation: {exc.message}")
            return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)
        except Exception as exc:  # a reviewer bug must never become an approval
            self._record(
                "pipeline",
                AttemptStatus.PROCESS_CRASH,
                _now(),
                f"{type(exc).__name__}: {exc}",
            )
            run.degraded.append(f"pipeline_crash: {type(exc).__name__}: {exc}")
            return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)

    # ------------------------------------------------------------------ #
    def _review(
        self,
        request: ReviewRequest,
        repo: GitRepo,
        head_sha: str,
        budget: Budget,
        run: _Run,
        review_id: str,
    ) -> ReviewVerdict:
        started = _now()
        try:
            run.contract = enforce_contract(self.settings, environ=self.environ)
            run.degraded.extend(run.contract.degraded)
        except ContractViolation as exc:
            run.degraded.append(f"contract_refused: {exc.message}")
            return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)
        self._cap_llm(budget)

        if request.expected_head_sha and request.expected_head_sha != head_sha:
            run.degraded.append(
                f"stale_head: event expected {_short(request.expected_head_sha)} but HEAD is {_short(head_sha)}"
            )
            return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)

        provider = request.provider
        if provider is not None and getattr(provider, "repo", None) is None:
            # The gate can verify stacked-branch ancestry only with a local
            # repository; attach one when the provider has no checkout handle.
            try:
                provider.repo = GitRepo(request.workspace)
            except Exception:
                pass
        pull = None
        if provider is not None and request.repository and request.pull_number:
            try:
                pull = provider.pull(request.repository, request.pull_number)
            except Exception as exc:
                run.degraded.append(f"provider_pull_failed: {type(exc).__name__}: {exc}")
            if pull is not None and pull.head_sha and pull.head_sha != head_sha:
                run.degraded.append(
                    f"stale_head: the provider reports {_short(pull.head_sha)}, local HEAD is {_short(head_sha)}"
                )
                return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)
            if pull is not None and not request.description and pull.description:
                request = replace(request, description=pull.description)
            if pull is not None and pull.draft:
                run.degraded.append("draft_pr: the pull request is a draft")

        if provider is not None and request.repository and request.pull_number and run.contract is not None:
            try:
                gate = HeadCommitGate(
                    provider, self.settings.gate, stacked=list(request.stacked)
                ).evaluate(
                    request.repository,
                    request.pull_number,
                    expected_head_sha=request.expected_head_sha or head_sha,
                    actor_login=request.actor_login,
                )
                run.gate = gate
                if gate.decision is GateDecision.REFUSE:
                    run.degraded.append("gate_refused: " + "; ".join(gate.reasons))
                    return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)
            except Exception as exc:
                run.degraded.append(f"gate_unavailable: {type(exc).__name__}: {exc}")
        self._record("1.gate", AttemptStatus.COMPLETED, started, f"head {_short(head_sha)}")

        base_ref = request.base_ref or (pull.base_sha if pull is not None else "")
        if not base_ref:
            run.degraded.append("no_base_ref: a review needs an explicit base commit or a provider pull request")
            return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)
        try:
            diff = repo.diff(base_ref, head_sha)
        except GitError as exc:
            run.degraded.append(f"diff_failed: {exc}")
            return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)
        run.diff = diff
        if self.settings.decomposition:
            try:
                run.change_plan = build_change_plan(repo, diff)
                self._record(
                    "1.change_graph",
                    AttemptStatus.COMPLETED,
                    _now(),
                    f"{run.change_plan.summary()['units']} unit(s), "
                    f"{run.change_plan.summary()['edges']} cross-unit edge(s)",
                )
            except Exception as exc:
                run.degraded.append(f"change_graph_degraded: {type(exc).__name__}: {exc}")

        # The description is author text: it is converted into structured,
        # host-grounded claims here and only those claims enter the other
        # prompts.  Raw author text never reaches a non-PMI prompt.
        if request.description.strip():
            started = _now()
            try:
                facts = pmi_mod.ast_facts(repo, diff)
                run.mci = pmi_mod.check_consistency(request.description, facts, self.llm, settings=self.settings)
                if run.mci.contradiction:
                    run.degraded.append(
                        "pr_message_inconsistency: the description claims behaviour the diff does not implement"
                    )
                self._record("1.pmi", AttemptStatus.COMPLETED, started, run.mci.summary or "claims checked")
            except LLMError as exc:
                run.degraded.append(f"pmi_degraded: {exc.kind.value}: {exc.message}")
            except Exception as exc:
                run.degraded.append(f"pmi_degraded: {type(exc).__name__}: {exc}")


        # ---- Stage 1: context, triage, PR-MCI -------------------------------
        started = _now()
        symbols = []
        try:
            symbols = context_mod.extract_symbols(repo, diff, head=diff.head_sha)
        except Exception as exc:
            run.degraded.append(f"symbol_extraction: {type(exc).__name__}: {exc}")
        plan: TriagePlan | None = None
        if not budget.expired():
            try:
                prompt = triage_prompt(
                    diff,
                    changed_symbols=[symbol.name for symbol in symbols],
                    critical_tokens=context_mod.critical_tokens(diff),
                    claims=run.mci.claims if run.mci else (),
                )
                answer = self.llm.complete("triage", prompt, TriagePlan, stage_model=self.settings.llm.triage)
                plan = answer.value
            except LLMError as exc:
                run.degraded.append(f"triage_degraded: {exc.kind.value}: {exc.message}")
            except Exception as exc:
                run.degraded.append(f"triage_degraded: {type(exc).__name__}: {exc}")
        try:
            run.context = context_mod.build_context(repo, diff, self.settings, plan=plan)
        except Exception as exc:
            run.degraded.append(f"context_failed: {type(exc).__name__}: {exc}")
            return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)
        self._record(
            "1.context",
            AttemptStatus.COMPLETED,
            started,
            f"{run.context.tokens} tokens of {run.context.budget}, {len(symbols)} symbols",
            input_sha256=run.context.input_sha256,
            input_bytes=run.context.input_bytes,
        )
        if run.context.material_truncated:
            run.degraded.append(
                "context_truncated: the token budget could not hold every changed file and its 1-hop context"
            )
        if diff.truncated:
            run.degraded.append("diff_truncated: only the first 8 MB of the patch was read")

        if not diff.files:
            run.degraded.append("empty_diff: there is no code change to review")
            return self._finish(review_id, request, repo, head_sha, run, budget)

        # ---- Stage 2 --------------------------------------------------------
        started = _now()
        self._cap_llm(budget)
        try:
            run.stage2 = run_stage2(repo.root, repo, diff, run.context, self.llm, self.settings, environ=self.environ)
            run.degraded.extend(run.stage2.degraded)
            self._record(
                "2.ensemble",
                AttemptStatus.COMPLETED,
                started,
                f"{len(run.stage2.candidates)} candidates, {len(run.stage2.findings)} analyzer findings, "
                f"{len(run.stage2.rale)} RALE views in {run.stage2.elapsed_ms} ms",
                input_sha256=run.context.input_sha256,
                input_bytes=run.context.input_bytes,
            )
        except Exception as exc:
            run.degraded.append(f"stage2_failed: {type(exc).__name__}: {exc}")
            self._record("2.ensemble", AttemptStatus.PROCESS_CRASH, started, str(exc))
            return self._finish(review_id, request, repo, head_sha, run, budget, force_human=True)

        candidates = dedupe_candidates(
            [
                *run.stage2.candidates,
                *_analyzer_candidates(run.stage2.findings),
                *_rale_candidates(run.stage2.rale),
                *self._behavioral_candidates(diff, run.context, plan, budget, run, run.stage2.findings),
            ]
        )

        # ---- Stage 3: probes ------------------------------------------------
        candidates, pairs = self._stage3(candidates, run, repo, diff, budget)
        run.reports.append(
            StageReport(
                stage=StageId.PROBE,
                status=AttemptStatus.COMPLETED if pairs else AttemptStatus.SKIPPED,
                candidates=candidates,
                degraded=[m for m in run.degraded if m.startswith(("probe_", "sandbox_", "cascade_", "fallback_"))],
                detail=f"{len(pairs)} dual-commit probe pair(s)",
            )
        )

        # ---- Stage 4: falsification -----------------------------------------
        candidates = self._stage4(candidates, run, repo, diff, budget)

        # ---- Stage 5: elicitation, monitor, decision ------------------------
        blind_spots = [
            entry
            for entry in run.degraded
            if entry.startswith(
                (
                    "2a ",
                    "2b ",
                    "2c ",
                    "stage2_failed",
                    "stage 2 ",
                    "behavioral_scan_degraded",
                    "behavioral_coverage_incomplete",
                    "behavioral_candidate_coverage_incomplete",
                    "behavioral_hunk_coverage_incomplete",
                    "integration_scan_degraded",
                    "probe_coverage_incomplete",
                    "civer_coverage_incomplete",
                    "context_truncated",
                    "diff_truncated",
                )
            )
        ]
        return self._finish(
            review_id,
            request,
            repo,
            head_sha,
            run,
            budget,
            candidates=candidates,
            blind_spots=blind_spots,
        )

    # ------------------------------------------------------------------ #
    def _behavioral_candidates(
        self,
        diff: DiffBundle,
        context: ContextSlice,
        plan: TriagePlan | None,
        budget: Budget,
        run: _Run,
        analyzer_findings: Sequence[AnalyzerFinding] = (),
    ) -> list[Candidate]:
        """Ask for grounded behavioural hypotheses; keep only diff-anchored ones.

        This is candidate generation, not a verdict: nothing here can approve or
        reject.  A draft whose cited site is outside the changed lines is
        discarded, which is what stops a hallucinated path from becoming a
        finding.  Large PRs are split into file-sized batches: a single 16k
        evidence block only ever sees the first files, and approving the rest
        unseen is exactly the failure mode this stage must not have.
        """
        if budget.remaining() < 20.0:
            return []
        batches, capped_batches, oversized = _behavioral_batches(
            diff,
            adapters=self.settings.probe_adapters,
            max_batches=(
                EXHAUSTIVE_BEHAVIOR_BATCHES
                if self.settings.exhaustive
                else MAX_BEHAVIOR_BATCHES
            ),
            plan=run.change_plan,
        )
        if not batches:
            return []
        max_candidates = (
            EXHAUSTIVE_BEHAVIOR_CANDIDATES
            if self.settings.exhaustive
            else MAX_BEHAVIOR_CANDIDATES
        )
        started = _now()
        kept: list[Candidate] = []
        seen: set[str] = set()
        rejected = 0
        over_cap = 0
        failed = 0
        # A huge PR can spend an hour, but a 4-minute review must not start 17
        # five-minute calls.  Batches run four at a time, so budget in waves:
        # two minutes per wave, four batches per wave.  The rest are a declared
        # blind spot, not a silent omission.
        workers = min(BEHAVIOR_WORKERS, len(batches))
        waves = max(1, int(budget.remaining() / 120))
        allowed = min(len(batches), waves * workers)
        if allowed < len(batches):
            capped_batches = True
            batches = batches[:allowed]
        self._cap_llm(budget)

        def run_batch(index: int, batch: list[FilePatch]) -> tuple[int, list[Any]]:
            batch_diff = diff.model_copy(update={"files": batch})
            batch_context = _context_for_batch(context, {patch.path for patch in batch})
            unit = run.change_plan.unit_for(batch[0].path) if run.change_plan is not None else None
            unit_note = ""
            if unit is not None:
                unit_note = (
                    f"{unit.title}\n"
                    f"unit id: {unit.id}\n"
                    f"files: {', '.join(unit.paths)}\n"
                    f"cross-unit edges: {', '.join(unit.related) or 'none'}"
                )
            answer = self.llm.complete(
                "behavioral",
                behavioral_prompt(
                    batch_diff,
                    batch_context,
                    plan=plan,
                    unit_note=unit_note,
                    claims=run.mci.claims if run.mci else (),
                ),
                BehavioralCandidates,
                stage_model=self.settings.llm.reviewer,
            )
            return index, list(answer.value.candidates)

        def accept(drafts: Sequence[Any], scope: DiffBundle) -> None:
            nonlocal rejected, over_cap
            for draft in drafts:
                sites = [
                    site
                    for site in draft.cited_sites
                    if _site_in_diff(scope, site) and _groundable(site, analyzer_findings, self.settings.probe_adapters)
                ]
                if not sites:
                    rejected += 1
                    continue
                if len(kept) >= max_candidates:
                    over_cap += 1
                    continue
                trigger = f" Trigger: {draft.suspected_trigger}" if draft.suspected_trigger else ""
                candidate = Candidate(
                    id=_candidate_id("3", sites[0].path, sites[0].line, draft.title),
                    stage=StageId.PROBE,
                    category=draft.category,
                    title=draft.title,
                    root_cause=(draft.root_cause + trigger)[:4000],
                    severity=_severity(draft.severity),
                    cited_sites=sites,
                    evidence=Evidence(
                        kind="llm_hypothesis",
                        analyzer="behavioral-scan",
                        trace=sites,
                        detail="a grounded hypothesis; it becomes a finding only after mechanical verification",
                    ),
                )
                if candidate.id in seen:
                    continue
                seen.add(candidate.id)
                kept.append(candidate)

        with ThreadPoolExecutor(max_workers=workers) as pool:
            futures = {
                pool.submit(run_batch, index, batch): (index, batch)
                for index, batch in enumerate(batches, 1)
            }
            for future in as_completed(futures):
                index, batch = futures[future]
                batch_diff = diff.model_copy(update={"files": batch})
                try:
                    _, drafts = future.result()
                except LLMError as exc:
                    failed += 1
                    run.degraded.append(
                        f"behavioral_scan_degraded: batch {index}/{len(batches)}: {exc.kind.value}: {exc.message}"
                    )
                    self._record("2.behavioral.batch", exc.kind, started, f"batch {index}: {exc.message}")
                    continue
                except Exception as exc:
                    failed += 1
                    run.degraded.append(
                        f"behavioral_scan_degraded: batch {index}/{len(batches)}: {type(exc).__name__}: {exc}"
                    )
                    self._record(
                        "2.behavioral.batch", AttemptStatus.PROCESS_CRASH, started, f"batch {index}: {exc}"
                    )
                    continue
                accept(drafts, batch_diff)
                self._record(
                    "2.behavioral.batch",
                    AttemptStatus.COMPLETED,
                    started,
                    f"batch {index}/{len(batches)}: {len(drafts)} draft(s), {len(kept)} kept so far",
                )
        if (
            run.change_plan is not None
            and len(run.change_plan.units) > 1
            and run.change_plan.edges
            and budget.remaining() > 30.0
        ):
            self._cap_llm(budget)
            notes = [
                f"{unit.id} | {unit.title} | files: {', '.join(unit.paths)}"
                for unit in run.change_plan.units
            ]
            try:
                answer = self.llm.complete(
                    "integration",
                    integration_prompt(
                        diff,
                        context,
                        edges=run.change_plan.edges,
                        unit_notes=notes,
                        claims=run.mci.claims if run.mci else (),
                    ),
                    BehavioralCandidates,
                    stage_model=self.settings.llm.reviewer,
                )
                integration_drafts = list(answer.value.candidates)
                accept(integration_drafts, diff)
                self._record(
                    "2.integration",
                    AttemptStatus.COMPLETED,
                    started,
                    f"{len(integration_drafts)} boundary draft(s), "
                    f"{len(run.change_plan.edges)} edge(s)",
                )
            except LLMError as exc:
                run.degraded.append(
                    f"integration_scan_degraded: {exc.kind.value}: {exc.message}"
                )
                self._record("2.integration", exc.kind, started, exc.message)
            except Exception as exc:
                run.degraded.append(
                    f"integration_scan_degraded: {type(exc).__name__}: {exc}"
                )
                self._record("2.integration", AttemptStatus.PROCESS_CRASH, started, str(exc))
        self._record(
            "2.behavioral",
            AttemptStatus.COMPLETED if not failed else AttemptStatus.SKIPPED,
            started,
            f"{len(kept)} hypotheses from {len(batches)} batch(es), {rejected} rejected, {failed} batch failure(s)",
        )
        if rejected:
            run.degraded.append(f"behavioral_scan_filtered: {rejected} hypothesis(es) were unanchored")
        if over_cap:
            run.degraded.append(
                f"behavioral_candidate_coverage_incomplete: {over_cap} anchored hypothesis(es) exceeded the candidate cap"
            )
        required = {
            (patch.path, line)
            for patch in diff.files
            if _behavioral_eligible(patch, self.settings.probe_adapters) and not _is_test_path(patch.path)
            for line in patch.changed_lines
        }
        covered = {
            (patch.path, line)
            for batch in batches
            for patch in batch
            for line in patch.changed_lines
        }
        missing = required - covered
        run.coverage_total = len(required)
        run.coverage_covered = len(required & covered)
        if missing:
            run.degraded.append(
                f"behavioral_hunk_coverage_incomplete: {len(missing)} production hunk(s) were not assigned to a batch"
            )
        if capped_batches:
            run.degraded.append(
                f"behavioral_coverage_incomplete: only {len(batches)} of the ranked file batches fit the review budget"
            )
        if oversized:
            run.degraded.append(
                "behavioral_coverage_incomplete: at least one changed file is larger than a single evidence block"
            )
        return kept

    # ------------------------------------------------------------------ #
    def _stage3(
        self,
        candidates: list[Candidate],
        run: _Run,
        repo: GitRepo,
        diff: DiffBundle,
        budget: Budget,
    ) -> tuple[list[Candidate], list[ProbePair]]:
        assert run.context is not None
        pairs: list[ProbePair] = []
        sandbox = self.sandbox
        ok, detail = (False, "sandbox not queried")
        try:
            ok, detail = sandbox.available()
        except Exception as exc:
            detail = f"{type(exc).__name__}: {exc}"
        if not ok:
            run.degraded.append(f"sandbox_unavailable: {detail}")
        updated: list[Candidate] = []
        probed = 0
        stage_settings = replace(
            self.settings,
            sandbox=replace(
                self.settings.sandbox,
                timeout_s=max(5.0, min(self.settings.sandbox.timeout_s, budget.remaining() - 5.0)),
            ),
            deadline_s=max(5.0, budget.remaining()),
        )
        ordered = sorted(candidates, key=lambda c: (-_severity(c.severity), c.id))
        pending: list[tuple[int, Candidate]] = []
        updates: dict[int, list[Candidate]] = {}

        def record_skip(index: int, candidate: Candidate, reason: str = "") -> None:
            updates[index] = [candidate]
            if reason:
                self._record("3.probe", AttemptStatus.SKIPPED, _now(), f"{candidate.id}: {reason}")

        for index, candidate in enumerate(ordered):
            if candidate.status is FindingStatus.SUPPRESSED:
                record_skip(index, candidate)
                continue
            self._cap_llm(budget)
            if not ok or budget.remaining() < 20.0:
                updated.append(candidate)
                run.degraded.append("probe_coverage_incomplete: the deadline ended probe synthesis")
                updates[index] = [candidate]
                continue
            if not _probe_supported(candidate, self.settings.probe_adapters):
                record_skip(
                    index,
                    candidate,
                    f"no probe framework for {candidate.cited_sites[0].path if candidate.cited_sites else 'the cited site'}",
                )
                continue
            if probed >= MAX_PROBED_CANDIDATES:
                updates[index] = [candidate]
                run.degraded.append(
                    f"probe_coverage_incomplete: only the first {MAX_PROBED_CANDIDATES} candidates were probed"
                )
                continue
            probed += 1
            pending.append((index, candidate))

        def probe_candidate(item: tuple[int, Candidate]):
            index, candidate = item
            started = _now()
            try:
                result, new_pairs, degraded = cascade_mod.run_cascade(
                    candidate,
                    run.context,
                    self.llm,
                    sandbox,
                    repo,
                    diff,
                    stage_settings,
                    healthz=lambda: not budget.expired(),
                )
                return index, candidate, result, new_pairs, degraded, started, ""
            except Exception as exc:  # one broken candidate must not stop the rest
                return (
                    index,
                    candidate,
                    [candidate],
                    [],
                    [],
                    started,
                    f"{type(exc).__name__}: {exc}",
                )

        workers = max(1, min(int(getattr(self.settings, "probe_workers", 1) or 1), len(pending) or 1))
        if pending:
            with ThreadPoolExecutor(max_workers=workers) as pool:
                futures = [pool.submit(probe_candidate, item) for item in pending]
                for future in as_completed(futures):
                    index, candidate, result, new_pairs, degraded, started, failure = future.result()
                    updates[index] = result
                    run.degraded.extend(degraded)
                    pairs.extend(new_pairs)
                    if failure:
                        run.degraded.append(f"cascade_failed: {candidate.id}: {failure}")
                        self._record("3.probe", AttemptStatus.PROCESS_CRASH, started, failure)
                        continue
                    status = AttemptStatus.COMPLETED if new_pairs else AttemptStatus.SKIPPED
                    pair = new_pairs[-1] if new_pairs else None
                    self._record(
                        "3.probe",
                        status,
                        started,
                        (
                            f"{candidate.id}: base={pair.base.status.value} head={pair.head.status.value} "
                            f"-> {pair.classification.value}"
                            if pair
                            else f"{candidate.id}: no probe pair"
                        ),
                    )
                    if pair is not None:
                        for side in (pair.base, pair.head):
                            if side.input_sha256:
                                self._record(
                                    "3.sandbox",
                                    AttemptStatus.COMPLETED if side.mechanical else AttemptStatus.SANDBOX_UNAVAILABLE,
                                    started,
                                    f"{side.status.value} exit={side.exit_code} image={side.container_image}",
                                    exit_code=side.exit_code,
                                    input_sha256=side.input_sha256,
                                )
        for index, candidate in enumerate(ordered):
            if index not in updates:
                updates[index] = [candidate]
        updated = [item for index in range(len(ordered)) for item in updates.get(index, [ordered[index]])]

        # Stage 3C: grounded 5-vote fallback for anything still unverified.
        for candidate in list(updated):
            if candidate.status is not FindingStatus.UNVERIFIED or budget.remaining() < 15.0:
                continue
            # Rule-backed candidates are verified by CIVER's analyzer rerun;
            # a 5-vote LLM fallback adds no mechanical evidence for them.  The
            # fallback is only for probe-capable candidates whose probe did not
            # produce a usable pair.
            if not _probe_supported(candidate, self.settings.probe_adapters):
                continue
            pair = candidate.verification.probe
            if (
                pair is not None
                and pair.head.status is ProbeStatus.FAIL_BEHAVIORAL
                and candidate.verification.probe_spec is not None
            ):
                # A head-side failing probe is already the stronger evidence;
                # CIVER will use it to verify the repair, so five model votes
                # would only spend budget on a weaker signal.
                continue
            started = _now()
            try:
                verified = cascade_mod.fallback_verify(
                    candidate, run.context, self.llm, settings=self.settings
                )
                updated[updated.index(candidate)] = verified
                self._record("3.fallback", AttemptStatus.COMPLETED, started, f"{candidate.id} -> {verified.status.value}")
            except LLMError as exc:
                run.degraded.append(f"fallback_degraded: {candidate.id}: {exc.kind.value}: {exc.message}")
                self._record("3.fallback", exc.kind, started, exc.message)
            except Exception as exc:
                run.degraded.append(f"fallback_degraded: {candidate.id}: {type(exc).__name__}: {exc}")
        return updated, pairs

    # ------------------------------------------------------------------ #
    def _stage4(
        self,
        candidates: list[Candidate],
        run: _Run,
        repo: GitRepo,
        diff: DiffBundle,
        budget: Budget,
    ) -> list[Candidate]:
        assert run.context is not None and run.diff is not None
        try:
            precedents = designlore_mod.load_precedents(repo, diff.head_sha, self.settings)
            directives = designlore_mod.load_directives(repo, diff.head_sha, self.settings)
        except Exception as exc:
            precedents, directives = [], []
            run.degraded.append(f"designlore_unavailable: {type(exc).__name__}: {exc}")

        work_root = self.settings.paths.work_dir
        budget_settings = replace(self.settings, deadline_s=max(5.0, budget.remaining()))
        frames = {frame.path: frame for frame in (run.stage2.frames if run.stage2 else [])}
        rule_tracks = {
            finding.rule_id: finding.track
            for finding in (run.stage2.findings if run.stage2 else [])
        }
        head_tree: Path | None = None
        out: list[Candidate] = []
        civer_calls = 0
        try:
            for candidate in sorted(candidates, key=lambda c: (-_severity(c.severity), c.id)):
                if candidate.status is FindingStatus.SUPPRESSED:
                    out.append(candidate)
                    continue
                self._cap_llm(budget)

                keep, reason = designlore_mod.filter_candidate(
                    candidate, precedents, directives, repo_root=repo.root
                )
                if not keep:
                    out.append(
                        candidate.model_copy(
                            update={
                                "status": FindingStatus.SUPPRESSED,
                                "suppression_reason": f"designlore: {reason}",
                            }
                        )
                    )
                    run.degraded.append(f"designlore_suppressed: {candidate.id}: {reason}")
                    continue

                probe_verified = (
                    candidate.status is FindingStatus.VERIFIED
                    and candidate.verification.kind is VerificationKind.PROBE
                )
                pair = candidate.verification.probe
                spec = candidate.verification.probe_spec
                # A new function cannot run on the base commit, so a dual-commit
                # pair legitimately reports a setup error there.  The head-side
                # behavioural failure is still a mechanical signal CIVER can use.
                probe_capable = (
                    spec is not None
                    and pair is not None
                    and pair.head.status is ProbeStatus.FAIL_BEHAVIORAL
                )

                # Stage 4B: bidirectional LSOG check for static/resource alerts.
                # A pattern rule such as Rust's `unwrap()` is not a taint or
                # lifetime obligation, so ObliVul cannot express it; its CIVER
                # analyzer rerun is the right falsifier.  Suppressing here would
                # discard every non-Python rule finding.
                track = rule_tracks.get(candidate.evidence.rule_id, "")
                oblivul_applies = track in ("taint", "lifetime") or (
                    not candidate.evidence.rule_id
                    and candidate.evidence.kind != "analyzer"
                    and bool(candidate.cwe)
                )
                if (
                    not probe_capable
                    and oblivul_applies
                    and budget.remaining() > 25.0
                ):
                    started = _now()
                    try:
                        frame = frames.get(candidate.cited_sites[0].path if candidate.cited_sites else "")
                        if frame is None:
                            site = candidate.cited_sites[0] if candidate.cited_sites else None
                            if site is not None:
                                source = repo.show(diff.head_sha, site.path)
                                frame = cpg_mod.frame_projection(site.path, source, _language(site.path))
                        if frame is None:
                            raise ReviewError("no CPG frame for the candidate")
                        if head_tree is None:
                            head_tree = repo.add_worktree(diff.head_sha, work_root / f"stage4-head-{sha256_hex(candidate.id)[:8]}")
                        spec_path = work_root / f"lsog-{candidate.id}.json"
                        sinks = _sink_lines(candidate, frame)
                        spec_path.write_text(
                            canonical_json({"path": _sink_path(frame), "sinks": sinks}),
                            encoding="utf-8",
                        )
                        command = [
                            "python3",
                            str(self._rules_dir / "lsog_risk.py"),
                            "--spec",
                            str(spec_path),
                        ]
                        result = oblivul_mod.counterfactuals(
                            candidate,
                            frame,
                            head_tree,
                            self.runner,
                            budget_settings,
                            analysis_cmd=command,
                        )
                        self._record(
                            "4.oblivul",
                            AttemptStatus.COMPLETED if result.passed else AttemptStatus.SKIPPED,
                            started,
                            result.detail,
                        )
                        if not result.passed:
                            out.append(
                                candidate.model_copy(
                                    update={
                                        "status": FindingStatus.SUPPRESSED,
                                        "suppression_reason": f"oblivul: {result.detail}",
                                    }
                                )
                            )
                            run.degraded.append(f"oblivul_suppressed: {candidate.id}: {result.detail[:300]}")
                            continue
                        candidate = candidate.model_copy(
                            update={
                                "verification": candidate.verification.model_copy(
                                    update={"counterfactual": result}
                                )
                            }
                        )
                    except Exception as exc:
                        run.degraded.append(f"oblivul_degraded: {candidate.id}: {type(exc).__name__}: {exc}")
                        self._record("4.oblivul", AttemptStatus.PROCESS_CRASH, started, str(exc))

                # Stage 4A: a committable, verified repair for every finding.
                # Without a probe or a rule to rerun there is no way to show the
                # patch removes the defect, so do not spend a model call on it.
                if budget.remaining() > 20.0 and (
                    probe_capable or bool(candidate.evidence.rule_id)
                ):
                    if civer_calls >= MAX_CIVER_CANDIDATES:
                        run.degraded.append(
                            f"civer_coverage_incomplete: only the first {MAX_CIVER_CANDIDATES} candidates received repair verification"
                        )
                    else:
                        civer_calls += 1
                        candidate = self._civer(candidate, run, repo, diff, budget, probe_capable)
                out.append(candidate)
        finally:
            if head_tree is not None:
                try:
                    repo.remove_worktree(head_tree)
                except Exception:
                    pass
        return out

    # ------------------------------------------------------------------ #
    def _civer(
        self,
        candidate: Candidate,
        run: _Run,
        repo: GitRepo,
        diff: DiffBundle,
        budget: Budget,
        probe_capable: bool,
    ) -> Candidate:
        assert run.context is not None
        budget_settings = replace(
            self.settings,
            deadline_s=max(5.0, budget.remaining()),
            sandbox=replace(
                self.settings.sandbox,
                timeout_s=max(5.0, min(self.settings.sandbox.timeout_s, budget.remaining() - 2.0)),
            ),
        )
        started = _now()
        try:
            proposal = civer_mod.propose_fix(candidate, run.context, self.llm)
        except LLMError as exc:
            run.degraded.append(f"civer_proposal: {candidate.id}: {exc.kind.value}: {exc.message}")
            self._record("4.civer.propose", exc.kind, started, exc.message)
            return candidate
        except Exception as exc:
            run.degraded.append(f"civer_proposal: {candidate.id}: {type(exc).__name__}: {exc}")
            return candidate
        if proposal is None:
            run.degraded.append(f"civer_no_patch: {candidate.id}: the model returned no usable unified diff")
            self._record("4.civer.propose", AttemptStatus.MALFORMED_OUTPUT, started, "no patch")
            return candidate

        defect_probe = None
        analyzer_rerun = None
        spec = candidate.verification.probe_spec
        if probe_capable and spec is not None:
            sandbox = self.sandbox

            def defect_probe(tree: Path) -> bool:
                result = sandbox.run_probe(
                    tree,
                    spec,
                    expected_sha=sha256_hex(spec.content.encode("utf-8")),
                    timeout_s=budget_settings.sandbox.timeout_s,
                )
                # Only a real PASS clears the signal.  A setup error, timeout or
                # sandbox failure on the patched tree is *not* evidence that the
                # repair works, so the signal stays "present".
                return result.status is not ProbeStatus.PASS

        if not probe_capable and candidate.cited_sites:
            rule = candidate.evidence.rule_id

            def analyzer_rerun(tree: Path) -> bool:
                return _rule_present(
                    tree,
                    candidate.cited_sites[0].path,
                    rule,
                    budget_settings,
                    budget.grant(45.0),
                    candidate.cited_sites[0].line,
                    candidate.cited_sites[0].end_line,
                )

        started = _now()
        try:
            result = civer_mod.verify_fix(
                candidate,
                proposal,
                repo,
                diff.head_sha,
                self.settings.paths.work_dir,
                self.runner,
                budget_settings,
                defect_probe=defect_probe,
                analyzer_rerun=analyzer_rerun,
                linter=None,
                tool_root=repo.root,
            )
            self._record(
                "4.civer.verify",
                AttemptStatus.COMPLETED if result.passed else AttemptStatus.SKIPPED,
                started,
                f"{result.detail} patch_sha256={sha256_hex(proposal.patch)[:16]}",
                input_sha256=sha256_hex(proposal.patch),
                input_bytes=len(proposal.patch.encode("utf-8")),
            )
        except Exception as exc:
            run.degraded.append(f"civer_failed: {candidate.id}: {type(exc).__name__}: {exc}")
            self._record("4.civer.verify", AttemptStatus.PROCESS_CRASH, started, str(exc))
            return candidate

        if not result.passed or result.fix is None:
            run.degraded.append(f"civer_unverified: {candidate.id}: {result.detail[:300]}")
            return candidate.model_copy(
                update={
                    "verification": candidate.verification.model_copy(update={"civer": result}),
                }
            )
        kind = VerificationKind.PROBE if probe_capable else VerificationKind.PERTURBATION
        return candidate.model_copy(
            update={
                "status": FindingStatus.VERIFIED,
                "verified_fix": result.fix,
                "verification": candidate.verification.model_copy(
                    update={"kind": kind, "civer": result}
                ),
            }
        )

    # ------------------------------------------------------------------ #
    # Final assembly: Stage 5
    # ------------------------------------------------------------------ #
    def _finish(
        self,
        review_id: str,
        request: ReviewRequest,
        repo: GitRepo,
        head_sha: str,
        run: _Run,
        budget: Budget,
        *,
        candidates: list[Candidate] | None = None,
        force_human: bool = False,
        blind_spots: Sequence[str] = (),
    ) -> ReviewVerdict:
        candidates = list(candidates or [])
        base_sha = run.diff.base_sha if run.diff is not None else ""
        # An unavailable model means the semantic sweep never happened.  That is
        # a failed attempt, and a failed attempt fails open to a human.
        if isinstance(self.llm, NullLLM):
            run.degraded.append("llm_unavailable: no model endpoint is configured; a human must review")
            force_human = True
        if force_human or (run.mci is not None and run.mci.contradiction):
            force_human = True

        # Only publishable or decision-relevant candidates are fused, capped to
        # keep the model budget inside the CI window.
        eligible = [c for c in candidates if c.status is not FindingStatus.SUPPRESSED]
        eligible.sort(key=lambda c: (-_severity(c.severity), c.id))
        chosen = eligible[:5]
        # Unverified hypotheses are never published, so fusing them cannot make
        # them publishable; it only burns five monitor calls per hypothesis on
        # every clean PR.  Only mechanically verified candidates reach Stage 5.
        verified_chosen = [c for c in chosen if c.status is FindingStatus.VERIFIED]
        mechanically_confirmed = any(
            candidate.status is FindingStatus.VERIFIED
            and candidate.verified_fix is not None
            and candidate.verification.kind
            in (
                VerificationKind.PROBE,
                VerificationKind.PERTURBATION,
                VerificationKind.CPG_COUNTERFACTUAL,
                VerificationKind.ANALYZER_TRACE,
            )
            for candidate in candidates
        )
        if force_human:
            run.degraded.append("fusion_skipped: the review was forced to human review")
        elif not verified_chosen:
            run.degraded.append(
                "fusion_skipped: no mechanically verified candidate; unverified hypotheses are not published"
            )
        elicitations: dict[str, RiskElicitation] = {}
        monitors: dict[str, MonitorScore] = {}

        def fuse_one(candidate: Candidate) -> tuple[str, RiskElicitation | None, MonitorScore | None, str]:
            reasons: list[str] = []
            elicitation = None
            monitor = None
            if not budget.expired():
                elicitation, why = elicit_mod.elicit(candidate, run.context, self.llm, self.settings) if run.context else (None, "no context")
                if elicitation is None:
                    reasons.append(f"elicit: {why}")
            if not budget.expired():
                monitor, why = fusion_mod.monitor_scores_or_none(
                    candidate, run.context, self.llm, self.monitor_llm, self.settings
                ) if run.context else (None, "no context")
                if monitor is None:
                    reasons.append(f"monitor: {why}")
            return candidate.id, elicitation, monitor, "; ".join(reasons)

        if verified_chosen and not force_human and budget.remaining() > 15.0:
            self._cap_llm(budget)
            workers = min(4, len(verified_chosen))
            with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
                futures = [pool.submit(fuse_one, candidate) for candidate in verified_chosen]
                for future in futures:
                    try:
                        cid, elicitation, monitor, why = future.result()
                    except Exception as exc:
                        run.degraded.append(f"fusion_failed: {type(exc).__name__}: {exc}")
                        continue
                    if elicitation is not None:
                        elicitations[cid] = elicitation
                    if monitor is not None:
                        monitors[cid] = monitor
                    if why:
                        run.degraded.append(f"fusion_degraded: {cid}: {why}")

        fused: dict[str, FusionResult] = {}
        for candidate in verified_chosen:
            elicitation = elicitations.get(candidate.id)
            monitor = monitors.get(candidate.id)
            if elicitation is None or monitor is None:
                continue
            result = fusion_mod.combine(elicitation.failure_probability, monitor, self.settings.fusion)
            fused[candidate.id] = result
            candidate.p = elicitation.failure_probability
            candidate.monitor = monitor
            candidate.p_combined = result.p_combined

        if force_human:
            decision, p_worst, reasons = Decision.HUMAN_REVIEW, 0.0, []
        elif verified_chosen:
            decision, p_worst, reasons = fusion_mod.decide(
                verified_chosen,
                elicitations,
                monitors,
                self.settings.fusion,
                mechanically_confirmed=mechanically_confirmed,
            )
            run.degraded.extend(
                f"decision: {reason}" for reason in reasons if "disagree" in reason or "unfusable" in reason
            )
        else:
            severe_unverified = any(
                candidate.status in (FindingStatus.UNVERIFIED, FindingStatus.ADVISORY)
                and _severity(candidate.severity) >= 2
                for candidate in candidates
            )
            decision = Decision.HUMAN_REVIEW if severe_unverified else Decision.APPROVE
            p_worst = 1.0 if severe_unverified else 0.0
            reasons = []

        # Mark exploitable security findings before selection: a redacted flag
        # is the one published shape that may omit a committable patch.
        candidates = [_mark_exploitable(candidate, self.settings) for candidate in candidates]

        already_suppressed = [c for c in candidates if c.status is FindingStatus.SUPPRESSED]
        # Two stages can reach the same repair (a rule finding and a semantic
        # monologue on the same line).  Publish one comment, not a duplicate.
        # Keep the security-flagged candidate when the two differ: suppressing
        # it in favour of a generic copy would also lose its private redaction.
        groups: dict[str, list[Candidate]] = {}
        deduped: list[Candidate] = []
        for candidate in candidates:
            if candidate.status is not FindingStatus.VERIFIED:
                deduped.append(candidate)
                continue
            key = (
                "suggestion:" + formatter_mod.suggestion_block(candidate.verified_fix)
                if candidate.verified_fix is not None
                else "redacted:" + "|".join(f"{s.path}:{s.line}" for s in candidate.cited_sites[:1])
            )
            groups.setdefault(key, []).append(candidate)
        duplicate_suppressed: list[Candidate] = []
        for group in groups.values():
            best = max(
                group,
                key=lambda c: (bool(c.security_exploitable), c.severity, c.p_combined or 0.0, c.id),
            )
            deduped.append(best)
            for candidate in group:
                if candidate.id == best.id:
                    continue
                duplicate_suppressed.append(
                    candidate.model_copy(
                        update={
                            "status": FindingStatus.SUPPRESSED,
                            "suppression_reason": f"duplicate_of: {best.id}",
                        }
                    )
                )
                run.degraded.append(f"duplicate_suppressed: {candidate.id} duplicates {best.id}")
        candidates = deduped
        already_suppressed = [*already_suppressed, *duplicate_suppressed]
        selected, suppressed = formatter_mod.select_with_receipts(
            [c for c in candidates if c.status in (FindingStatus.VERIFIED, FindingStatus.ADVISORY, FindingStatus.UNVERIFIED)],
            self.settings.max_findings,
        )
        suppressed = [*already_suppressed, *suppressed]
        published: list[Finding] = []
        private_log_path = ""
        for candidate in selected:
            redaction = None
            if candidate.security_exploitable and not candidate.public_safe:
                redaction = redaction_mod.redact(
                    candidate,
                    self.settings.redaction,
                    private_dir=self.settings.paths.state_dir / "private",
                )
                private_log_path = str(self.settings.paths.state_dir / "private")
            elicitation = elicitations.get(candidate.id)
            monitor = monitors.get(candidate.id)
            verified = candidate.status is FindingStatus.VERIFIED
            p_combined = fused[candidate.id].p_combined if candidate.id in fused else (1.0 if verified else 0.0)
            if candidate.p is None:
                candidate = candidate.model_copy(update={"p": 1.0 if verified else 0.0, "p_combined": p_combined})
            elif candidate.p_combined is None:
                candidate = candidate.model_copy(update={"p_combined": p_combined})
            published.append(
                formatter_mod.to_finding(candidate, elicitation, monitor, p_combined, redaction)
            )

        # Decision overrides: a mechanically confirmed defect that cannot be
        # published is a human decision, not an approval.
        if force_human:
            decision = Decision.HUMAN_REVIEW
        elif mechanically_confirmed and not published:
            decision = Decision.HUMAN_REVIEW
            run.degraded.append(
                "unpublishable_confirmed_defect: a mechanical check confirmed a defect but no verified "
                "one-click suggestion could be produced; a human takes over"
            )
        elif published and decision is Decision.APPROVE:
            decision = Decision.REQUEST_CHANGES
            run.degraded.append(
                "published_finding_forces_changes: a verified finding is published, so the review "
                "cannot be an approval even when the fused probability stays under the threshold"
            )
        elif decision is Decision.REQUEST_CHANGES and not published:
            decision = Decision.HUMAN_REVIEW
            run.degraded.append("unpublishable_finding: the threshold was crossed but no verified suggestion exists")
        elif decision is Decision.APPROVE and blind_spots and not published:
            decision = Decision.HUMAN_REVIEW
            run.degraded.append(
                "incomplete_analysis: "
                + ", ".join(blind_spots[:5])
                + "; a failed stage is not evidence of a clean PR"
            )
        elif any(
            candidate.status is FindingStatus.VERIFIED
            and candidate.verified_fix is None
            and not formatter_mod.is_redacted_flag(candidate)
            for candidate in candidates
        ) and not published:
            decision = Decision.HUMAN_REVIEW
            run.degraded.append(
                "unexpressible_verified_finding: a verified defect has no committable suggestion, "
                "so it cannot be published and must not be silently approved"
            )
        elif any(
            candidate.status is FindingStatus.UNVERIFIED and _severity(candidate.severity) >= 2
            for candidate in candidates
        ) and not published:
            decision = Decision.HUMAN_REVIEW
            run.degraded.append("unverified_candidates: at least one severe candidate could not be mechanically verified")

        verdict = self._verdict(
            review_id,
            base_sha,
            head_sha,
            decision,
            (1.0 if mechanically_confirmed else p_worst),
            published,
            run,
            run.degraded,
            suppressed=suppressed,
            gate=run.gate,
            mci=run.mci,
            budget=budget,
            private_log_path=private_log_path,
        )
        verdict = verdict.model_copy(
            update={"repository": request.repository, "pull_number": request.pull_number}
        )
        if self._log is not None:
            self._log.write_json("verdict.json", verdict.model_dump(mode="json"))
        return verdict

    # ------------------------------------------------------------------ #
    def _verdict(
        self,
        review_id: str,
        base_sha: str,
        head_sha: str,
        decision: Decision,
        p_combined: float,
        findings: list[Finding],
        run: _Run,
        degraded: list[str],
        *,
        suppressed: list[Candidate] | None = None,
        gate: GateResult | None = None,
        mci: MciResult | None = None,
        budget: Budget | None = None,
        private_log_path: str = "",
    ) -> ReviewVerdict:
        suppressed = suppressed or []
        tau = self.settings.fusion.tau
        return ReviewVerdict(
            review_id=review_id,
            repository="",
            pull_number=0,
            base_sha=base_sha,
            head_sha=head_sha,
            decision=decision,
            p_combined=max(0.0, min(1.0, p_combined)),
            tau=tau,
            findings=findings[: (self.settings.max_findings or formatter_mod.MAX_FINDINGS)],
            suppressed_count=len(suppressed),
            redacted_count=sum(1 for finding in findings if finding.redacted),
            receipts=list(self._receipts),
            degraded=_unique(degraded),
            elapsed_ms=int(budget.elapsed() * 1000) if budget is not None else 0,
            budget_exceeded=bool(budget is not None and budget.expired()),
            mci=mci,
            gate=gate,
            private_log_path=private_log_path,
            input_sha256=run.context.input_sha256 if run.context is not None else "",
            coverage_lines=run.coverage_total,
            coverage_covered=run.coverage_covered,
        )


# --------------------------------------------------------------------------- #
# Candidate construction and de-duplication
# --------------------------------------------------------------------------- #


def _category_of(finding: AnalyzerFinding) -> Category:
    if finding.cwe or finding.track in ("taint", "lifetime"):
        return Category.SECURITY
    return Category.CORRECTNESS


def _analyzer_candidates(findings: Sequence[AnalyzerFinding]) -> list[Candidate]:
    out: list[Candidate] = []
    for finding in findings:
        if finding.suppressed or not finding.patch_caused:
            continue
        site = CitedSite(path=finding.path, line=finding.line, end_line=finding.end_line, symbol=finding.rule_id)
        category = _category_of(finding)
        severity = _severity(finding.severity)
        if category is Category.SECURITY:
            severity = max(severity, 2)
        title = f"{finding.rule_id or finding.analyzer} at {finding.path}:{finding.line}"
        out.append(
            Candidate(
                id=_candidate_id(StageId.SAST.value, finding.path, finding.line, title),
                stage=StageId.SAST,
                category=category,
                title=title,
                root_cause=finding.message or f"{finding.analyzer} reported {finding.rule_id}",
                severity=severity,
                cited_sites=[site],
                cwe=finding.cwe,
                evidence=Evidence(
                    kind="analyzer",
                    analyzer=finding.analyzer,
                    rule_id=finding.rule_id,
                    trace=[site],
                    detail=f"{finding.track} rule {finding.rule_id}",
                ),
            )
        )
    return out


def _rale_candidates(results: Sequence[RaleResult]) -> list[Candidate]:
    out: list[Candidate] = []
    for result in results:
        if not result.flagged:
            continue
        reasons = [*result.ast_reasons, *result.cfg_reasons]
        if result.code_flag:
            reasons.append(f"local classifier score {result.code_score:.2f}")
        title = f"structural risk in {result.function or result.path}"
        out.append(
            Candidate(
                id=_candidate_id(StageId.RALE.value, result.path, result.line, title),
                stage=StageId.RALE,
                category=Category.LOGIC,
                title=title,
                root_cause="; ".join(reasons[:6]) or "AST/CFG ensemble flagged this function",
                severity=2,
                cited_sites=[CitedSite(path=result.path, line=result.line, symbol=result.function)],
                evidence=Evidence(
                    kind="classifier",
                    analyzer=result.classifier or "rale",
                    trace=[CitedSite(path=result.path, line=result.line, symbol=result.function)],
                    detail="(AST && CFG) || Code",
                ),
            )
        )
    return out


def dedupe_candidates(candidates: Iterable[Candidate]) -> list[Candidate]:
    """One candidate per (path, line, cwe) or (path, line, rule), worst first."""
    chosen: dict[tuple[str, int, str], Candidate] = {}
    for candidate in candidates:
        site = candidate.cited_sites[0] if candidate.cited_sites else CitedSite(path="", line=1)
        key = (site.path, site.line, candidate.cwe or candidate.evidence.rule_id or candidate.category.value)
        previous = chosen.get(key)
        if previous is None or (_severity(candidate.severity), candidate.stage.value) > (
            _severity(previous.severity),
            previous.stage.value,
        ):
            chosen[key] = candidate
    return sorted(chosen.values(), key=lambda c: (-_severity(c.severity), c.id))


def _unique(values: Iterable[str]) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    for value in values:
        if value and value not in seen:
            seen.add(value)
            out.append(value)
    return out


def _mark_exploitable(candidate: Candidate, settings: Settings) -> Candidate:
    if not redaction_mod.is_exploitable(candidate, settings.redaction):
        return candidate
    return candidate.model_copy(update={"security_exploitable": True, "public_safe": False})


def _site_in_diff(diff: DiffBundle, site: CitedSite) -> bool:
    """A hypothesis may only cite a line the patch actually touches."""
    file_patch = diff.file(site.path)
    if file_patch is None or file_patch.is_deleted:
        return False
    changed = file_patch.changed_lines
    if not changed:
        return False
    return any(abs(site.line - line) <= 2 for line in changed)


_BEHAVIOR_EXTENSIONS = {
    ".py", ".pyi", ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs",
    ".go", ".java", ".kt", ".rb", ".php", ".c", ".h", ".cc", ".cpp", ".hpp",
    ".cs", ".swift", ".scala", ".ex", ".exs", ".lua", ".dart", ".vue",
    ".svelte", ".erb", ".twig", ".m", ".mm", ".fs", ".fsx", ".jl", ".pl", ".pm",
}
_NON_CODE_SUFFIXES = {
    ".md", ".rst", ".txt", ".json", ".jsonl", ".ndjson", ".yaml", ".yml",
    ".toml", ".ini", ".cfg", ".lock", ".sum", ".mod", ".snap", ".csv", ".tsv",
    ".svg", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".pdf", ".zip",
    ".gz", ".tar", ".woff", ".woff2", ".ttf", ".map",
}
_BEHAVIOR_SKIP_PARTS = {
    "node_modules", "target", "dist", "build", "vendor", "third_party",
    "site", ".git", ".venv", "venv", "__pycache__",
}
_BEHAVIOR_RISK_WORDS = (
    "auth", "security", "session", "token", "payment", "billing", "db",
    "sql", "exec", "parse", "io", "api", "route", "handler",
)


def _behavioral_eligible(
    patch: FilePatch, adapters: Sequence[ProbeAdapter] = BUILTIN_ADAPTERS
) -> bool:
    if patch.is_binary or patch.is_deleted or not patch.changed_lines:
        return False
    path = patch.path.replace("\\", "/")
    if any(part in _BEHAVIOR_SKIP_PARTS for part in Path(path).parts):
        return False
    if adapter_for(path, adapters) is not None:
        return True
    suffix = Path(path).suffix.lower()
    if not suffix:
        return True
    if suffix in _NON_CODE_SUFFIXES:
        return False
    # Unknown languages still get an LLM review; only an adapter or an
    # analyzer rule can turn one of those hypotheses into a verified finding.
    return True


def _behavioral_weight(patch: FilePatch) -> int:
    score = min(200, len(patch.changed_lines))
    low = patch.path.lower()
    if any(word in low for word in _BEHAVIOR_RISK_WORDS):
        score += 50
    if "test" in low or "spec" in low:
        score -= 20
    return score


def _is_test_path(path: str) -> bool:
    low = path.lower()
    return (
        "/tests/" in low
        or "/test/" in low
        or Path(low).name.startswith("test_")
        or ".test." in low
        or ".spec." in low
        or low.endswith("_test.py")
        or low.endswith("_test.rs")
        or re.search(
            r"(_test|_spec)\.(go|rb|php|exs|ex|cs|swift|lua|kt|java|scala|c|cc|cpp|h|hpp|fs|fsx|jl|pl|pm)$",
            low,
        )
        is not None
    )


def _split_hunk(hunk) -> tuple[list, bool]:
    """Split one oversized hunk into line-numbered pieces for evidence."""
    size = len(hunk.header) + sum(len(line) + 1 for line in hunk.lines)
    if size <= MAX_BEHAVIOR_BATCH_CHARS:
        return [hunk], False
    pieces = []
    current: list[str] = []
    chars = 0
    old_start = hunk.old_start
    new_start = hunk.new_start
    old_count = 0
    new_count = 0
    old_cursor = old_start
    new_cursor = new_start

    def flush() -> None:
        nonlocal current, chars, old_start, new_start, old_count, new_count
        if not current:
            return
        pieces.append(
            hunk.model_copy(
                update={
                    "old_start": old_start,
                    "old_lines": old_count,
                    "new_start": new_start,
                    "new_lines": new_count,
                    "header": f"@@ -{old_start},{old_count} +{new_start},{new_count} @@",
                    "lines": list(current),
                }
            )
        )
        current = []
        chars = 0
        old_start = old_cursor
        new_start = new_cursor
        old_count = 0
        new_count = 0

    for raw in hunk.lines:
        if current and chars + len(raw) + 1 > MAX_BEHAVIOR_BATCH_CHARS:
            flush()
        current.append(raw)
        chars += len(raw) + 1
        if raw.startswith("+") and not raw.startswith("+++"):
            new_cursor += 1
            new_count += 1
        elif raw.startswith("-") and not raw.startswith("---"):
            old_cursor += 1
            old_count += 1
        elif raw.startswith("\\"):
            pass
        else:
            old_cursor += 1
            new_cursor += 1
            old_count += 1
            new_count += 1
    flush()
    return pieces, False


def _split_large_patch(patch: FilePatch) -> tuple[list[FilePatch], bool]:
    """Split a large file diff at hunk boundaries for behavioural batches."""
    pieces = []
    unsplittable = False
    for hunk in patch.hunks:
        split, bad = _split_hunk(hunk)
        pieces.extend(split)
        unsplittable = unsplittable or bad
    if not pieces:
        return [patch], True
    chunks: list[list] = []
    current: list = []
    chars = 0
    for piece in pieces:
        size = len(piece.header) + sum(len(line) + 1 for line in piece.lines)
        if current and chars + size > MAX_BEHAVIOR_BATCH_CHARS:
            chunks.append(current)
            current = []
            chars = 0
        current.append(piece)
        chars += size
    if current:
        chunks.append(current)
    return (
        [
            patch.model_copy(
                update={
                    "hunks": group,
                    "additions": sum(len(hunk.added_lines) for hunk in group),
                    "deletions": sum(hunk.old_lines for hunk in group),
                }
            )
            for group in chunks
        ],
        unsplittable,
    )


def _hunk_key(path: str, hunk) -> str:
    return f"{path}:{hunk.old_start}:{hunk.new_start}:{hunk.header}"


def _behavioral_batches(
    diff: DiffBundle,
    *,
    max_batches: int = MAX_BEHAVIOR_BATCHES,
    plan: ChangePlan | None = None,
    adapters: Sequence[ProbeAdapter] = BUILTIN_ADAPTERS,
) -> tuple[list[list[FilePatch]], bool, bool]:
    """Rank changed source files and group them into evidence-sized batches.

    When a change plan is available, batches never cross a semantic unit: a
    feature's API, implementation and tests stay in the same prompt even when
    they are split over several size-bounded batches.
    """
    files = sorted(
        (patch for patch in diff.files if _behavioral_eligible(patch, adapters)),
        key=lambda patch: (-_behavioral_weight(patch), patch.path),
    )
    if not files:
        return [], False, False

    def split(files: list[FilePatch]) -> tuple[list[FilePatch], bool]:
        units: list[FilePatch] = []
        oversized = False
        for patch in files:
            if len(patch.patch_text()) <= MAX_BEHAVIOR_BATCH_CHARS:
                units.append(patch)
                continue
            pieces, unsplittable = _split_large_patch(patch)
            if unsplittable and _is_test_path(patch.path):
                continue
            units.extend(pieces)
            oversized = oversized or unsplittable
        return units, oversized

    def chunk(units: list[FilePatch]) -> list[list[FilePatch]]:
        batches: list[list[FilePatch]] = []
        current: list[FilePatch] = []
        chars = 0
        for patch in units:
            size = len(patch.patch_text())
            if current and (
                chars + size > MAX_BEHAVIOR_BATCH_CHARS
                or len(current) >= MAX_BEHAVIOR_FILES_PER_BATCH
            ):
                batches.append(current)
                current = []
                chars = 0
            current.append(patch)
            chars += size
        if current:
            batches.append(current)
        return batches

    batches: list[list[FilePatch]] = []
    oversized = False
    assigned: set[str] = set()
    if plan is not None and plan.units:
        by_path = {patch.path: patch for patch in files}
        for unit in plan.units:
            unit_files = [by_path[path] for path in unit.paths if path in by_path]
            if not unit_files:
                continue
            assigned.update(patch.path for patch in unit_files)
            pieces, unsplittable = split(unit_files)
            oversized = oversized or unsplittable
            batches.extend(chunk(pieces))
    # A file the graph did not know about still gets reviewed.
    leftovers = [patch for patch in files if patch.path not in assigned]
    if leftovers:
        pieces, unsplittable = split(leftovers)
        oversized = oversized or unsplittable
        batches.extend(chunk(pieces))
    capped = len(batches) > max_batches
    return batches[:max_batches], capped, oversized


def _context_for_batch(context: ContextSlice, paths: set[str]) -> ContextSlice:
    selected = [
        unit
        for unit in context.units
        if unit.kind not in ("diff", "function") or unit.source in paths
    ]
    text = "\n\n".join(unit.text for unit in selected) or context.text
    text = text[: MAX_BEHAVIOR_BATCH_CHARS * 4]
    return context.model_copy(update={"text": text, "tokens": max(1, len(text) // 4)})


def _probe_supported(
    candidate: Candidate, adapters: Sequence[ProbeAdapter] = BUILTIN_ADAPTERS
) -> bool:
    """Whether a configured language adapter can run a probe for this site."""
    if not candidate.cited_sites:
        return False
    return adapter_for(candidate.cited_sites[0].path, adapters) is not None


def _groundable(
    site: CitedSite,
    findings: Sequence[AnalyzerFinding],
    adapters: Sequence[ProbeAdapter] = BUILTIN_ADAPTERS,
) -> bool:
    """Whether the stack can *test* a hypothesis about this site.

    Stage 3 can execute a Python, Rust, Go or JavaScript/TypeScript probe; a
    rule finding can be rerun for any language.  A behavioural guess about YAML,
    Markdown or another unsupported file has no mechanical falsifier, so it
    must not become a candidate: otherwise every clean docs/workflow PR ends in
    an unverified human review.
    """
    if adapter_for(site.path, adapters) is not None:
        return True
    return any(
        finding.path == site.path
        and finding.patch_caused
        and not finding.suppressed
        and abs(finding.line - site.line) <= 3
        for finding in findings
    )


def _language(path: str) -> str:
    if path.endswith((".py", ".pyi")):
        return "python"
    if path.endswith((".c", ".h")):
        return "c"
    if path.endswith((".cc", ".cpp", ".cxx", ".hpp")):
        return "cpp"
    return ""


def _sink_lines(candidate: Candidate, frame: Any) -> list[int]:
    lines: list[int] = []
    for site in candidate.cited_sites:
        if site.path == frame.path:
            lines.append(site.line)
    for taint in frame.taint:
        for site in (taint.sink, taint.source):
            if site.path == frame.path:
                lines.append(site.line)
    for lifetime in frame.lifetime:
        for site in (lifetime.use, lifetime.free, lifetime.alloc):
            if site is not None and site.path == frame.path:
                lines.append(site.line)
                break
    return sorted(set(lines))[:20] or [1]


def _sink_path(frame: Any) -> str:
    return frame.path


def _rule_present(
    tree: Path,
    rel_path: str,
    rule_id: str,
    settings: Settings,
    timeout_s: float,
    line: int | None = None,
    end_line: int | None = None,
) -> bool:
    """Rerun the shipped Semgrep rules on one file of a scratch tree.

    When ``line`` is given, only a result within a few lines of that site counts.
    A file can contain several findings of the same rule; removing one must not
    leave CIVER believing the candidate's defect is still present just because
    its siblings remain.
    """
    if not rule_id:
        return False
    config = settings.sast.semgrep_config or str(Path(__file__).resolve().parents[1] / "rules")
    target = Path(tree) / rel_path
    argv = [
        settings.sast.semgrep_bin,
        "--json",
        "--metrics=off",
        "--quiet",
        "--config",
        config,
        str(target),
    ]
    run = run_bounded(argv, cwd=Path(tree), timeout_s=max(5.0, timeout_s))
    if run.exit_code != 0 or not run.stdout.strip():
        # An analyzer that did not complete cannot show a rule is gone; keeping
        # the signal "present" makes CIVER fail closed.
        return True
    import json

    try:
        doc = json.loads(run.stdout)
    except ValueError:
        return True
    for result in doc.get("results", []):
        check = str(result.get("check_id", ""))
        if not (check == rule_id or check.endswith("." + rule_id) or rule_id in check):
            continue
        if line is not None:
            start = result.get("start") if isinstance(result, dict) else None
            end = result.get("end") if isinstance(result, dict) else None
            found_start = start.get("line") if isinstance(start, dict) else None
            found_end = end.get("line") if isinstance(end, dict) else None
            if isinstance(found_start, int):
                high = end_line if end_line is not None else line
                found_high = found_end if isinstance(found_end, int) else found_start
                # Overlap between the candidate's cited range and the rule hit.
                if found_high < line - 2 or found_start > high + 2:
                    continue
        return True
    return False
