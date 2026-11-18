"""Strict schemas for every boundary in the reviewer.

Rules that matter:

* LLM-facing models forbid extra keys.  A model that invents a field is a
  schema drift and must be recorded as ``malformed_output`` rather than
  silently accepted.
* ``RiskElicitation`` has exactly the six fields the elicitation contract
  names.  No cost, ratio, or merge-policy field exists on it, so policy cannot
  leak through the schema.
* Every finding carries its grounding: an analyzer trace, a CPG
  counterfactual, or a probe pair.  There is no "model said so" kind.
"""

from __future__ import annotations

import hashlib
import json
from enum import Enum, IntEnum
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator


# --------------------------------------------------------------------------- #
# Enumerations
# --------------------------------------------------------------------------- #


class StrictModel(BaseModel):
    model_config = ConfigDict(extra="forbid", populate_by_name=True)


class Decision(str, Enum):
    APPROVE = "approve"
    REQUEST_CHANGES = "request_changes"
    HUMAN_REVIEW = "human_review"


class Category(str, Enum):
    LOGIC = "logic"
    CORRECTNESS = "correctness"
    RESOURCE = "resource"
    SECURITY = "security"
    CONCURRENCY = "concurrency"
    API_CONTRACT = "api_contract"
    ERROR_HANDLING = "error_handling"
    PERFORMANCE = "performance"
    TEST_GAP = "test_gap"
    OTHER = "other"


class AttemptStatus(str, Enum):
    COMPLETED = "completed"
    PROCESS_CRASH = "process_crash"
    EMPTY_RESPONSE = "empty_response"
    MALFORMED_OUTPUT = "malformed_output"
    INPUT_DELIVERY_FAILED = "input_delivery_failed"
    TIMEOUT = "timeout"
    CANCELLED = "cancelled"
    SANDBOX_UNAVAILABLE = "sandbox_unavailable"
    TOOL_UNAVAILABLE = "tool_unavailable"
    NOT_CONFIGURED = "not_configured"
    SKIPPED = "skipped"

    @property
    def is_success(self) -> bool:
        return self is AttemptStatus.COMPLETED


class ProbeStatus(str, Enum):
    PASS = "pass"
    FAIL_BEHAVIORAL = "fail_behavioral"
    SETUP_ERROR = "setup_error"
    TIMEOUT = "timeout"
    SANDBOX_UNAVAILABLE = "sandbox_unavailable"
    INPUT_DELIVERY_FAILED = "input_delivery_failed"
    CANCELLED = "cancelled"
    MALFORMED_PROBE = "malformed_probe"


class VerificationKind(str, Enum):
    ANALYZER_TRACE = "analyzer_trace"
    CPG_COUNTERFACTUAL = "cpg_counterfactual"
    PROBE = "probe"
    PERTURBATION = "perturbation"
    CLASSIFIER_ENSEMBLE = "classifier_ensemble"
    UNVERIFIED = "unverified"


class FindingStatus(str, Enum):
    VERIFIED = "verified"
    SUPPRESSED = "suppressed"
    UNVERIFIED = "unverified"


class StageId(str, Enum):
    CONTRACT = "1"
    SAST = "2a"
    SECSEM = "2b"
    RALE = "2c"
    PROBE = "3"
    FALSIFY = "4"
    CONTROLLER = "5"


class SeverityLevel(IntEnum):
    INFO = 0
    LOW = 1
    MEDIUM = 2
    HIGH = 3


class GateDecision(str, Enum):
    ALLOW = "allow"
    REFUSE = "refuse"


class CheckConclusion(str, Enum):
    SUCCESS = "success"
    FAILURE = "failure"
    NEUTRAL = "neutral"
    CANCELLED = "cancelled"
    SKIPPED = "skipped"
    TIMED_OUT = "timed_out"
    ACTION_REQUIRED = "action_required"
    PENDING = "pending"
    UNKNOWN = "unknown"


class ProbeClassification(str, Enum):
    OMISSION_CONFIRMED = "omission_confirmed"
    FIX_CONFIRMED = "fix_confirmed"
    REGRESSION_CONFIRMED = "regression_confirmed"
    BOTH_FAIL = "both_fail"
    BOTH_PASS = "both_pass"
    SETUP_ERROR = "setup_error"
    INCONCLUSIVE = "inconclusive"


class ReconciliationTier(str, Enum):
    TIER1_ANALYZER = "tier1_analyzer"
    UNVERIFIED_TRUST = "unverified_trust"
    REJECTED = "rejected"


