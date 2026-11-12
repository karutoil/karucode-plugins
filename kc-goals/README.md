# kc-goals

A goal an agent cannot quietly drop.

karucode ends a run when the model returns a turn with no tool calls — and nothing
in the harness vetoes that. A model that decides it is done at 40 % is, as far as
the runtime is concerned, done. `kc-goals` is the outside party that disagrees: it
records what "finished" means, notices when a run stops with the goal unmet, and
puts the agent back to work with the goal restated — **until it is finished**.

```
> goal: finish the migration and prove it with the test suite

  goal_set        g1 created, 3 criteria (one judged by a command), no cap, no deadline
  …run ends early…
  [kc-goals] Continuing goal g1 — finish the migration. Remaining criteria (1/3 met):
    c2 — the migration runs on a copy of the real schema  [checked by: ./migrate --dry-run]
    c3 — the suite passes against the migrated database
  …run ends early…
  goal_check      PASS c2 — exit 0 in 1.4s   PASS c3 — exit 0 in 38s
  goal_complete   done — the engine stops resuming
```

Design notes, the failure-mode table and the roadmap live in [PLAN.md](PLAN.md).
This file is the operator's half.

---

## 1. Install

Register it in `~/.config/karucode/config.toml`:

```toml
[[plugins]]
name = "kc-goals"
command = ["/home/karutoil/karucode-plugins/kc-goals/target/release/kc-goals"]
enabled = true
# Goals live in the workspace; the engine talks to the local server.
permissions = ["files.read", "files.write", "network"]

[plugins.settings]
# The server that spawned the plugin. Plain HTTP, loopback only.
server = "http://127.0.0.1:8787"
# Only if the server runs with KARUCODE_AUTH_TOKEN: the plugin's environment is
# cleared, so it cannot inherit it.
# token = ""
# How often the reconciler re-examines sessions with an open goal. This is what
# recovers a goal after a dropped hook, a plugin restart or a failed POST.
tick_secs = 20
# The loop is never re-triggered faster than this (it backs off on failures).
min_gap_secs = 5
# The two tools that only read. Every mutating goal tool keeps prompting.
read_only_tools = ["goal_status", "goal_list"]
```

**Nothing else is required, and there is no budget to set: a goal runs until it is
finished.** See §5 for the one guard that is on by default and why it is not a
budget.

### Turning on command-checked criteria (optional)

A criterion can carry a shell command whose exit code is the evidence, so the
agent cannot close it on its own word. That is real command execution, so it is
opt-in and needs two things:

```toml
permissions = ["files.read", "files.write", "network", "shell"]   # on the [[plugins]] entry
[plugins.settings]
verify_commands = true
check_timeout_secs = 120     # a check that runs longer is killed
```

With `verify_commands` off the plugin does not register `goal_check` at all (nine
tools); with it on there are ten. Reload the plugin after changing it
(`POST /api/plugins/kc-goals/reload`).

Checks run as `/bin/sh -c <command>` with the workspace as the working directory
and the plugin's own OS rights — the server user's rights, **not a sandbox**. They
go through the approval policy like any other plugin tool, so `approvals.mode =
"ask"` will prompt for each one unless you add `goal_check` to the allowlist.

Build it with `cargo build --release`. There is no published crate and no
dependency on karucode: `src/sdk.rs` and `src/protocol.rs` are the vendored SDK
(`karucode plugin init` wrote them; `karucode plugin sdk .` refreshes them).

`plugin.toml` in this directory is the same registration in the
`<data_dir>/plugins/kc-goals/` form. Use **one or the other**: a name registered
twice fails the slot with an error naming both sources, and the `plugin.toml` form
cannot carry `read_only_tools`.

## 2. The mental model

