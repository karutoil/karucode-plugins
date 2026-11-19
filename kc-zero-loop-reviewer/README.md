# zero-loop-reviewer

A karucode plugin that reviews a pull request **once**, grounds every claim in
code evidence, and publishes only findings a mechanical check has confirmed —
no follow-up review loops, no ungrounded model opinions.

A review publishes every mechanically verified finding up to a configurable cap
(`review.max_findings`, default `0` = every verified finding, hard ceiling 50).
Each published finding carries a CIVER-verified patch rendered as a one-click
suggestion.

Five stages run inside a CI budget (240 s by default; the karucode workflow
allows 3600 s for hour-scale pull requests):

1. **Contract + context** — a six-property runtime contract, the exact-head
   gate (including stacked branches), PR-description isolation, 1-hop symbols,
   conventions, full enclosing functions of changed lines, a 500k-token context
   budget and a 64k-token output ceiling. A deterministic change graph turns a
   large diff into semantic units.
2. **Parallel static/ensemble** — patch-caused Semgrep delta, a secSemScan
   security monologue reconciled against the analyzer trace, CPG frames, and
   the RALE `(AST && CFG) || Code` ensemble. Behavioural hypotheses are generated
   in unit-aware, size-bounded batches; a cross-unit integration pass reviews
   the boundaries between units.
3. **Execution-grounded probes** — a reproduction test in the candidate's
   language (Python, Rust, JavaScript/TypeScript) synthesised with up to three
   Reflexion attempts, executed against the base and head commits in a
   network-isolated container. Setup errors are discarded; omissions and
   regressions are retained.
4. **Falsification** — CIVER (a minimal repair must remove the defect signal,
   with a semantic no-op control), ObliVul (adding a guard must lower measured
   risk; removing one must never lower it), and DesignLore precedent checks.
5. **Coded controller** — policy-free risk elicitation, an independent k=5
   monitor, log-odds fusion and the coded-Bayes threshold in host code, the
   configurable publication cap, one-click suggestions, and tiered security
   redaction.

Every attempt writes an immutable, hash-chained receipt. A stage that cannot
verify degrades to **human review**; it never becomes "no findings".

## Install

Requires Python 3.11+ and two runtime dependencies:

```bash
python3 -m pip install --user 'pydantic>=2.6' 'httpx>=0.27'
```

Then install the plugin one of three ways:

```bash
cd kc-zero-loop-reviewer
./install.sh                 # karucode plugin install ./ with a direct-copy fallback
# or: karucode plugin install /absolute/path/to/kc-zero-loop-reviewer
# or: copy this directory to <data_dir>/plugins/zero-loop-reviewer and rescan
```

`install.sh` tries `karucode plugin install` first. On a machine where the
installer's `/tmp` staging area is on a different filesystem from the data
directory (this one), it falls back to a direct self-contained copy, rewrites
`command` to the installed path, and never grants operator-only settings.

`plugin.toml` registers `bin/zero-loop-reviewer`, a launcher that only sets
`PYTHONPATH` and runs `python3 -m zero_loop_reviewer`.

After changing the plugin's Python sources, reload that plugin
(`POST /api/plugins/zero-loop-reviewer/reload`, or restart the server): the host
fingerprints the launcher file, not every file in the package, so a Python edit
alone does not look like an upgrade.

## Configure

Configuration has four layers, highest precedence first:

1. an explicit environment override `ZERO_LOOP_<KEY>` (dots → underscores,
   upper case), e.g. `ZERO_LOOP_FUSION_C_FR=25`;
2. host settings — **Settings → Plugins & MCP**, delivered through
   `KARUCODE_PLUGIN_SETTINGS` / `plugin/settings/get`;
3. a TOML file passed with `--config` (CI mode);
4. the defaults below.

Secrets are never read from settings: only the **name** of an environment
variable is configured, and its value is read at call time. A settings key that
would store a credential is refused.