# --------------------------------------------------------------------------- #
# Small value objects
# --------------------------------------------------------------------------- #


def _clamp(value: float, low: float, high: float) -> float:
    return max(low, min(high, value))


class CitedSite(StrictModel):
    path: str = Field(min_length=1, max_length=1024)
    line: int = Field(ge=1, le=10_000_000)
    end_line: int | None = Field(default=None, ge=1, le=10_000_000)
    symbol: str = Field(default="", max_length=512)
    snippet: str = Field(default="", max_length=400)

    @model_validator(mode="after")
    def _end_after_start(self) -> "CitedSite":
        if self.end_line is not None and self.end_line < self.line:
            raise ValueError("end_line precedes line")
        return self

    def key(self) -> tuple[str, int]:
        return (self.path, self.line)


class TokenUsage(StrictModel):
    input: int = Field(default=0, ge=0)
    output: int = Field(default=0, ge=0)

    def add(self, other: "TokenUsage") -> "TokenUsage":
        return TokenUsage(input=self.input + other.input, output=self.output + other.output)


class ModelAnswer(StrictModel):
    """What a model actually answered, before any policy is applied."""

    value: Any
    raw: str = ""
    model: str = ""
    usage: TokenUsage = Field(default_factory=TokenUsage)
    attempts: int = 1


# --------------------------------------------------------------------------- #
# Diff and context
# --------------------------------------------------------------------------- #


class Hunk(StrictModel):
    old_start: int = Field(ge=0)
    old_lines: int = Field(ge=0)
    new_start: int = Field(ge=0)
    new_lines: int = Field(ge=0)
    header: str = ""
    lines: list[str] = Field(default_factory=list)

    @property
    def added_lines(self) -> list[int]:
        """New-file line numbers of ``+`` lines."""
        out: list[int] = []
        cursor = self.new_start
        for raw in self.lines:
            if raw.startswith("+") and not raw.startswith("+++"):
                out.append(cursor)
                cursor += 1
            elif raw.startswith("-") and not raw.startswith("---"):
                continue
            elif raw.startswith("\\"):
                continue
            else:
                cursor += 1
        return out


class FilePatch(StrictModel):
    path: str
    old_path: str | None = None
    is_new: bool = False
    is_deleted: bool = False
    is_binary: bool = False
    is_rename: bool = False
    additions: int = 0
    deletions: int = 0
    hunks: list[Hunk] = Field(default_factory=list)

    @property
    def changed_lines(self) -> set[int]:
        """New-file lines the patch adds, or anchors a removal to.

        A pure deletion has no ``+`` line; the defect it causes still needs a
        site to cite.  The new-file position where the deletion happened is the
        only honest anchor, and without it a removed guard would be invisible to
        the behavioral scan and to patch-caused filtering.
        """
        lines: set[int] = set()
        for hunk in self.hunks:
            added = hunk.added_lines
            lines.update(added)
            if not added and hunk.old_lines > 0:
                lines.add(max(1, hunk.new_start))
        return lines

    def patch_text(self) -> str:
        parts = [f"--- a/{self.old_path or self.path}", f"+++ b/{self.path}"]
        for hunk in self.hunks:
            parts.append(hunk.header)
            parts.extend(hunk.lines)
        return "\n".join(parts)


class DiffBundle(StrictModel):
    base_sha: str
    head_sha: str
    files: list[FilePatch] = Field(default_factory=list)
    patch_text: str = ""
    truncated: bool = False

    def file(self, path: str) -> FilePatch | None:
        for f in self.files:
            if f.path == path or f.old_path == path:
                return f
        return None

    def changed_paths(self) -> list[str]:
        return [f.path for f in self.files]

    def changed_line_map(self) -> dict[str, set[int]]:
        return {f.path: f.changed_lines for f in self.files if f.changed_lines}


class SymbolView(StrictModel):
    """A caller/callee/type discovered outside the diff (1 hop)."""

    name: str = Field(min_length=1)
    kind: Literal["function", "method", "class", "type", "import", "call", "constant"] = "function"
    path: str
    line: int = Field(default=1, ge=1)
    signature: str = ""
    hop: int = Field(default=1, ge=0, le=2)
    changed: bool = False
    unresolved: bool = False


class ContextUnit(StrictModel):
    kind: Literal["diff", "signature", "type", "convention", "precedent", "critical_token", "file_head"]
    source: str
    text: str
    tokens: int = Field(ge=0)
    priority: int = Field(ge=0, le=100)
    reason: str = ""