| piece | what it is | why |
| --- | --- | --- |
| **goal** | title + the contract in the words it was given in | the continuation has to restate it, not summarise it |
| **criteria** | 1–20 things that must be true at the end | "finished" becomes checkable instead of a feeling |
| **evidence** | required the moment a criterion is marked met | a status you cannot prove is not a status |
| **check** | optional command per criterion; exit 0 is the only pass | the agent cannot be the judge of its own work |
| **journal** | append-only notes: what changed, what is next | the goal's memory across a context window, a model swap or a restart |

State lives in `<workspace>/.karucode/goals/<session>.json` — one file per
session, readable by the workspace-confined file tools, editable by hand, and
greppable. A second session in the same workspace has its own goal and its own
engine; nothing cross-continues.

## 3. Tools

Exposed as `plugin__kc-goals__<tool>`.

| tool | what it does | refuses when |
| --- | --- | --- |
| `goal_set` | create and activate a goal: `title`, `statement`, `criteria` (strings or `{text, check}`), `replace`, `auto_start` | an open goal exists and `replace` is absent; a `check` when verification is off |
| `goal_status` | criteria, evidence, **check verdicts**, journal, **and whether the engine will resume this session** | — |
| `goal_criterion` | mark one criterion met/unmet, with `evidence` | `met` without evidence; the criterion has a `check` that has not passed |
| `goal_progress` | append a journal note + next step (this is what counts as progress) | note shorter than 3 chars; closed goal |
| `goal_check` | run the criteria's check commands and record the verdicts | verification disabled (the tool is not even registered); no criterion has a check |
| `goal_options` | read or change this goal's own limits (cap, wall clock, stall threshold, retries); `0` = no limit | the goal is closed |
| `goal_complete` | close the goal and stop the loop | any criterion unmet, unevidenced, or its check has not passed |
| `goal_pause` | stop resuming, keep the goal (`reason` required) | already paused |
| `goal_resume` | re-arm a paused/blocked goal | not paused/blocked |
| `goal_abandon` | drop the goal (`reason` required) | already closed |
| `goal_list` | every goal in the workspace | — |

`goal_complete` is the important one. Its refusals are the product:

```
cannot complete `g1`: 2/3 criteria are unmet:
  c2 - the migration runs on a copy of the real schema
  c3 - the suite passes against the migrated database
Do the work, then mark each with `goal_criterion` and its evidence.

cannot complete `g1`: these criteria are judged by a command that has not passed:
  c2 - `./migrate --dry-run` (last run failed)
Run `goal_check` and let the commands decide.
```

## 4. The continuation policy

Exactly one rulebook, evaluated every time a run ends:

| the run ended as | meaning | what happens |
| --- | --- | --- |
| `completed` | the model stopped itself — the case this plugin exists for | **resume** |
| `limit_reached` | a *run* ceiling (wall clock / auto-continue cap) | resume as a new run |
| `interrupted` | the server died mid-run | resume |
| `failed` | provider or transport error | retry forever, with exponential backoff (5 s → 5 min) |
| `cancelled` | a human (or another plugin) pressed stop | **pause the goal**, notify once, never fight it |

Everything else is a guard rail somebody may add: a continuation cap, a wall
clock, a retry cap. All default to **0 = no limit**.

**What a resume is.** An ordinary `POST /api/sessions/:id/messages` from a normal
client — `send`, falling back to `?mode=follow_up` when a run is still registered,
then back to `send` if the run ended in between. Nothing privileged, and the
server's own rules (409 when busy, queue limits, approval policy) still apply.

**Why it survives things.** Two triggers, one rulebook: `on_run_end` is the fast
path (it only queues an intent and wakes a thread), and a reconcile tick (20 s) is
the safety net. Both key their work on the run id, so the safety net cannot
double-continue what the fast path handled. Kill the plugin, restart the server,
drop the hook: the next tick reads `GET /api/sessions/:id/info`, sees an open goal
whose newest run was never handled, and continues it. Nothing authoritative is in
memory.

**Why it does not thrash the prompt cache.** The `before_prompt` block is a
constant — no counter, no timestamp, no goal text — and it is emitted only while a
goal is open. A goal therefore costs two invalidations in its whole life (start
and end) and none in between. Everything dynamic travels in the appended
continuation message and in tool results.

