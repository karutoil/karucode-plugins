# zero-loop-reviewer

A karucode plugin that reviews a pull request once, grounds every claim, and
returns at most three findings with a verified patch — no follow-up review
loops.

Five stages run inside a 1–4 minute CI budget:

1. **Contract + context** — six-property runtime contract, exact-head gate,
   PR-description isolation, 1-hop symbols, conventions, a 500k-token context budget and 64k output.
2. **Parallel static/ensemble** — patch-caused Semgrep delta, a secSemScan
   security monologue reconciled against the analyzer trace, CPG frames, and
   the RALE `(AST && CFG) || Code` ensemble.
3. **Execution-grounded probes** — a reproduction test synthesised with up to
   three Reflexion attempts, executed against the base and head commits in a
   network-isolated container. Setup errors are discarded; omissions and
   regressions are retained.
4. **Falsification** — CIVER (a minimal repair must remove the defect signal,
   with a semantic no-op control), ObliVul (adding a guard must lower measured
   risk; removing one must not lower it), and DesignLore precedent checks.
5. **Coded controller** — policy-free risk elicitation, an independent k=5
   monitor, log-odds fusion and the Bayes threshold in host code, a configurable
   dynamic publication cap (all verified, safety ceiling 50),
   one-click suggestions, and AFCRA-style security redaction.

Every attempt writes an immutable, hash-chained receipt. A stage that cannot
verify degrades to **human review**; it never becomes "no findings".

## Install

```bash
cd kc-zero-loop-reviewer
./install.sh                 # karucode plugin install ./
# or: karucode plugin install /absolute/path/to/kc-zero-loop-reviewer
# or: copy this directory to <data_dir>/plugins/zero-loop-reviewer and rescan
```

On a machine where the installer's `/tmp` staging area is on a different
filesystem from the data directory (this one), `install.sh` falls back to a
direct self-contained copy, rewrites `command` to the installed path, and never
grants operator-only settings.

After changing the plugin's Python sources, reload that plugin
(`POST /api/plugins/zero-loop-reviewer/reload`, or restart the server): the host
fingerprints the launcher file, not every file in the package.

`plugin.toml` registers `bin/zero-loop-reviewer`, which needs Python 3.11+,
`pydantic>=2.6` and `httpx>=0.27`:

```bash
python3 -m pip install --user 'pydantic>=2.6' 'httpx>=0.27'
```

## Configure

Everything is operator data in **Settings → Plugins & MCP**. The important
values:

| Setting | Default | Meaning |
| --- | --- | --- |
| `model.primary` | `od/deepseek-v4.1-flash` | Model for every LLM stage |
| `model.{triage,security,probe,reviewer,monitor}` | empty | Per-stage override; empty = primary |
| `model.monitor_alt` | empty | Second-provider monitor model |
| `llm.base_url` | empty | OpenAI-compatible API root (`http://127.0.0.1:4000/v1`, a hosted endpoint, …) |
| `llm.api_key_env` | `ZERO_LOOP_LLM_API_KEY` | **Name** of the key variable; no secret is stored |
| `review.deadline_secs` | `240` | Hard review budget; set `3600` for a huge PR |
| `review.stage2_timeout_secs` | `60` | One parallel static track |
| `review.max_model_tokens` | `500000` | Context ceiling (tokens) |
| `review.max_findings` | `0` | `0` = every verified finding up to 50; or an explicit 1–50 cap |
| `fusion.c_fr` / `fusion.c_fa` | `10` / `1` | Coded Bayes τ = C_FR/(C_FA+C_FR) = 0.909; host-side only |
| `fusion.monitor_votes` | `5` | Independent monitor samples |
| `fusion.disagreement_threshold` | `0.55` | Wider reviewer/monitor gap → human review |
| `sandbox.runtime` | `podman` | `podman`, `docker`, `local` (dev only), `none` |
| `sandbox.image` | `python:3.12-alpine` | Must contain the project runtime |
| `sast.semgrep_config` | empty | Empty = the rules shipped in `rules/` |
| `gate.bot_login` | `zero-loop-reviewer[bot]` | Review identity |
| `gate.require_checks` | empty | Checks that must pass on the exact HEAD SHA |
| `gate.require_approval` | `false` | Turn on only for a merge gate, not a review trigger |
| `ci.provider` / `ci.token_env` | `none` / `GITHUB_TOKEN` | Git provider and **name** of its token variable |
| `state.dir` | `~/.local/state/zero-loop-reviewer` | Receipts, worktrees, private security log |

