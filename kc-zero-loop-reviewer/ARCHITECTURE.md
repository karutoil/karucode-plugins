# Phase 0 — Discovery and integration mapping

This document is the required pre-implementation discovery output. It describes
what this repository actually provides, what it does **not** provide, and where
each stage of `zero_loop_ci_reviewer` attaches. It contains no design borrowed
from another plugin in this repository: every interface below was read from the
harness protocol (the `protocol`/`sdk` copies that `karucode` itself generates,
and karucode's own documentation), or measured on this machine.

## 1. How plugins are registered

A karucode plugin is a **separate executable** supervised by the server. It is
not linked into the harness and does not import it. The wire format is
newline-delimited JSON-RPC 2.0 on stdin/stdout, plugin protocol version `1`.

* Discovery sources: `[[plugins]]` in `~/.config/karucode/config.toml`, or a
  manifest at `<data_dir>/plugins/<name>/plugin.toml`
  (`<data_dir>` defaults to `~/.local/share/karucode`). A duplicate name is an
  error, not a precedence rule.
* Manifest keys: `name`, `command` (argv, absolute), `enabled`, `permissions`,
  `env`, `settings`.
* The plugin answers `initialize` with its manifest (name, version,
  capabilities, permissions, hooks, tool prefix, panels, **settings
  declarations**), `tools/list` with `{tools: [{name, description,
  parameters, class}]}`, and `tools/call` with
  `{ok, content, meta}`; params are `{name, arguments, context}` where
  `context` carries `workspace`, `session_id`, `run_id`, `agent_id`, `project`,
  `depth`.
* Settings are **operator data**: the plugin declares them, the host validates
  and stores them, and the plugin reads them through `plugin/settings/get`
  (a request the plugin sends to the host over the same stream) or the
  `KARUCODE_PLUGIN_SETTINGS` environment variable. A plugin cannot write them.
* Tool `class` (`read_only`, `mutating`, `shell`, `external`) drives approvals.
* `permissions` the host understands include `files.read`, `files.write`,
  `shell`, `network`. They are declarations, **not** an OS sandbox.
* A plugin may contribute a panel (`panels/read` → opaque JSON data) and UI
  actions. This plugin contributes one read-only panel (`review_status`).

Measured host version: `karucode 171 (4c8d189cf4c9, 2026-11-16)`.

### Consequences for this plugin

The review engine is a Python package with a thin JSON-RPC entrypoint
(`zero_loop_reviewer.plugin.protocol`). The same engine is reachable as a
one-shot CLI (`zero-loop-reviewer ci`), so CI can run it without a live
karucode server. The plugin is spawned as
`bin/zero-loop-reviewer` (an executable launcher), which is what
`plugin.toml` points at.

## 2. How LLM calls are routed

karucode's provider layer is **OpenAI-compatible HTTP**:
`POST {base_url}/chat/completions`. It supports LiteLLM, vLLM, Ollama,
llama.cpp, hosted vendors, and named `[[providers]]` profiles. No vendor SDK is
involved and no reasoning about a specific vendor is needed.

* The install inspected here has **no** `[[providers]]` block in
  `~/.config/karucode/config.toml`; the running service gets its endpoint from
  its own environment. No provider API key is present in this shell.
* Therefore the plugin does not read karucode's config or credentials. It takes
  a base URL, key **environment variable name**, and per-stage model ids from
  its own settings, with these fallbacks:
  `ZERO_LOOP_LLM_BASE_URL` → `LITELLM_BASE_URL` → `OPENAI_BASE_URL` →
  `KARUCODE_BASE_URL`; key env `ZERO_LOOP_LLM_API_KEY` →
  `LITELLM_API_KEY` → `OPENAI_API_KEY` → `KARUCODE_API_KEY`.
* Default model for every stage: `od/deepseek-v4.1-flash` (the DeepSeek-V4.1
  Flash id used by this install's subagent profile), configurable per stage so
  a different model can be swapped in without code changes.
* Requests are non-streaming, `response_format={"type":"json_object"}` when the
  endpoint accepts it, with a strict Pydantic validation pass on the answer.
  Sampling temperature is per stage and is only sent when explicitly set.
* Determinism in tests comes from a real in-process OpenAI-compatible fake
  server (HTTP), not from monkeypatching the client; live-model runs are opt-in
  and are reported separately.

## 3. How CI webhooks, Git provider APIs and containers are invoked

**CI webhooks.** This repository contains no CI configuration and the harness
has no HTTP ingress for plugins (plugins are stdio processes; the server's own
routes are not extensible). Two integration surfaces are therefore implemented
by the plugin itself:

1. `zero-loop-reviewer ci --event pull_request --payload event.json` — the
   one-shot job entrypoint. Reads a GitHub/GitLab/Bitbucket webhook JSON, or
   explicit `--base`/`--head` refs, runs one review, posts one review payload.
2. `zero-loop-reviewer webhook --port 8791 --secret-env ...` — an optional
   receiver: HMAC signature verification, replay window, event de-duplication,
   then the same one-shot path. It binds loopback by default and refuses a
   non-loopback bind without a secret.

**Git provider APIs.** GitHub, GitLab and Bitbucket all expose the four
operations this plugin needs, behind one adapter interface:

| Operation | GitHub | GitLab | Bitbucket |
| --- | --- | --- | --- |
| PR metadata + head SHA | `GET /repos/{o}/{r}/pulls/{n}` | `GET /projects/{id}/merge_requests/{iid}` | `GET /repositories/{ws}/{slug}/pullrequests/{id}` |
| Diff | `GET .../pulls/{n}` (Accept: diff) | `GET .../merge_requests/{iid}/changes` | `GET .../diff` |
| Check runs for an exact SHA | `GET /repos/{o}/{r}/commits/{sha}/check-runs` | `GET /projects/{id}/pipelines?sha=` | `GET .../commit/{sha}/statuses` |
| Post one review | `POST .../pulls/{n}/reviews` | `POST .../merge_requests/{iid}/notes` | `POST .../pullrequests/{id}/comments` |

Adapters are plain HTTP clients with a fake transport in the tests; no SDK is
added. `BotIdentity` from the same interface enforces the bot-author gate.

**Sandboxed containers.** No Docker daemon is present. `podman 5.4.2`
(rootless) is, and it works with one measured caveat: `sethostname` is denied in
this outer environment, so every `podman run` must pass `--uts=host`. A second
measured caveat: only paths under `/home` are visible to the container
(`statfs /tmp/...` fails), so scratch worktrees live under the plugin's state
directory, never `/tmp`.

The verified sandbox invocation is:

```
podman run --rm --network none --uts=host --read-only --cap-drop=ALL \
  --security-opt no-new-privileges --memory=… --pids-limit=… \
  --cpus=… -v <scratch>:/w:rw -w /w <image> /bin/sh -c <cmd>
```

Images: `docker.io/library/python:3.12-alpine` (probes) and
`docker.io/library/alpine:latest` (hash/verification helpers); both are
configurable. When no runtime or image is available the sandbox reports
`sandbox_unavailable`, which the pipeline treats as **fail-open to human
review** — never as "no findings".

## 4. Where the five stages attach

```
git provider / CI job / karucode tool call
                │
                ▼
 stage 1  RuntimeContract ── receipts ── HeadCommitGate ── PR-MCI isolation
          ContextSlicer (1-hop symbols, conventions, DesignLore, 500k budget)
          ChangeGraph: imports/tests/directories → units + cross-unit edges
                │
                ▼
 stage 2  parallel executor (asyncio, ≤60 s)
          unit-aware behavioural batches ── cross-unit integration pass
          2A Semgrep/SAST delta ── patch-caused filter
          2B secSemScan monologue ── fresh line reconciliation ── CPG frames
          2C RALE: (AST && CFG) || Code
                │
                ▼
 stage 3  probe synthesis (≤3 reflexion attempts)
          sandbox: base commit b  ×  head commit b+delta
          fail-on-base retention / regression retention / setup-error discard
                │
                ▼
 stage 4  CIVER perturbation x' ── ObliVul bidirectional CPG ── DesignLore
                │
                ▼
 stage 5  risk-only elicitation p ── independent monitor S (k=5, T=0.8)
          coded Bayes τ = C_FR/(C_FA+C_FR) ── configurable publication cap ── one-click suggestion
          tiered security redaction
                │
                ▼
    one PR review payload + durable receipts + private security log
```

## 5. What is verified live vs. by deterministic doubles

| Component | Verification in this repository |
| --- | --- |
| stdio plugin protocol | subprocess test against the real JSON-RPC loop, plus a live `karucode` server on a throw-away data dir: plugin `ready`, 3 tools, 32 settings, no manifest errors |
| git diff / worktrees / commits | real `git` on fixture repositories |
| Semgrep delta | real `semgrep 1.173.0` on fixtures and shipped rules |
| symbol extraction | real Python `ast`; `ast-grep 0.45.3` when the language is not Python |
| sandboxed dual-commit probes | real `podman 5.4.2` + `python:3.12-alpine`, network `none`, `--uts=host` |
| LLM stages | real OpenAI-compatible HTTP against a local fake server, including a full `tools/call review_pr` run |
| Git providers | real HTTP request/response shapes against `httpx.MockTransport`; loopback webhook receiver over a real socket |
| annotated corpus | 6 buggy + 1 fixed + 3 clean cases; 6/6 intercepted, 0/4 clean or fix-confirming flagged with oracle candidate generation |

The recall target (93.5–100%) is a **design objective**. The corpus measures the
deterministic verification/emission pipeline; a live DeepSeek-V4.1 Flash run is
opt-in (`ZERO_LOOP_LLM_BASE_URL` + key, `eval/run_eval.py --provider live`) and
was not run here because this environment has no model endpoint. The README
states the measured numbers and does not claim a live-model recall it has not
observed.
