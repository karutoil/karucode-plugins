# Internal interfaces — read before writing code

Python 3.13. Runtime dependencies: **pydantic v2**, **httpx**. Tests: **pytest**.
Everything else is the standard library.

## Ground rules for every contributor

1. Own only the files named for your task. Never edit `models.py`, `config.py`,
   `gitops.py`, `llm/*`, `errors.py`, `stage1/contract.py`, `stage1/receipts.py`
   or another agent's files. If a shared change is needed, report it instead.
2. Import models with `from ..models import ...` (or `from zero_loop_reviewer.models import ...`
   in tests). Never re-declare a schema that exists in `models.py`.
3. Never call a real model. Use `tests.support.llm.ScriptedLLM` / `ConstantLLM`.
   Tests must be deterministic and offline.
4. Every failure is typed: raise `LLMError(kind=...)`, `SandboxError`,
   `ToolError` or return a status from the models. **Never** turn a failure into
   "no findings"; an unverifiable stage degrades to `human_review`.
5. All public functions are pure where possible and take explicit inputs.
   No module-level mutable state, no hidden clock, no `os.environ` reads in a
   stage function (take `environ` or config as an argument).
6. Tests live in `tests/test_<area>.py`, are run from the plugin root with
   `python3 -m pytest tests/test_<area>.py -q`, and must pass before you report.
   Prefer real tools over mocks where one exists (`git`, `semgrep`, `podman`,
   `python`); mock only HTTP/model transport.
7. Report: files written, exact commands run, exact test result line, and any
   interface you could not satisfy. Under 1500 bytes. No "should work".

## Shared facts

* `Settings` (`config.py`) carries `max_model_tokens`, `deadline_s`,
  `stage2_timeout_s`, `sandbox`, `sast`, `fusion`, `gate`, `paths`.
* `DiffBundle.files[*]` has `.path`, `.hunks`, `.changed_lines`,
  `.is_new/.is_deleted`.
* `CitedSite(path, line, end_line, symbol, snippet)`.
* `Candidate` is the unit every stage produces and consumes. It has
  `id, stage, category, title, root_cause, severity(0-3), cited_sites, cwe,
  evidence, verification, p, monitor, p_combined, status, verified_fix...`.
  Construct with `Candidate(id=..., stage=StageId.SAST, ...)`; `stage` is a
  `StageId` enum value (`"2a"`, `"2b"`, `"2c"`, `"3"`, `"4"`, `"5"`).
* `ContextSlice.text` is the compressed evidence block; `ContextSlice.tokens`
  is its size; `.budget` the ceiling.
* `LLM` protocol: `complete(stage, prompt, schema, *, stage_model=None,
  temperature=None, instance="reviewer") -> ModelAnswer`; `ModelAnswer.value`
  is the validated schema instance.
* `ExecResult(argv, exit_code, stdout, stderr, duration_ms, status)`.
* Paths under `settings.paths.work_dir` are real host paths. **podman cannot
  see `/tmp` in this environment**; scratch dirs must live under `/home`
  (the state dir default does).
* `run_bounded(...)` in `stage1/contract.py` is the only subprocess runner for
  host commands: it enforces timeouts, cancellation points, read-only digests
  and typed outcomes. Do not use bare `subprocess.run` for analyzers.

## Owned modules and their exact entry points

### Stage 1 (agent: stage1)
`zero_loop_reviewer/stage1/gatekeeper.py`
```python
def evaluate_gate(snapshot: GateSnapshot, config: GateConfig) -> GateResult
class HeadCommitGate:
    def __init__(self, provider, config: GateConfig, *, stacked: list[StackedBranch] = ...)
    def collect(self, repository: str, number: int, *, expected_head_sha: str = "", actor_login: str = "") -> GateSnapshot
    def evaluate(self, repository: str, number: int, *, expected_head_sha: str = "", actor_login: str = "") -> GateResult
```
Rules: expected head == actual head; head is ancestor of every intermediate
stacked branch; every required check present with `SUCCESS` on the *exact* head
SHA; approvals whose `sha != head_sha` are discounted; dismissed approvals are
discounted; the bot may not approve its own PR (`actor_login == bot_login` is
refused). Reasons are human-readable strings.