class ContextSlice(StrictModel):
    text: str
    tokens: int = Field(ge=0)
    budget: int = Field(ge=1)
    units: list[ContextUnit] = Field(default_factory=list)
    dropped_units: int = 0
    unresolved_symbols: list[str] = Field(default_factory=list)
    conventions_loaded: list[str] = Field(default_factory=list)
    precedents_loaded: list[str] = Field(default_factory=list)
    input_sha256: str = ""
    input_bytes: int = 0
    truncated: bool = False
    material_truncated: bool = False


class DesignPrecedent(StrictModel):
    id: str = Field(min_length=1, max_length=200)
    problem: str = ""
    trigger: list[str] = Field(default_factory=list)
    options: list[str] = Field(default_factory=list)
    decision: str = ""
    rationale: str = ""
    applies_to: list[str] = Field(default_factory=list)
    preconditions: list[str] = Field(default_factory=list)
    source: str = ""


class ConventionDirective(StrictModel):
    source: str
    line: int = Field(default=1, ge=1)
    text: str
    kind: Literal["must", "never", "prefer", "allow"] = "must"
    applies_to: list[str] = Field(default_factory=list)


# --------------------------------------------------------------------------- #
# Stage 1: gates
# --------------------------------------------------------------------------- #


class CheckRunV1(StrictModel):
    name: str
    sha: str
    status: str = "completed"
    conclusion: CheckConclusion = CheckConclusion.UNKNOWN
    app: str = ""


class ApprovalV1(StrictModel):
    review_id: str = ""
    reviewer: str = ""
    sha: str = ""
    state: Literal["approved", "changes_requested", "commented", "dismissed"] = "commented"
    dismissed: bool = False
    stale: bool = False


class StackedBranch(StrictModel):
    branch: str
    sha: str
    role: Literal["base", "head", "intermediate"] = "intermediate"


class GateSnapshot(StrictModel):
    repository: str = ""
    pull_number: int = Field(default=0, ge=0)
    base_sha: str
    head_sha: str
    expected_head_sha: str = ""
    head_is_ancestor_of_branch: bool = True
    checks: list[CheckRunV1] = Field(default_factory=list)
    approvals: list[ApprovalV1] = Field(default_factory=list)
    stacked: list[StackedBranch] = Field(default_factory=list)
    actor_login: str = ""
    bot_login: str = ""
    required_checks: list[str] = Field(default_factory=list)


class GateResult(StrictModel):
    decision: GateDecision
    reasons: list[str] = Field(default_factory=list)
    effective_head: str = ""
    discounted_approvals: list[str] = Field(default_factory=list)
    missing_checks: list[str] = Field(default_factory=list)
    failing_checks: list[str] = Field(default_factory=list)
    stale_branches: list[str] = Field(default_factory=list)


class MciClaim(StrictModel):
    claim: str = Field(min_length=1, max_length=600)
    symbols: list[str] = Field(default_factory=list)
    supported: bool = False
    evidence: str = ""


class MciResult(StrictModel):
    claims: list[MciClaim] = Field(default_factory=list)
    contradiction: bool = False
    summary: str = ""
    stripped_description: bool = True


# --------------------------------------------------------------------------- #
# Stage 2: static, neuro-symbolic, RALE
# --------------------------------------------------------------------------- #


class AnalyzerFinding(StrictModel):
    analyzer: str
    rule_id: str
    path: str
    line: int = Field(ge=1)
    end_line: int | None = None
    message: str = ""
    severity: int = Field(default=1, ge=0, le=3)
    track: Literal["pattern", "taint", "lifetime"] = "pattern"
    cwe: str = ""
    patch_caused: bool = True
    suppressed: bool = False
    suppression_reason: str = ""
    test_context: bool = False


class TaintTuple(StrictModel):
    source: CitedSite
    sink: CitedSite
    rule_id: str = ""


class LifetimeTuple(StrictModel):
    alloc: CitedSite | None = None
    free: CitedSite | None = None
    use: CitedSite | None = None
    rule_id: str = ""


class LsegObligation(StrictModel):
    kind: Literal["separated", "assigns_from", "taint", "lifetime"]
    subjects: list[str] = Field(default_factory=list)
    detail: str = ""


