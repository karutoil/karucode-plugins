# zero-loop-reviewer

A karucode plugin that reviews a pull request once, grounds every claim, and
returns at most three findings with a verified patch — no follow-up review
loops.

Five stages run inside a 1–4 minute CI budget:

1. **Contract + context** — six-property runtime contract, exact-head gate,
   PR-description isolation, 1-hop symbols, conventions, a 128k-token budget.
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
   monitor, log-odds fusion and the Bayes threshold in host code, top-3 cap,
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
| `review.deadline_secs` | `240` | Hard review budget |
| `review.stage2_timeout_secs` | `60` | One parallel static track |
| `review.max_model_tokens` | `128000` | Compressed context ceiling |
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

* **416 tests pass** (real git, real Semgrep 1.173.0, real podman 5.4.2,
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

## Known limits

* Deep verification is Python-first. Semgrep, ast-grep and the CPG-lite views
  handle other languages, but probe execution and the RALE AST/CFG views are
  Python.
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
* The top-3 cap is hard. More than three verified findings means three are
  published and the rest are recorded as suppressed in the receipt; the
  decision still reflects the worst finding.
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