## 5. The one thing that stops a goal by default

Not a budget — a **stall detector**.

A goal runs until it is finished, so nothing counts resumes. But a run in which
the model made **no tool call and write no `goal_progress` note** did literally
nothing, and a model that answers "I'm done" forever without acting would spend a
full prompt per cycle, indefinitely. Five of those in a row pauses the goal:
recoverable (`goal_resume`), notified once, and the reason says what happened.

The count comes from the harness (`GET /api/sessions/:id/info` → tool calls), not
from the agent's own account, so a long compile or a 40-tool-call refactor resets
it even if the model forgot to write a note. Set `max_stalled_resumes = 0` to turn
it off entirely; the panel and `goal_status` show the current streak either way.

## 6. What it will not do (read this before trusting it)

* **It cannot see quality.** Without a `check`, a criterion is closed by the
  agent's own evidence: an agent that writes a convincing lie gets away with it.
  That is why check commands exist, and why the descriptions ask for a command a
  human can re-run.
* **A check is not a sandbox.** It runs with the server user's rights, from the
  workspace. Enable it where that is acceptable; the approval policy still applies
  per call.
* **It does not run unattended through an approval prompt.** With
  `approvals.mode = "auto"` (or an allowlist) a goal runs on its own. With `"ask"`
  it parks at the first unapproved tool call until someone answers.
* **Nothing caps the cost.** There is no token or continuation budget: that is the
  point, and it is also the risk. The stall detector is the only automatic stop;
  the panel shows continuations used so a human can notice.
* **It cannot make one run longer.** A run that hits `limits.max_run_secs` ends;
  the goal continues as a *new* run with fresh ceilings.
* **Escape pauses, it does not cancel.** Cancelling a run pauses the goal
  (recoverable with `goal_resume`). Pressing stop should never be silently undone.