class CpgFrame(StrictModel):
    path: str
    function: str = ""
    taint: list[TaintTuple] = Field(default_factory=list)
    lifetime: list[LifetimeTuple] = Field(default_factory=list)
    memory_roots: list[str] = Field(default_factory=list)
    alias_pairs: list[tuple[str, str]] = Field(default_factory=list)
    write_sets: list[str] = Field(default_factory=list)
    obligations: list[LsegObligation] = Field(default_factory=list)
    language: str = ""


class SecSemScan(StrictModel):
    monologue: str = Field(min_length=1, max_length=8000)
    cwe: str = Field(default="", max_length=32)
    verdict: Literal["VULNERABLE", "SAFE"] = "SAFE"
    cited_lines: list[CitedSite] = Field(default_factory=list, max_length=40)
    taint: list[TaintTuple] = Field(default_factory=list, max_length=20)
    lifetime: list[LifetimeTuple] = Field(default_factory=list, max_length=20)
    confidence: float = Field(default=0.5, ge=0.0, le=1.0)


class Reconciliation(StrictModel):
    tier: ReconciliationTier
    matched_lines: list[int] = Field(default_factory=list)
    detail: str = ""


class RaleResult(StrictModel):
    path: str
    function: str = ""
    line: int = Field(default=1, ge=1)
    ast_flag: bool = False
    cfg_flag: bool = False
    code_flag: bool = False
    ast_reasons: list[str] = Field(default_factory=list)
    cfg_reasons: list[str] = Field(default_factory=list)
    code_score: float = Field(default=0.0, ge=0.0, le=1.0)
    flagged: bool = False
    classifier: str = ""


# --------------------------------------------------------------------------- #
# Stage 3: probes
# --------------------------------------------------------------------------- #


class ProbeSpec(StrictModel):
    test_path: str = Field(min_length=1, max_length=1024)
    content: str = Field(min_length=1, max_length=200_000)
    target_symbols: list[str] = Field(default_factory=list)
    framework: Literal["pytest", "unittest"] = "pytest"
    rationale: str = ""


class ProbeResult(StrictModel):
    status: ProbeStatus
    exit_code: int | None = None
    stdout: str = ""
    stderr: str = ""
    failure_kind: str = ""
    assertion_message: str = ""
    duration_ms: int = Field(default=0, ge=0)
    container_image: str = ""
    input_sha256: str = ""

    @property
    def mechanical(self) -> bool:
        return self.status in (ProbeStatus.PASS, ProbeStatus.FAIL_BEHAVIORAL)


class ProbePair(StrictModel):
    base: ProbeResult
    head: ProbeResult
    classification: ProbeClassification
    detail: str = ""


# --------------------------------------------------------------------------- #
# Stage 4: falsification
# --------------------------------------------------------------------------- #


class Counterfactual(StrictModel):
    intervention: Literal["add_guard", "remove_guard", "noop"]
    risk_before: float = Field(ge=0.0, le=1.0)
    risk_after: float = Field(ge=0.0, le=1.0)
    applied: bool = False
    detail: str = ""

    @property
    def direction_ok(self) -> bool:
        if self.intervention == "add_guard":
            return self.risk_after < self.risk_before
        if self.intervention == "remove_guard":
            return self.risk_after > self.risk_before
        return abs(self.risk_after - self.risk_before) < 1e-9


class OblivulResult(StrictModel):
    passed: bool = False
    positive: Counterfactual | None = None
    negative: Counterfactual | None = None
    obligations: list[LsegObligation] = Field(default_factory=list)
    detail: str = ""


class Fix(StrictModel):
    patch: str = Field(min_length=1, max_length=400_000)
    files_changed: list[str] = Field(default_factory=list)
    applies_to_sha: str = ""
    verified: bool = False
    compile_ok: bool = False
    lint_ok: bool = False
    defect_removed: bool = False
    noop_outperformed: bool = False
    checks: list[str] = Field(default_factory=list)
    private: bool = False


class CiverResult(StrictModel):
    passed: bool = False
    fix: Fix | None = None
    semantic_noop_signal: bool = False
    detail: str = ""
    attempts: int = Field(default=0, ge=0)


# --------------------------------------------------------------------------- #
# Stage 5: risk-only elicitation, monitor, fusion
# --------------------------------------------------------------------------- #