`zero_loop_reviewer/stage1/pmi.py`
```python
def strip_author_text(text: str) -> str
def ast_facts(repo: GitRepo, diff: DiffBundle) -> dict          # AST of changed files at head
def check_consistency(description: str, facts: dict, llm, *, settings: Settings) -> MciResult
```
`check_consistency` may only use the LLM to *extract claims* (`pmi_prompt`,
`MciScan`). Grounding is host code: a claim is `supported` only when every
symbol it names exists in `facts` and at least one is a changed/added
definition or added call; otherwise `supported=False` and, when the claim
asserts behaviour the diff cannot produce (e.g. "removes X" while X is still
defined), `contradiction=True`. Never include the description in the result's
public summary beyond a short quote.

`zero_loop_reviewer/stage1/context.py`
```python
class TokenCounter:  # chars/4, floor 1
    def count(self, text: str) -> int
def critical_tokens(diff: DiffBundle) -> list[str]
def extract_symbols(repo: GitRepo, diff: DiffBundle, *, head: str, max_files: int = 2000) -> list[SymbolView]
def load_conventions(repo: GitRepo, head: str, settings: Settings) -> tuple[list[ConventionDirective], list[DesignPrecedent], list[str]]
def build_context(repo: GitRepo, diff: DiffBundle, settings: Settings, *, plan: TriagePlan | None = None) -> ContextSlice
```
`build_context` assembles `ContextUnit`s, sorts by priority, drops whole units
until `tokens <= settings.max_model_tokens`, sets `truncated/dropped_units`,
computes `input_sha256`/`input_bytes` over the final text. It must never see
the PR description. Conventions are read from the head commit (`gitops.show`),
not the working tree. 1-hop symbols: Python via `ast`; other languages via the
`ast-grep` CLI when available, otherwise a bounded regex fallback; unresolved
imports go in `unresolved_symbols`.

### Stage 2 (agent: stage2)
`zero_loop_reviewer/stage2/sast.py`
```python
@dataclass
class SastOutcome:
    findings: list[AnalyzerFinding]; status: AttemptStatus; detail: str; raw_sha256: str
def run_semgrep(repo_root: Path, diff: DiffBundle, settings: Settings, *, environ=None) -> SastOutcome
def patch_caused(findings, diff: DiffBundle) -> list[AnalyzerFinding]     # keeps only changed-line findings
def apply_suppressions(findings, diff, settings: SastConfig) -> list[AnalyzerFinding]
```
Use `semgrep --json --metrics=off --config <shipped rules or configured>
--include <changed paths>`, with `--timeout` and `run_bounded`. Parse
`results[*]` (`check_id`, `path`, `start.line`, `extra.message`,
`extra.metadata.cwe`). Baseline: rerun on the base worktree if one is supplied?
No — filter by changed lines, and put every pre-existing line in
`patch_caused=False` (never dropped silently).
Shipped rules live in `rules/semgrep-python.yml` (you may add to it).

`zero_loop_reviewer/stage2/cpg.py`
```python
def taint_and_lifetime(analyzer: list[AnalyzerFinding], policy: dict) -> tuple[list[TaintTuple], list[LifetimeTuple]]
def frame_projection(path: str, source: str, language: str) -> CpgFrame
def parse_policy(path: Path) -> dict
```
Policy file `rules/policy.json`: `{"rules": [{"id", "track": "taint"|"lifetime",
"cwe", "severity"}]}`. `frame_projection` is deterministic: Python AST for
roots/aliases/write sets; C/C++ regex-level roots (`*p`, `malloc`, `free`,
`memcpy`) plus LSOG obligations (`\separated`, `assigns ... \from`). It never
claims more than the source shows.