`model.max_tokens` (TOML/env, default `65536`) is the output ceiling for every
stage. Reasoning effort is per stage: `high` for security/reviewer, `medium`
for probe/monitor, `low` for triage. Override with
`model.{triage,security,probe,reviewer,monitor}_reasoning_effort` or the global
`model.reasoning_effort` in TOML/env; these are deliberately not host settings,
so the host's 32-setting limit stays intact.

Endpoint fallbacks when `llm.base_url` is empty:
`ZERO_LOOP_LLM_BASE_URL` → `LITELLM_BASE_URL` → `OPENAI_BASE_URL` →
`KARUCODE_BASE_URL`; key: `ZERO_LOOP_LLM_API_KEY` → `LITELLM_API_KEY` →
`OPENAI_API_KEY` → `DEEPSEEK_API_KEY` → `KARUCODE_API_KEY`.

No endpoint means no semantic review: the plugin returns **human review** with
the reason `llm_unavailable`, never an approval.

## Tools

| Tool | Class | What it does |
| --- | --- | --- |
| `review_pr` | shell | Runs all five stages; returns the verdict, findings and committable suggestions. `post=true` publishes one review. |
| `review_status` | read-only | Reads a stored verdict and verifies its receipt hash chain. |
| `explain_finding` | read-only | Reads the private diagnostic of a redacted security finding. |

Example (inside a project session):

```json
{"name": "review_pr", "arguments": {"base": "origin/main", "head": "HEAD"}}
```

A tool call takes 1–4 minutes and writes receipts under `state.dir/receipts/`.
The review body is also exposed as a panel, `plugin__zero-loop-reviewer__review_status`.

`post=true` needs `ci.provider`, `repository` and `pull_number`. An `approve`
decision is posted as a **COMMENT**, never an `APPROVE` review: a bot must not
satisfy a branch-protection approval.

## CI

### One-shot job

```bash
zero-loop-reviewer ci \
  --provider github --repository owner/repo --number 42 \
  --workspace "$GITHUB_WORKSPACE" \
  --payload "$GITHUB_EVENT_PATH" \
  --out-dir artifacts
```

Exit codes: `0` approve, `1` request changes, `2` human review / configuration
error. Set `GITHUB_TOKEN`, `GITLAB_TOKEN` or `BITBUCKET_TOKEN` according to
`ci.provider`, and `ZERO_LOOP_LLM_BASE_URL` + `ZERO_LOOP_LLM_API_KEY` for the
reviewer. `--no-post` runs the review without publishing.

The job must have the PR **head** checked out. The pipeline refuses a review
whose checkout does not match the event's `head_sha`; it does not fetch for you.

### Webhook receiver

```bash
zero-loop-reviewer webhook --provider github --workspace /srv/repo --port 8791
```

It verifies `X-Hub-Signature-256` (GitHub), `X-Gitlab-Token` (GitLab) or
`X-Hub-Signature` (Bitbucket), enforces a 300-second replay window, de-duplicates
delivery ids, answers the provider immediately (202/204/401/400) and reviews in a
background thread. Bind loopback; put a TLS reverse proxy in front. A
non-loopback bind without a secret is refused. The secret variable is
`ci.webhook_secret_env` (default `ZERO_LOOP_WEBHOOK_SECRET`).

## Security model

* Probes run with `--network none`, a read-only root, `--cap-drop=ALL`,
  `no-new-privileges`, memory/PID/CPU limits, and a writable bind of a
  throw-away copy — never the review worktree.
* The probe file's SHA-256 is recomputed inside the container before the probe
  runs. A one-byte difference aborts with `input_delivery_failed`.
* `local` sandbox mode runs probes with the reviewer's own privileges. It is a
  development convenience **and not a security boundary**.
* Analyzer commands are refused if they carry a write flag, and the workspace
  digest is checked after each bounded run.
* Exploitable security findings are redacted in public: only the CWE class and
  the gate status are posted; the trace and verified patch go to a `0600` record
  under `state.dir/private/`.
* The PR description and reply threads never enter the bug-hunting prompt. Only
  the dedicated PR-MCI check reads the description, and it extracts claims; it
  cannot change the review prompt.
* Receipts are hash-chained. `review_status` reports a broken chain instead of
  trusting it.

## Measured results

`./check.sh` runs everything below. On this machine:

* **444 tests pass** (real git, real Semgrep 1.173.0, real podman 5.4.2,
  a real OpenAI-compatible HTTP fake for the client tests).
* `python3 eval/run_eval.py` on the annotated corpus: **6/6 buggy cases
  intercepted, 0/4 clean or fix-confirming cases flagged**, in ~33 s for all ten cases.

That evaluation runs the deterministic half of the pipeline with *oracle*
candidate/probe/patch answers from the corpus annotation. It measures
verification, falsification and emission — not a live model's recall. The
93.5–100% objective is a design target; **no live-model recall number is claimed
here**, because no model endpoint is configured in this environment. To measure
it, set `ZERO_LOOP_LLM_BASE_URL`/`ZERO_LOOP_LLM_API_KEY` and run:

```bash
python3 eval/run_eval.py --provider live
```

The report is written to `eval/report.json`.

## Using it on the karucode repository

`.github/workflows/zero-loop-review.yml` runs this plugin on `pull_request`
events. It checks out the PR head with full history, the reviewer from
`karutoil/karucode-plugins` at a **pinned commit** (`ZERO_LOOP_PLUGIN_REF`), and
reviews `base..head` with `ci --payload`. The model endpoint comes from
`ZERO_LOOP_LLM_BASE_URL` / `ZERO_LOOP_MODEL`; the key is the
`ZERO_LOOP_LLM_API_KEY` Actions secret. Fork and Dependabot PRs are skipped:
GitHub does not give them repository secrets, and falling back to
`pull_request_target` would expose the key to untrusted code.

The shipped rule packs cover Python, Rust, TypeScript/JavaScript and GitHub
Actions YAML. CIVER parses the repaired file with `compileall`, `rustfmt`,
`node --check` or `esbuild` (installed from `web/package-lock.json`) before it
publishes a suggestion. A workflow-only PR with pinned actions therefore
approves with no comments; that is the intended result, not a failure to run.
Pullfrog and CodeRabbit can comment on a clean YAML diff because they publish
ungrounded LLM opinions; this reviewer only comments on a mechanically
verified finding or reports `human_review`.

## Large PRs

The diff is read up to an 8 MB safety ceiling and the context assembler may use
up to **500k tokens**; the model output ceiling is **64k tokens**. Behavioural
review is semantically batched: up to 100 size-bounded batches of 4 source
files / 30k characters, four in flight at once, and a batch never mixes two
change units. Large files are split at hunk boundaries; a file with one
enormous hunk is a declared blind spot unless it is a test file. Semgrep scans
the complete changed-file set in chunks of at most 200 paths, and up to 24
hypotheses reach the probe/CIVER stages, which are capped at 32 each. Every
verified finding is published up to the safety ceiling of 50;
`review.max_findings` can impose a lower explicit cap.

### Change-graph decomposition

`review.decomposition = true` (the default; TOML/env) builds a deterministic file graph
from changed-file imports, test/source pairing and feature-directory affinity.
Connected components become **change units** (capped at 12 files per unit,
split by feature when larger). Each behavioural batch carries its unit's title,
files and cross-unit edges; batches remain size-bounded, so this changes the
routing, not the transport.

When the graph has cross-unit edges, one **integration pass** reviews the
boundaries as well: changed signatures/contracts, API ↔ client, schema ↔
migration, shared types, feature flags and contradictory changes between units.
If the integration call fails, the result is a human-review blind spot, not an
approval. If the graph is empty or unusable, the current file-ranked batching
is the fallback.

On the 268-line test commit the current plan used 3 batches and 255 s; the
decomposed plan used 5 semantic batches and 118 s, with the same coverage
(151/151 lines) and the same 0 verified findings. That is a latency/locality
result, not evidence of better recall. A synthetic cross-file bug — a backend
response key renamed while a frontend consumer still indexed the old key — was
grouped into one unit, caught by the reproduction probe and published with a
verified patch in 60 s (2/2 changed lines covered). On the 9.6k-line PR #17 the
graph produced 21 units and 19 cross-unit edges, 29 batches, and still covered
6,492/6,492 production changed lines.

A second synthetic 14-file PR split one connected component into two units and
exercised the integration pass. The current file-batched mode published 1
finding in 101 s; the graph mode published **2** in 135 s, including the actual
`KeyError` in the consumer unit that the current mode missed, with 14/14 lines
covered in both. That is the first quality signal for the combined design, on a
small synthetic case rather than a labelled production corpus.