class RiskElicitation(StrictModel):
    """The exact policy-free contract.  Six fields, nothing else.

    Deliberately absent: any cost, ratio, threshold, or ACCEPT/REJECT field.
    The decision is computed in host code from ``failure_probability``.
    """

    defect_present: bool
    category: str
    severity_score: int = Field(ge=0, le=3)
    failure_probability: float = Field(ge=0.0, le=1.0)
    cited_sites: list[CitedSite] = Field(default_factory=list, max_length=10)
    root_cause: str = Field(min_length=1, max_length=2000)

    @model_validator(mode="after")
    def _cited_when_present(self) -> "RiskElicitation":
        if self.defect_present and not self.cited_sites:
            raise ValueError("defect_present requires at least one cited site")
        return self


class MonitorVote(StrictModel):
    score: float = Field(ge=0.0, le=1.0)
    rationale: str = Field(default="", max_length=2000)


class MonitorScore(StrictModel):
    score: float = Field(ge=0.0, le=1.0)
    votes: list[MonitorVote] = Field(default_factory=list)
    dispersion: float = Field(default=0.0, ge=0.0, le=1.0)
    model: str = ""
    independent: bool = True


class FusionResult(StrictModel):
    p: float = Field(ge=0.0, le=1.0)
    monitor: float = Field(ge=0.0, le=1.0)
    p_combined: float = Field(ge=0.0, le=1.0)
    tau: float = Field(gt=0.0, lt=1.0)
    decision: Decision
    reasons: list[str] = Field(default_factory=list)


# --------------------------------------------------------------------------- #
# Findings and the final verdict
# --------------------------------------------------------------------------- #


class Evidence(StrictModel):
    kind: Literal["analyzer", "cpg", "probe", "perturbation", "classifier", "llm_hypothesis"]
    analyzer: str = ""
    rule_id: str = ""
    trace: list[CitedSite] = Field(default_factory=list)
    probe: ProbePair | None = None
    frames: list[CpgFrame] = Field(default_factory=list)
    detail: str = ""


class Verification(StrictModel):
    kind: VerificationKind = VerificationKind.UNVERIFIED
    detail: str = ""
    probe: ProbePair | None = None
    probe_spec: ProbeSpec | None = None
    counterfactual: OblivulResult | None = None
    civer: CiverResult | None = None


class Candidate(StrictModel):
    id: str
    stage: StageId
    category: Category = Category.OTHER
    title: str = Field(min_length=1, max_length=300)
    root_cause: str = Field(default="", max_length=4000)
    severity: int = Field(default=1, ge=0, le=3)
    cited_sites: list[CitedSite] = Field(default_factory=list)
    cwe: str = ""
    evidence: Evidence
    verification: Verification = Field(default_factory=Verification)
    p: float | None = Field(default=None, ge=0.0, le=1.0)
    monitor: MonitorScore | None = None
    p_combined: float | None = Field(default=None, ge=0.0, le=1.0)
    status: FindingStatus = FindingStatus.UNVERIFIED
    suppression_reason: str = ""
    security_exploitable: bool = False
    public_safe: bool = True
    private_detail: str = ""
    verified_fix: Fix | None = None
    design_precedents: list[str] = Field(default_factory=list)


class Finding(StrictModel):
    id: str
    title: str
    category: Category
    severity: int = Field(ge=0, le=3)
    root_cause: str
    cited_sites: list[CitedSite] = Field(default_factory=list)
    cwe: str = ""
    evidence_summary: str = ""
    verification: VerificationKind
    failure_probability: float = Field(ge=0.0, le=1.0)
    p_combined: float = Field(ge=0.0, le=1.0)
    suggestion: str = ""
    suggestion_patch: str = ""
    suggestion_start_line: int | None = Field(default=None, ge=1)
    suggestion_end_line: int | None = Field(default=None, ge=1)
    redacted: bool = False
    private_detail_id: str = ""


class Redaction(StrictModel):
    redacted: bool = False
    reason: str = ""
    public_text: str = ""
    private_record_id: str = ""
    cwes: list[str] = Field(default_factory=list)


class Receipt(StrictModel):
    attempt_id: str
    review_id: str
    stage: str
    status: AttemptStatus
    started_at: str
    finished_at: str
    exit_code: int | None = None
    input_sha256: str = ""
    input_bytes: int = 0
    output_sha256: str = ""
    output_bytes: int = 0
    model: str = ""
    tokens_in: int = 0
    tokens_out: int = 0
    cancellation_point: str = ""
    detail: str = ""
    prev_hash: str = ""
    hash: str = ""