`zero_loop_reviewer/stage2/rale.py`
```python
def ast_view(source: str, path: str) -> tuple[bool, list[str], list[str]]   # flag, reasons, functions
def cfg_view(source: str, path: str) -> tuple[bool, list[str]]
def code_view(source: str, path: str, *, classifier: "LocalCodeClassifier | None") -> tuple[bool, float, str]
def rale(path: str, source: str, *, classifier=None) -> RaleResult
def ensemble(ast_flag: bool, cfg_flag: bool, code_flag: bool) -> bool   # (AST && CFG) || Code
```
AST flag: structural defects (bare except, mutable default, unreachable after
return, `except: pass`, assert-on-tuple, missing return branch). CFG flag:
missing return on a path, exception path skipping cleanup, loop with no exit.
`ensemble` is the required Boolean absorption and is tested exhaustively.

`zero_loop_reviewer/stage2/classifiers.py`
```python
class LocalCodeClassifier(Protocol):
    name: str
    def score(self, source: str, path: str) -> float
class TransformersCodeClassifier:  # lazy import; raises ToolError if unavailable
    ...
class HashingCodeClassifier:        # deterministic fallback, name "hashing-linear"
    ...
def default_classifier(settings: Settings) -> LocalCodeClassifier | None
```
The CodeBERT-125M / CodeGPT-124M pair is *retained* as the preferred
classifier when `transformers`+`torch` and a local model are available; when
they are not, report `ToolError` and let `code_view` return `(False, 0.0,
"unavailable")`. Never invent a model score.

`zero_loop_reviewer/stage2/secsem.py`
```python
def reconcile(scan: SecSemScan, analyzer: list[AnalyzerFinding], diff: DiffBundle) -> Reconciliation
def to_candidates(scan, reconciliation, frames, diff) -> list[Candidate]
def run_security(diff, context, analyzer, frames, llm, settings, *, policy) -> tuple[SecSemScan | None, Reconciliation | None, list[Candidate], str]
```
Tier-1: every `cited_lines` entry must exist in the analyzer trace (same path
and line ±2). No analyzer rule but a high-confidence contextual CWE →
`UNVERIFIED_TRUST`; the candidate must go to Stage 3/4, never straight to
output. A `SAFE` verdict yields no candidates, but is still recorded.

`zero_loop_reviewer/stage2/executor.py`
```python
@dataclass
class Stage2Outcome:
    candidates: list[Candidate]; findings: list[AnalyzerFinding]
    frames: list[CpgFrame]; rale: list[RaleResult]; scans: list[SecSemScan]
    degraded: list[str]; elapsed_ms: int
def run_stage2(repo_root: Path, repo: GitRepo, diff: DiffBundle, context: ContextSlice,
               llm, settings: Settings, *, environ=None) -> Stage2Outcome
```
Three tracks run concurrently (`asyncio.to_thread` or ThreadPoolExecutor) with
a hard `settings.stage2_timeout_s`; a failed track records a degraded reason
and does not abort the others. Total wall clock must be bounded.

### Stage 3 (agent: stage3)
`zero_loop_reviewer/stage3/probe.py`
```python
def synthesize(candidate: Candidate, context: ContextSlice, llm, settings, *,
               project_files: list[str], max_attempts: int = 3,
               validate: Callable[[ProbeSpec], str] | None = None) -> tuple[ProbeSpec | None, list[str]]
```
Up to 3 attempts with the previous failure fed back (Reflexion). Before
returning, verify `compile(content, "<probe>", "exec")` succeeds; a syntax error
is an attempt failure. Instructions must demand return/state assertions on
boundary inputs.