### Host settings (declared, editable in the UI)

The plugin declares exactly 32 settings — the host's per-plugin limit — so the
long tail below lives in TOML/environment instead.

| Setting | Default | Meaning |
| --- | --- | --- |
| `model.primary` | `od/deepseek-v4.1-flash` | Model for every LLM stage |
| `model.{triage,security,probe,reviewer,monitor}` | empty | Per-stage override; empty = primary |
| `model.monitor_alt` | empty | Second-provider monitor model |
| `llm.base_url` | empty | OpenAI-compatible API root (`http://127.0.0.1:4000/v1`, a hosted endpoint, …) |
| `llm.api_key_env` | `ZERO_LOOP_LLM_API_KEY` | **Name** of the key variable; no secret is stored |
| `llm.timeout_secs` | `120` | Hard deadline for one model call |
| `review.deadline_secs` | `240` | Whole-review budget; set `3600` for a huge PR |
| `review.stage2_timeout_secs` | `60` | Wall clock for the static/ensemble stage (all three tracks) |
| `review.max_model_tokens` | `500000` | Context ceiling (tokens) |
| `review.probe_attempts` | `3` | Reflexion attempts per candidate (1–3) |
| `review.max_findings` | `0` | `0` = every verified finding up to the ceiling of 50; or an explicit 1–50 cap |
| `fusion.c_fr` / `fusion.c_fa` | `10` / `1` | Coded Bayes τ = C_FR/(C_FA+C_FR) = 0.909; host-side only |
| `fusion.monitor_votes` | `5` | Independent monitor samples (k) |
| `fusion.monitor_temperature` | `0.8` | Deliberately different from the reviewer's deterministic setting |
| `fusion.disagreement_threshold` | `0.55` | Wider reviewer/monitor gap → human review |
| `sandbox.runtime` | `podman` | `podman`, `docker`, `local` (dev only), `none` |
| `sandbox.image` | `docker.io/library/python:3.12-alpine` | Python probe image; must contain `pytest`/`unittest` and the project runtime |
| `sandbox.timeout_secs` | `120` | One base-or-head execution |
| `sast.semgrep_config` | empty | Empty = the rules shipped in `rules/` |
| `gate.bot_login` | `zero-loop-reviewer[bot]` | Review identity |
| `gate.require_checks` | empty | Checks that must pass on the exact HEAD SHA |
| `gate.require_approval` | `false` | Turn on only for a merge gate, not a review trigger |
| `gate.cross_provider_audit` | `true` | Refuse to launch when the reviewer key is the authoring agent's key |
| `redaction.enabled` | `true` | Public comments get a gate status; the diagnostic stays private |
| `ci.provider` | `none` | `github`, `gitlab`, `bitbucket` or `none` |
| `ci.token_env` | `GITHUB_TOKEN` | **Name** of the Git token variable |
| `state.dir` | empty | Receipts, worktrees, private security log; empty = `~/.local/state/zero-loop-reviewer` |

### TOML/environment-only keys

These are deliberately not host settings, so the 32-key limit stays intact.
Set them in `--config` TOML or as `ZERO_LOOP_*`:

| Key | Default | Meaning |
| --- | --- | --- |
| `model.max_tokens` | `65536` | Output ceiling for every stage |
| `model.reasoning_effort` | `high` | Global default; per stage `triage` `low`, `security` and `reviewer` `high`, `probe` and `monitor` `medium` via `model.<stage>_reasoning_effort` |
| `llm.max_retries` / `llm.response_format` | `2` / `true` | Transport retries; request a JSON object when the endpoint supports it |
| `review.exhaustive` | `true` | Exhaustive batch and candidate caps (see [Large PRs](#large-prs)) |
| `review.decomposition` | `true` | Build the change graph and review cross-unit boundaries |
| `fusion.reviewer_weight` / `monitor_weight` | `1.0` / `0.75` | Log-odds weights |
| `fusion.prior_log_odds` / `fusion.clip` | `-0.85` / `12` | Prior and clip on the combined log-odds |
| `sast.enabled` / `sast.semgrep_bin` / `sast.timeout_secs` | `true` / `semgrep` / `55` | Static track |
| `sandbox.helper_image` | `docker.io/library/alpine:latest` | Hash/verification helper |
| `sandbox.rust_image` / `sandbox.node_image` / `sandbox.go_image` | `docker.io/library/rust:1.89-slim` / `docker.io/library/node:22-alpine` / `docker.io/library/golang:1.23-alpine` | Probe images for Rust (`cargo test --offline`), JavaScript/TypeScript (`node --test`, `tsx`) and Go (`go test`) |
| `sandbox.memory_mb` / `cpus` / `pids_limit` | `1024` / `1.0` / `256` | Container limits |
| `sandbox.local_fallback` | `false` | Allow `local` mode when no container runtime exists |
| `gate.authoring_pool_env` / `gate.reviewer_pool_env` | empty / `ZERO_LOOP_LLM_API_KEY` | Pool-disjointness audit |
| `redaction.private_log_name` | `security-private.jsonl` | Base name of the private record |
| `ci.api_base` | empty | Provider API root (self-hosted GitLab/Bitbucket) |
| `ci.webhook_secret_env` / `ci.webhook_port` | `ZERO_LOOP_WEBHOOK_SECRET` / `8791` | Webhook receiver |

`plugin.toml` ships a `[settings]` block with the operator-visible defaults for
a fresh install; it mirrors the declarations above (plus the three probe images,
which are TOML/environment-only) and leaves the per-stage model overrides unset
so they fall back to `model.primary`.

### Model endpoint

Endpoint fallbacks when `llm.base_url` is empty: `ZERO_LOOP_LLM_BASE_URL` →
`LITELLM_BASE_URL` → `OPENAI_BASE_URL` → `KARUCODE_BASE_URL`; key:
`ZERO_LOOP_LLM_API_KEY` → `LITELLM_API_KEY` → `OPENAI_API_KEY` →
`DEEPSEEK_API_KEY` → `KARUCODE_API_KEY`. A monitor on another provider can be
pointed at its own endpoint with `ZERO_LOOP_MONITOR_BASE_URL` /
`ZERO_LOOP_MONITOR_API_KEY`.

No endpoint means no semantic review: the plugin returns **human review** with
the reason `llm_unavailable`, never an approval.

## Tools

| Tool | Class | What it does |
| --- | --- | --- |
| `review_pr` | shell | Runs all five stages; returns the verdict, findings and committable suggestions. `post=true` publishes one review. |
| `review_status` | read-only | Reads a stored verdict and verifies its receipt hash chain. |
| `explain_finding` | read-only | Reads the private diagnostic of a redacted security finding. |

`review_pr` arguments: `base` (required), `head` (default `HEAD`), `workspace`
(defaults to the session workspace), `expected_head_sha`, `repository` +
`pull_number`, `description` (read only by the message/code consistency check; structured claims are forwarded)
`stacked_branches` (branch/SHA pairs that must also pass the gate) and `post`
(default `false`).

Example (inside a project session):

```json
{"name": "review_pr", "arguments": {"base": "origin/main", "head": "HEAD"}}
```

A tool call takes 1–4 minutes by default and writes receipts under
`state.dir/receipts/<review_id>/` (`receipts.jsonl` chain plus `verdict.json`).
`review.max_findings` and the hard ceiling of 50 bound how many findings a
single review can publish.

`post=true` needs `ci.provider`, `repository` and `pull_number`. An `approve`
decision is posted as a **COMMENT**, never an `APPROVE` review: a bot must not
satisfy a branch-protection approval.

The latest stored verdict is also exposed as a read-only panel,
`plugin__zero-loop-reviewer__review_status` (decision, counts, elapsed time and
the degraded reasons — not the whole review body).

## Command line

The same engine runs without a server. `serve` is the default when karucode
spawns the plugin with no arguments.

| Command | What it does |
| --- | --- |
| `serve` | The karucode plugin: JSON-RPC over stdio |
| `review` | One local review, no posting |
| `ci` | One webhook-shaped review, optionally posted once |
| `webhook` | A loopback receiver that turns deliveries into reviews |
| `config` | Print the effective configuration, with no secrets |

```bash
# local review, machine-readable verdict
zero-loop-reviewer review --base origin/main --head HEAD --json --out-dir artifacts

# a stacked PR: every intermediate branch must still sit on this head
zero-loop-reviewer review --base origin/main --stacked api=3f2a1c9 --stacked ui=77de104

# print what the plugin would actually use
zero-loop-reviewer config
```

Exit codes: `0` approve, `1` request changes, `2` human review / configuration
error.

### One-shot CI job

```bash
zero-loop-reviewer ci \
  --provider github --repository owner/repo --number 42 \
  --workspace "$GITHUB_WORKSPACE" \
  --payload "$GITHUB_EVENT_PATH" \
  --out-dir artifacts
```

`ci` needs `ci.provider` set to `github`, `gitlab` or `bitbucket`, and a token
in the variable **named by `ci.token_env`** (`GITHUB_TOKEN` by default), plus
`ZERO_LOOP_LLM_BASE_URL` + `ZERO_LOOP_LLM_API_KEY` for the reviewer.
`--no-post` runs the review without publishing. Passing `--payload` uses the
event's repository, number and head SHA; otherwise pass `--repository`,
`--number` and optionally `--head-sha`.

The job must have the PR **head** checked out. The pipeline refuses a review
whose checkout does not match the event's `head_sha` (or the provider's reported
head); it does not fetch for you.

### Webhook receiver

```bash
zero-loop-reviewer webhook --provider github --workspace /srv/repo --port 8791
```

It verifies `X-Hub-Signature-256` (GitHub), `X-Gitlab-Token` (GitLab) or
`X-Hub-Signature` (Bitbucket), enforces a 300-second replay window when the
delivery carries a timestamp header, de-duplicates delivery ids, answers the
provider immediately (`202` accepted, `204` ignored/duplicate, `401` bad
signature, `400` bad request) and reviews in a background thread. The secret is
read from the variable named by `ci.webhook_secret_env` (default
`ZERO_LOOP_WEBHOOK_SECRET`) and the receiver refuses to start without it. Bind
loopback and put a TLS reverse proxy in front.

## Security model

* Probes run with `--network none`, a read-only root, `--cap-drop=ALL`,
  `no-new-privileges`, memory/PID/CPU limits, a writable tmpfs at `/tmp` for the
  test runner, and a writable bind of a throw-away copy — never the review
  worktree. The image is chosen by the probe's language.
* The probe file's SHA-256 is recomputed inside the container before the probe
  runs. A one-byte difference aborts with `input_delivery_failed`.
* `local` sandbox mode runs probes with the reviewer's own privileges. It is a
  development convenience **and not a security boundary**.
* Analyzer commands are refused if they carry a write flag, and the digest of
  the files a command may touch is checked before and after each bounded run.
* Exploitable security findings are redacted in public: only the CWE class and
  the gate status are posted; the trace and verified patch go to a `0600`
  record under `state.dir/private/`, retrievable with `explain_finding`.
* A both-fail probe is published as advisory with a test-repair note instead of
  being silently suppressed.
* The PR description and reply threads never enter the bug-hunting prompt. Only
  the dedicated PR-MCI check reads the description, and it extracts claims; it
  cannot change the review prompt.
* The reviewer refuses to launch when its model key is the authoring agent's
  key (`gate.cross_provider_audit`, `gate.authoring_pool_env`). When no
  authoring pool is configured, the review records
  `pool_disjointness_unverified` instead of claiming disjointness.
* Receipts are hash-chained. `review_status` reports a broken chain instead of
  trusting it.

## Measured results

`./check.sh` runs everything below. On this machine:

* **468 tests** in about 60–67 s (real git 2.47.3, real Semgrep 1.173.0,
  real podman 5.4.2, a real OpenAI-compatible HTTP fake for the client tests);
  four podman-image tests skip when no image can run pytest.
* `python3 eval/run_eval.py` on the annotated corpus: **6/6 buggy cases
  intercepted, 0/4 clean or fix-confirming cases flagged**, in 33.8 s for all
  ten cases.
* A head-to-head against CodeRabbit on karucode PR #17
  (`benchmarks/coderabbit/`): CodeRabbit's export holds 11 unique comments, 6 of
  them on the reviewed head; this reviewer published **0** verified findings at
  that head. The six are mostly Rust/TypeScript semantic issues, and the
  executable proof path was Python-only when that snapshot was taken, so the gap
  is real rather than a parsing accident. Counts are not a quality score: label
  the unmatched items before drawing a conclusion.

The evaluation runs the deterministic half of the pipeline with *oracle*
candidate/probe/patch answers from the corpus annotation. It measures
verification, falsification and emission — not a live model's recall. The
93.5–100% objective is a design target; **no live-model recall number is claimed
here**, because no model endpoint is configured in this environment. To measure
it, set `ZERO_LOOP_LLM_BASE_URL`/`ZERO_LOOP_LLM_API_KEY` and run:

```bash
python3 eval/run_eval.py --provider live
```

The report is written to `eval/report.json`.

### Adjudicating against another reviewer

`tools/compare_reviews.py` pairs a CodeRabbit comment export with a zero-loop
`--json` verdict and prints matched, CodeRabbit-only and zero-loop-only issues.
It is an adjudication aid, not a scoreboard: counting comments favours a
reviewer that publishes unverified opinions.

```bash
zero-loop-reviewer review --base origin/main --head HEAD --json --out-dir out
python3 tools/compare_reviews.py \
  --coderabbit benchmarks/coderabbit/pr17.json \
  --verdict out/review-<id>.json \
  --head 68fa207f5b4bad10c5e6218c95d247e4205d1d37 \
  --labels labels.json --out comparison.md
```

`--head` keeps only comments whose `commit_id` starts with that SHA, so a stale
comment from an earlier push is not counted against the current diff. Each
unmatched item has an id; the optional labels file marks them
`{"cr-1": {"valid": true, "note": "real bug"}}` so the report counts what a human
actually judged valid. `benchmarks/coderabbit/README.md` carries the exact
`gh api` command for a fresh export and records what the first snapshot showed;
`benchmarks/coderabbit/pr17-report.md` is its saved report.

## Using it on the karucode repository

`karucode/.github/workflows/zero-loop-review.yml` runs this plugin on
`pull_request` events (and on `workflow_dispatch`). It checks out the PR head
with full history, checks the reviewer out from `karutoil/karucode-plugins` at a
**pinned full 40-character commit** (`vars.ZERO_LOOP_PLUGIN_REF`; the job fails
rather than run an unpinned reviewer), and reviews `base..head` with
`ci --payload`. A manual dispatch runs `review` instead. The model endpoint
comes from `ZERO_LOOP_LLM_BASE_URL` / `ZERO_LOOP_MODEL` repository variables;
the key is the `ZERO_LOOP_LLM_API_KEY` Actions secret. Fork and Dependabot PRs
are skipped: GitHub does not give them repository secrets, and falling back to
`pull_request_target` would expose the key to untrusted code. The job installs
`rustfmt` and the web `node_modules` so CIVER can syntax-check Rust and
TypeScript repairs, pulls the three probe images, and runs with a 3600 s review
budget and a 75-minute job timeout.

The shipped rule packs cover Python, Rust, TypeScript/JavaScript and GitHub
Actions YAML. CIVER parses the repaired file with `compileall`, `rustfmt`,
`node --check` or `esbuild` (from `web/node_modules/.bin/esbuild`) before it
publishes a suggestion. A workflow-only PR with pinned actions therefore
approves with no comments; that is the intended result, not a failure to run.
Pullfrog and CodeRabbit can comment on a clean YAML diff because they publish
ungrounded LLM opinions; this reviewer only comments on a mechanically verified
finding or reports `human_review`.

## Portability: any repository, any language

The review core is repository-agnostic: it reads the base/head diff, the
project's own entry points and its Semgrep configuration, and it never assumes
a fixed path such as `web/` or `crates/`. A language only needs an **adapter**
when you want executable reproduction probes. Built-ins cover Python, Rust, Go,
JavaScript and TypeScript; any other language is configured with
`probe.adapters` (JSON):

```toml
[probe]
adapters = '''
{
  "ruby": {
    "extensions": [".rb"],
    "test_path": "test/zl_probe_{name}_test.rb",
    "framework": "ruby",
    "image": "docker.io/library/ruby:3.3-alpine",
    "command": ["ruby", "{test_path}"],
    "test_marker": "def test_",
    "assertion_marker": "assert",
    "syntax_command": ["ruby", "-c", "{path}"]
  }
}
'''
```

The adapter supplies the test path template, image, command, test/assertion
markers and optional syntax command. The generated test still has to run in
the isolated sandbox and fail on the bad commit before it is published. A
repository with no adapter for a changed language is reviewed statically and
sent to human review at the probe boundary; the reviewer never invents a
language command. `sandbox.runtime = local` is development-only and is never a
CI security boundary.

A minimally language-specific GitHub workflow is in
`examples/zero-loop-review.yml`; it installs only the Python reviewer runtime
and pulls the Python probe image, so it can be copied into a Ruby, Java, PHP,
C#, Elixir or any other repository. Add adapters only if you want executable
probes for that language.

## Languages

Static coverage: Python, Rust, TypeScript/JavaScript and GitHub Actions rule
packs, plus the generic Semgrep pass. CIVER syntax-checks Rust with `rustfmt`,
JavaScript/TypeScript with node/esbuild and Python with its parser.

Executable probes now dispatch by language:

| Candidate | Probe | Framework | Image |
| --- | --- | --- | --- |
| Python | pytest/unittest | `pytest`/`unittest` | `sandbox.image` |
| Rust | `tests/*.rs` integration test | `cargo` | `sandbox.rust_image` |
| TypeScript | `node:test` file | `node`/`tsx` | `sandbox.node_image` |
| JavaScript | `node:test` file | `node` | `sandbox.node_image` |
| Go | `*_test.go` file | `go` | `sandbox.go_image` |

The probe prompt is language-specific, the host validates the test path and
framework, and the sandbox picks the matching image. Rust runs
`cargo test --offline` against `<crate>/Cargo.toml` with the target directory on
a writable tmpfs, so the image (or the copied worktree) must carry the crate
registry for an offline build. JavaScript/TypeScript runs `node --test`
(`--experimental-strip-types` for `.ts`, or `npx --no-install tsx` when that
framework is chosen) and needs the project's `node_modules` in the scratch copy.
Go runs `go test ./<package> -run Test` and, with no network in the sandbox,
needs the module cache or a vendored tree in the image.
Missing toolchains, dependencies or images are **setup errors**, not evidence
against the PR.

## Large PRs

The diff is read up to an 8 MB safety ceiling and the context assembler may use
up to **500k tokens**; the model output ceiling is **64k tokens**. Behavioural
review is semantically batched: up to 100 size-bounded batches (24 with
`review.exhaustive = false`) of 4 source files / 30k characters, four in flight
at once, and a batch never mixes two change units. Batches are handed out in
two-minute waves, so a four-minute review never starts an hour of model calls:
the batches that did not fit become a declared blind spot. Large files are split
at hunk boundaries, and an oversized single hunk is split at line boundaries
rather than dropped. Semgrep scans the complete changed-file set in chunks of at
most 200 paths (rebalanced into at most 12 chunks for a PR with thousands of
files), and up to 100 anchored hypotheses (24 with `review.exhaustive = false`)
reach the probe/CIVER stages, which are each capped at 32 candidates.

### Change-graph decomposition

`review.decomposition = true` (the default; TOML/env) builds a deterministic
file graph from changed-file imports, test/source pairing and feature-directory
affinity. Connected components become **change units** (capped at 12 files per
unit, split by feature when larger). Each behavioural batch carries its unit's
title, files and cross-unit edges; batches remain size-bounded, so this changes
the routing, not the transport.

When the graph has more than one unit and cross-unit edges, one **integration
pass** reviews the boundaries as well: changed signatures/contracts, API ↔
client, schema ↔ migration, shared types, feature flags and contradictory
changes between units. If the integration call fails, the result is a
human-review blind spot, not an approval. If the graph is empty or unusable, the
current file-ranked batching is the fallback.

Recorded while this was built (re-measure on your own repository before relying
on them):

* On a 268-line test commit the previous plan used 3 batches and 255 s; the
  decomposed plan used 5 semantic batches and 118 s, with the same coverage
  (151/151 lines) and the same 0 verified findings. That is a latency/locality
  result, not evidence of better recall.
* A synthetic cross-file bug — a backend response key renamed while a frontend
  consumer still indexed the old key — was grouped into one unit, caught by the
  reproduction probe and published with a verified patch in 60 s (2/2 changed
  lines covered).
* On the 9.6k-line PR #17 the graph produced 21 units and 19 cross-unit edges,
  29 batches, and still covered 6,492/6,492 production changed lines.
* A second synthetic 14-file PR split one connected component into two units and
  exercised the integration pass. The file-batched mode published 1 finding in
  101 s; the graph mode published **2** in 135 s, including the actual
  `KeyError` in the consumer unit that the file-batched mode missed, with 14/14
  lines covered in both. That is the first quality signal for the combined
  design, on a small synthetic case rather than a labelled production corpus.

What a review costs: one triage call, one security monologue, one behavioural
call per batch (plus one integration call when the graph has cross-unit edges),
up to three probe-synthesis attempts per probed candidate, one repair proposal
per probe- or rule-backed candidate, and one elicitation plus five monitor votes
per verified candidate. A small PR is a handful of calls; a 9k-line, 83-file PR
is about 24 behavioural calls and can legitimately take 20–40 minutes. The
workflow budget is **3600 s** and its job timeout is 75 minutes. The default
`review.deadline_secs=240` remains for ordinary PRs, so a small PR is still
reviewed in a few minutes.

### Coverage, not recall

Every changed line of a production source file must be assigned to a behavioural
batch; a missing assignment is recorded as
`behavioral_hunk_coverage_incomplete` and stops an approval. The full enclosing
function of each changed site is added to the context, not just the diff hunk and
its 1-hop signatures. When more anchored hypotheses exist than the candidate cap
allows, the excess is recorded as `behavioral_candidate_coverage_incomplete`.

Coverage is allowed to be *incomplete*, never silently green. A truncated diff
or context, a capped batch scan, or a probe/CIVER cap adds a blind spot; if no
finding is verified the verdict is then `human_review` instead of an approval. A
verified finding still produces `request_changes` and a committable suggestion.

`review.exhaustive` (default `true`) raises the batch cap from 24 to 100 and the
hypothesis cap from 24 to 100. It does not change the coverage rules above,
which always apply.

Analyzer findings are suppressed before they can become candidates when they sit
on a configured suppression path (`tests/`, `test/`, `vendor/`, `third_party/`),
inside test code, on a line carrying `nosec` / `semgrep-ignore` /
`zero-loop:ignore`, or — for an injection CWE — on a line where a configured
trusted sanitizer is applied. Test files are still reviewed behaviourally;
coverage simply does not require their lines.

This is a **coverage** guarantee, not a recall guarantee. On the 9.6k-line PR #17
it covered all **6,492 production changed lines** with zero missing assignments
and added 725 enclosing-function slices to the context. It still cannot promise
99.9% bug recall: the model is probabilistic, verification can reject a real but
hard-to-reproduce defect, and publication still stops at the ceiling of 50
rather than being unbounded.

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

* The executable probe path dispatches by language: Python (pytest/unittest),
  Rust (a `tests/*.rs` integration test run with `cargo test --offline`), Go (a
  `*_test.go` file run with `go test`) and JavaScript/TypeScript (`node --test`,
  or `tsx`). A Rust probe is always an integration test, so a defect that is only
  reachable through private internals cannot be reproduced; a missing toolchain,
  image or dependency is a setup error, which leaves the candidate unverified
  instead of publishing it. The RALE ensemble and ObliVul's risk counter remain
  Python-only: `rules/lsog_risk.py` parses the file with Python's `ast`, so a
  non-Python site cannot be ObliVul-verified.
* A finding needs a probe or an analyzer rerun to be emitted. A real defect
  whose reproduction cannot be synthesised becomes **human review**, not a
  public finding. That is deliberate: the zero-loop contract forbids an
  unverifiable suggestion.
* Risk elicitation and the independent monitor run on the five most severe
  eligible candidates (that is what bounds the model budget). A sixth verified
  finding can still be published on its mechanical evidence alone, with its
  probability left at 1.0 rather than fused.
* A PR that actually fixes a bug is approved when its reproduction fails on the
  base and passes on the head. That pair is recorded as `fix_confirmed` and
  suppressed; it is evidence for the fix, never a defect to request.
* ObliVul's risk model counts unguarded sinks. It is a causality check, not a
  semantic sanitizer proof; CIVER remains the stronger gate.
* GitLab/Bitbucket approvals do not carry a commit SHA. The gate reports them as
  discounted instead of pretending they cover HEAD.
* This environment needs `--uts=host` for podman (already the default in
  `extra_args`) and cannot bind-mount `/tmp`; scratch work happens under
  `state.dir`, so keep it under `/home`.
* The publication cap is dynamic: `0` (the default) publishes every verified
  finding up to the ceiling of 50. An explicit 1–50 value caps it lower.
  Findings beyond the effective cap are recorded as suppressed with a
  `publication cap` reason; the decision still reflects the worst finding. Every
  extra finding costs another inline comment and another verified suggestion.
* A single-hunk repair is rendered as a native `suggestion` block (one click). A
  multi-hunk repair is rendered as a verified `diff` block, because a provider
  suggestion covers exactly one line range. Two findings on the same line share
  one comment.
* `review_pr` runs one review at a time per invocation. The webhook receiver
  serialises reviews with a lock, so a burst of deliveries queues behind the
  first.

## Tests

```bash
./check.sh                                        # compileall + version + full suite
python3 -m pytest tests -q
python3 -m pytest tests -m 'not integration' -q   # no podman/semgrep
python3 eval/run_eval.py
```

Fault injection covers truncated input at 1%, 50% and one byte short;
cancellation at input-write and output-read; timeout kills; nonzero exit with a
valid JSON payload (`process_crash`); empty and malformed output; write-flag and
workspace-mutation refusal; and receipt tampering.

The Rust, Go and JavaScript/TypeScript probe tests run the real `cargo`/`go`/`node`
toolchain against a throw-away project and skip when it is absent; the container
path is covered by the pure argv/script tests, so a host without those images
still exercises the dispatch logic.