A 1,000–2,000 line PR usually takes one to three model calls. A 9k-line,
83-file PR is about 24 calls and can legitimately take 20–40 minutes; the
workflow budget is **3600 s** and its job timeout is 75 minutes. The default
`review.deadline_secs=240` remains for ordinary PRs, so a small PR is still
reviewed in a few minutes.

Exhaustive mode (`review.exhaustive = true`, the default) also means:

- every production changed line is assigned to a behavioural batch, and a
  missing assignment is a failed review rather than a silent omission;
- the full enclosing function of each changed site is added to the context,
  not just the diff hunk and its 1-hop signatures;
- an oversized single hunk is split at line boundaries, not dropped;
- an anchored hypothesis that exceeds the candidate cap is recorded as
  `behavioral_candidate_coverage_incomplete` and fails open to human review.

Coverage is allowed to be *incomplete*, never silently green. A truncated diff
or context, a capped batch scan, an oversized production-file hunk, or a
probe/CIVER cap adds a blind spot; if no finding is verified the verdict is
`human_review`. A verified finding still produces `request_changes` and a
committed suggestion. Generated/vendor/lock/doc files and findings inside test
modules are excluded from publication, but the rule packs still scan their
changed lines.

This is a **coverage** guarantee, not a recall guarantee. On the 9.6k-line
PR #17 it covered all **6,492 production changed lines** with zero missing
assignments and added 725 enclosing-function slices to the context. It still
cannot promise 99.9% bug recall: the model is probabilistic, verification can
reject a real but hard-to-reproduce defect, and publication still stops at the
safety ceiling of 50 rather than being unbounded.

## Why a clean diff has no comments

The pipeline separates *candidate generation* from *publication*. Triage,
security and behavioural prompts can produce ideas, but a finding is published
only when a failing probe, a patch-caused analyzer trace that disappears after
the repair, or a verified causal perturbation backs it. A model-only hypothesis
about a YAML file or an unruled Rust line has no mechanical falsifier, so it is
dropped (`behavioral_scan_filtered` / `unverified_candidates`) rather than
posted as noise. If the verified set is empty the check approves; if a severe
candidate cannot be verified it fails open to `human_review` with a receipt.

## Known limits

* Probe execution is Python-only.  Rust, TypeScript/JavaScript and GitHub
  Actions have patch-caused Semgrep rule packs and language-aware CIVER syntax
  checks, but there is no Rust/TS probe runner yet; a Rust/TS logic bug with no
  matching rule still ends in `human_review`, never a public comment.
* A finding needs a probe or an analyzer rerun to be emitted. A real defect
  whose reproduction cannot be synthesised becomes **human review**, not a
  public finding. That is deliberate: the zero-loop contract forbids an
  unverifiable suggestion.
* A PR that actually fixes a bug is approved when its reproduction fails on the
  base and passes on the head. That pair is recorded as `fix_confirmed` and
  suppressed; it is evidence for the fix, never a defect to request.
* ObliVul's risk model counts unguarded sinks. It is a causality check, not a
  semantic sanitizer proof; CIVER remains the stronger gate.
* GitLab/Bitbucket approvals do not carry a commit SHA. The gate reports them
  as discounted instead of pretending they cover HEAD.
* This environment needs `--uts=host` for podman (already the default in
  `extra_args`) and cannot bind-mount `/tmp`; keep `state.dir` under `/home`.
* The publication cap is dynamic: `0` (the default) publishes every verified
  finding up to the safety ceiling of 50. An explicit 1–50 value caps it
  lower. Findings beyond the effective cap are recorded as suppressed with a
  `publication cap` reason; the decision still reflects the worst finding.
  Every extra finding costs another inline comment and another verified
  suggestion.
* A single-hunk repair is rendered as a native `suggestion` block (one click). A
  multi-hunk repair is rendered as a verified `diff` block, because a provider
  suggestion covers exactly one line range.
* `review_pr` runs one review at a time per invocation. The webhook receiver
  serialises reviews with a lock.

## Tests

```bash
./check.sh
python3 -m pytest tests -q
python3 -m pytest tests -m 'not integration' -q   # no podman
python3 eval/run_eval.py
```

Fault injection covers truncated input at 1%, 50% and one byte short;
cancellation at input-write and output-read; timeout kills; nonzero exit with a
valid JSON payload (`process_crash`); empty and malformed output; write-flag and
workspace-mutation refusal; and receipt tampering.