`zero_loop_reviewer/stage3/sandbox.py`
```python
@dataclass
class SandboxConfigView: ...   # built from Settings.sandbox
class Sandbox(Protocol):
    available(self) -> tuple[bool, str]
    def run_probe(self, worktree: Path, spec: ProbeSpec, *, expected_sha: str, timeout_s: float | None = None) -> ProbeResult
class PodmanSandbox: ...
class LocalSandbox: ...   # dev-only, documented as no security boundary
def build_sandbox(settings: Settings) -> Sandbox
```
Container invocation must include `--rm --network none --read-only
--cap-drop=ALL --security-opt no-new-privileges --memory=… --pids-limit=…
--uts=host`, a writable bind of a scratch copy (not the review worktree), and
a `sha256sum` verification of the probe file before execution (mismatch →
`INPUT_DELIVERY_FAILED`). Classification: `pass`, `fail_behavioral`,
`setup_error` (ImportError/ModuleNotFoundError/SyntaxError/fixture/container
error), `timeout`, `sandbox_unavailable`, `cancelled`. Never let a generated
test's setup error count against the PR.

`zero_loop_reviewer/stage3/cascade.py`
```python
def classify_pair(base: ProbeResult, head: ProbeResult) -> ProbePair
def run_cascade(candidate, context, llm, sandbox, repo: GitRepo, diff, settings,
                *, healthz=None) -> tuple[list[Candidate], list[ProbePair], list[str]]
def fallback_verify(candidate, context, llm) -> Candidate    # 5 votes, accept/reject/uncertain
```
Retention: `BOTH_FAIL` is a confirmed omission in the reviewed code and
`REGRESSION_CONFIRMED` is a confirmed regression — both become findings.
`FIX_CONFIRMED` (fails on base, passes on head) confirms the PR's fix and is
**suppressed**, never emitted as a defect: otherwise every bug-fix PR would be
blocked. `BOTH_PASS` is suppressed as not reproducible; setup errors on either
side stay `UNVERIFIED` and are discarded as evidence. A head-side behavioural
failure with a base setup error is still usable by CIVER, because a new
function legitimately cannot run on the base commit. Fallback: k=5
(`settings.llm.monitor.votes`) at temperature 0.8; a majority `accept` never
verifies on votes alone — it only verifies a candidate that already carries a
real perturbation, otherwise it stays `UNVERIFIED`. `reject` suppresses;
`uncertain` leaves it unverified.

### Stage 4 (agent: stage4)
`zero_loop_reviewer/stage4/civer.py`
```python
class CommandRunner(Protocol):
    def run(self, argv: list[str], *, cwd: Path, timeout_s: float) -> ExecResult
def propose_fix(candidate, context, llm) -> PatchProposal | None
def verify_fix(candidate, proposal, repo: GitRepo, head_sha: str, work_root: Path,
               runner: CommandRunner, settings, *, defect_probe=None) -> CiverResult
```
Checks, in order: patch applies (`git apply --check`), compiles
(`python -m compileall -q`), configured linter (only if present), the defect
signal disappears (probe passes on patched head, or the analyzer rule no longer
fires when a rerun callback is provided), and a semantic no-op perturbation
does **not** remove the signal. Any failure → `passed=False`, no suggestion.

`zero_loop_reviewer/stage4/oblivul.py`
```python
def build_lsog(frame: CpgFrame) -> list[LsegObligation]
def counterfactuals(candidate, frame, worktree, runner, settings, *, analysis_cmd: list[str] | None = None) -> OblivulResult
```
Positive intervention (add a guard) must lower measured risk; negative
(remove/weaken the guard) must raise it. Risk is a deterministic count of
reachable dangerous paths (or analyzer findings) recomputed by rerunning the
same analyzer command on a scratch copy. If both directions cannot be measured,
`passed=False` and the candidate is suppressed (not emitted).

`zero_loop_reviewer/stage4/designlore.py`
```python
def load_precedents(repo: GitRepo, head: str, settings: Settings) -> list[DesignPrecedent]
def load_directives(repo: GitRepo, head: str, settings: Settings) -> list[ConventionDirective]
def filter_candidate(candidate, precedents, directives, *, repo_root: Path) -> tuple[bool, str]
```
Suppress when a suggestion contradicts a matched precedent (path glob +
trigger) whose preconditions hold, or violates a `never`/`must` directive.
Return `(keep, reason)`; reasons go to the receipt, never to the public comment.