* **A long goal still fills the context window.** The journal is what carries it
  across compactions, model changes and restarts. A goal bigger than one window
  should be split, or continued in a fresh session seeded from
  `.karucode/goals/<session>.json` (v0.2's `goal_handoff`).
* **It is not a scheduler.** `/api/missions` is the multi-session DAG; goals are
  one conversation that refuses to stop.
* **It does not touch karucode.** If upstream ever adds an `after_run` hook that
  may return a continuation message, the worker here becomes unnecessary.

## 7. The inspector panel

The **Goals** tab shows every goal in the project — the one for the open session
first — as a card: status, score with a progress bar, the contract, every
criterion with its check verdict and evidence, the engine's own verdict on
whether it will resume, the next step, per-goal limits, the journal tail, and the
last decisions the engine made (`continue`, `pause`, `skip`, `error`, with why).

That is the "why didn't it resume?" answer, which is the question this plugin
exists to make answerable.

### What the buttons do (and cannot do)

There is **no route that executes a tool**. A panel returns JSON; the only way a
client can make the agent act is to send it a message. So each action in the
payload carries the exact `instruction` to post, and the renderer posts it as a
user turn — `send`, falling back to `follow_up` when a run is already working
(the same 409 the engine handles):

| action | when it appears |
| --- | --- |
| What is left? | while the goal is open |
| Run the checks | while a check has not passed |
| Complete | always, but disabled until every criterion is met and checked |
| Pause / Abandon | while the goal is open |
| Resume | once the goal is paused or blocked |

The wording lives in the plugin, so renaming an operation does not touch the UI.
`goal_pause`, `goal_check` and friends stay the single definition of what each
action means, and a human clicking a button is doing exactly what the model does.

### Installing the renderer

A plugin cannot ship UI: the frontend is a compile-time snapshot embedded in the
binary. The panel works without this (the generic JSON view draws the payload),
but the *source* of the nice renderer is kept here, next to the payload it was
written for, and copied into the app:

| plugin | app |
| --- | --- |
| `ui/GoalsPanel.tsx` | `web/src/components/GoalsPanel.tsx` |
| `ui/GoalsPanel.test.tsx` | `web/src/components/GoalsPanel.test.tsx` |
| `ui/goals.css` | `web/src/styles/goals.css` |

plus two one-line wirings the app owns: `components/goalsPanelRegister.ts`
(registered under the hosted id `plugin__kc-goals__goals`) imported from
`main.tsx`, and `@import './goals.css';` in `styles/index.css`.

```bash
./check.sh                                  # the copies and the payload agree
cd /home/karutoil/karucode/web && npm test  # the renderer's tests
npm run build                               # embeds into the app bundle
```

`check.sh` is the guard against drift: the two halves are one contract, and a
renamed field in `src/render.rs` breaks a test on the other side.

**The embedded UI only changes when karucode is rebuilt and restarted** (or run
with `KARUCODE_WEB_DIR=web/dist`, which is how `tests/live/hold_lab.py` shows the
panel in a real browser without touching the live server).

## 8. Testing

```bash
cargo test                     # 184 unit tests: the rulebook, the file, the checks, the payload
cargo clippy --all-targets     # clean
python3 tests/live/drive.py    # 6 scenarios against a real server, ~4 min
python3 tests/live/drive.py --scenario checks --keep
./check.sh                     # the panel renderer copies have not drifted
cd /home/karutoil/karucode/web && npm test   # 19 renderer tests, plus the rest of the app

# Look at the panel in a real browser (boots a throwaway server with the built
# UI and holds it open; prints the URL and the session id):
python3 tests/live/hold_lab.py --web-dir /home/karutoil/karucode/web/dist
```

The live driver boots a throwaway karucode server (temp data dir, temp workspace,
its own port) with the plugin registered, and points it at
`tests/live/mock_provider.py` — a scripted OpenAI-compatible endpoint whose turns
come from a JSON list. That makes "the model stops early" a deterministic input:

| scenario | what it proves |
| --- | --- |
| `stop-early` | a goal recorded then abandoned by the model is resumed **once**; evidence closes it; no third run |
| `unlimited` | six consecutive stops are all resumed, with no cap recorded and no pause |
| `capped` | an explicit `max_continuations` is still honoured, and the pause names it |
| `stall` | a model that only says "done" is paused after `max_stalled_resumes`, not resumed forever |
| `checks` | two criteria judged by real commands (`test -f`, `grep -qx`) close the goal; the verdicts land in the transcript |
| `cancel` | cancelling the run pauses the goal and posts nothing |

## 9. Debugging

Talk to the plugin without the server (the protocol is just lines of JSON):

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol":"1"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"goal_status","arguments":{},"context":{"session_id":"s_x","workspace":"/tmp/ws"}}}' \
  | ./target/release/kc-goals
```

In the running harness:

```bash
curl -s localhost:8787/api/plugins | jq '.plugins[] | select(.name=="kc-goals")'
# The panel payload the inspector draws (schema kc-goals/panel/2):
curl -s "localhost:8787/api/plugins/kc-goals/panels/goals?session=s_…" | jq .data
```

The **Goals** tab in the inspector shows every goal in the project: status, score,
continuations used, stalled resumes, the engine's verdict, pending checks, and the
last 20 engine decisions (`continue`, `skip`, `pause`, `error`) with their reasons
— the fastest way to answer "why didn't it resume?".

Logs go to stderr only (stdout is the protocol). The session → workspace index
lives in `~/.local/share/karucode/kc-goals/sessions.<server>.json`, one file per
server, so two servers never read each other's sessions.

## 10. Uninstall

Remove the `[[plugins]]` entry (or `plugin.toml`) and rescan/restart. Goal files
stay where they are — they are yours. Delete `.karucode/goals/` in a workspace to
forget its goals, and `~/.local/share/karucode/kc-goals/` for the index (a hint,
safe to delete).