class StageReport(StrictModel):
    stage: StageId
    status: AttemptStatus
    candidates: list[Candidate] = Field(default_factory=list)
    suppressed: list[Candidate] = Field(default_factory=list)
    degraded: list[str] = Field(default_factory=list)
    elapsed_ms: int = Field(default=0, ge=0)
    detail: str = ""


class ReviewVerdict(StrictModel):
    review_id: str
    repository: str = ""
    pull_number: int = 0
    base_sha: str
    head_sha: str
    decision: Decision
    p_combined: float = Field(default=0.0, ge=0.0, le=1.0)
    tau: float = Field(gt=0.0, lt=1.0)
    findings: list[Finding] = Field(default_factory=list, max_length=3)
    suppressed_count: int = Field(default=0, ge=0)
    redacted_count: int = Field(default=0, ge=0)
    receipts: list[Receipt] = Field(default_factory=list)
    degraded: list[str] = Field(default_factory=list)
    elapsed_ms: int = Field(default=0, ge=0)
    budget_exceeded: bool = False
    mci: MciResult | None = None
    gate: GateResult | None = None
    private_log_path: str = ""
    input_sha256: str = ""
    schema_version: int = 1


# --------------------------------------------------------------------------- #
# LLM answer contracts beyond the final elicitation
# --------------------------------------------------------------------------- #


class ExecResult(StrictModel):
    argv: list[str] = Field(default_factory=list)
    exit_code: int | None = None
    stdout: str = ""
    stderr: str = ""
    duration_ms: int = Field(default=0, ge=0)
    status: AttemptStatus = AttemptStatus.COMPLETED



class TriageFocus(StrictModel):
    path: str = Field(min_length=1, max_length=1024)
    function: str = Field(default="", max_length=512)
    reason: str = Field(default="", max_length=600)
    priority: int = Field(default=1, ge=0, le=3)


class TriagePlan(StrictModel):
    focus: list[TriageFocus] = Field(default_factory=list, max_length=20)
    search_symbols: list[str] = Field(default_factory=list, max_length=40)
    risk_hypotheses: list[str] = Field(default_factory=list, max_length=10)
    budget_note: str = Field(default="", max_length=400)


class MciScan(StrictModel):
    """What the PR description *claims*, before host-side grounding."""

    claims: list[MciClaim] = Field(default_factory=list, max_length=20)
    summary: str = Field(default="", max_length=600)


class CandidateDraft(StrictModel):
    """One behavioural defect hypothesis tied to changed lines.

    A draft is not a finding: the host keeps only drafts whose cited sites are
    inside the diff, and Stage 3/4 must verify the behaviour mechanically.
    """

    title: str = Field(min_length=1, max_length=300)
    category: Category = Category.CORRECTNESS
    severity: int = Field(default=2, ge=0, le=3)
    cited_sites: list[CitedSite] = Field(min_length=1, max_length=5)
    root_cause: str = Field(min_length=1, max_length=2000)
    suspected_trigger: str = Field(default="", max_length=600)


class BehavioralCandidates(StrictModel):
    candidates: list[CandidateDraft] = Field(default_factory=list, max_length=3)


class FallbackVerdict(StrictModel):
    verdict: Literal["accept", "reject", "uncertain"]
    confidence: float = Field(default=0.5, ge=0.0, le=1.0)
    rationale: str = Field(default="", max_length=2000)
    cited_sites: list[CitedSite] = Field(default_factory=list, max_length=10)


class PatchProposal(StrictModel):
    patch: str = Field(min_length=1, max_length=200_000)
    files_changed: list[str] = Field(default_factory=list, max_length=20)
    rationale: str = Field(default="", max_length=1200)


class GuardProposal(StrictModel):
    """A minimal LSOG intervention (add or remove one guard)."""

    path: str = Field(min_length=1)
    line: int = Field(ge=1)
    kind: Literal["add", "remove"]
    code: str = Field(default="", max_length=2000)
    guard_symbol: str = Field(default="", max_length=200)
    rationale: str = Field(default="", max_length=600)


# --------------------------------------------------------------------------- #
# Helpers
# --------------------------------------------------------------------------- #


def canonical_json(value: Any) -> str:
    """Deterministic JSON used for hashing receipts."""
    if isinstance(value, BaseModel):
        value = value.model_dump(mode="json")
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def sha256_hex(data: bytes | str) -> str:
    if isinstance(data, str):
        data = data.encode("utf-8")
    return hashlib.sha256(data).hexdigest()


def clamp01(value: float) -> float:
    return _clamp(float(value), 0.0, 1.0)