### Stage 5 (agent: stage5)
`zero_loop_reviewer/stage5/elicit.py`
```python
def elicit(candidate, context, llm, settings) -> tuple[RiskElicitation | None, str]
def assert_policy_free(prompt_text: str) -> None   # reuse llm.client
```
`risk_only_prompt` only. No retry that changes the question.

`zero_loop_reviewer/stage5/fusion.py`
```python
def monitor_scores(candidate, context, llm, monitor_llm, settings) -> MonitorScore
def combine(p: float, monitor: MonitorScore, config: FusionConfig) -> FusionResult
def decide(candidates: list[Candidate], elicitations: dict[str, RiskElicitation],
           monitors: dict[str, MonitorScore], config: FusionConfig,
           *, mechanically_confirmed: bool) -> tuple[Decision, float, list[str]]
def fuse(p: float, s: float, config: FusionConfig) -> float   # log-odds, clipped
```
`tau = c_fr / (c_fa + c_fr)`. `p_combined >= tau` OR a mechanically confirmed
defect → `REQUEST_CHANGES`. Disagreement wider than
`config.disagreement_threshold` → `HUMAN_REVIEW` unless mechanically
confirmed. Deterministic; no I/O.

`zero_loop_reviewer/stage5/redaction.py`
```python
def is_exploitable(candidate, config: RedactionConfig) -> bool
def redact(candidate, config, *, private_dir: Path) -> Redaction
def public_body(redaction: Redaction, candidate) -> str
```
Exploitable = security category or CWE in `config.exploitable_cwes`, with a
cited untrusted-input path (taint evidence or analyzer track). Public text
names the class and gate status only; no exploit mechanics, no line-by-line
bypass. Full diagnostic + verified patch go to `private_dir` (mode 0600).

`zero_loop_reviewer/stage5/formatter.py`
```python
def select(findings: list[Candidate], max_findings: int) -> list[Candidate]
def to_finding(candidate: Candidate, elicitation, monitor, p_combined, redaction) -> Finding
def render_review(verdict: ReviewVerdict, *, bot_login: str, max_body: int = 60000) -> ReviewPayload
def suggestion_block(fix: Fix) -> str     # ```suggestion with the verified patch
```
Top-3 hard cap, sorted by (severity, p_combined) descending. Every public
finding except a redacted security flag carries a CIVER-verified suggestion;
a finding without one is not emitted (`status=SUPPRESSED`). One consolidated
payload; no per-comment threads.

### Providers (agent: providers)
`zero_loop_reviewer/providers/{github,gitlab,bitbucket,webhook}.py`
Implement `GitProvider` (`base.py`) for all three, using `HttpProvider.request`.
Endpoints are listed in `ARCHITECTURE.md §3`. One review per push; no comment
thread reads. `webhook.py`:
```python
def verify(provider: str, secret: str, body: bytes, headers: Mapping[str,str]) -> bool
class WebhookServer:  # stdlib http.server, loopback by default
    def __init__(self, host, port, *, secret, provider, handler: Callable[[WebhookEvent], None])
    def start(self) -> None; def stop(self) -> None; @property port
```
GitHub: `X-Hub-Signature-256: sha256=<hmac>`; GitLab: `X-Gitlab-Token`
constant-time equality; Bitbucket: `X-Hub-Signature` HMAC. Replay window 300 s
when a timestamp header exists; de-duplicate `X-GitHub-Delivery` /
`X-Request-Id`. Return 202 for accepted, 401 for bad signature, 400 for
unparseable, 204 for ignored actions. Never echo the payload.

Tests: a real `http.server` on 127.0.0.1:0 with a handler that records events;
provider tests use an httpx `MockTransport` (or a local fake server) asserting
method, path, JSON body and headers.

## What the pipeline does with your output

`pipeline.py` calls the stages in order, records a `Receipt` for every attempt,
and degrades to `HUMAN_REVIEW` whenever a stage cannot verify. It never
publishes a candidate whose `verification.kind` is `UNVERIFIED`. Stage modules
therefore do not decide the merge outcome; they produce candidates, evidence
and explicit status.
