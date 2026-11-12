# kc-goals — a goal the agent cannot quietly drop

> A karucode plugin that gives a session a **goal**, a **definition of done**, and an
> **engine that puts the agent back to work every time it stops early** — until the
> goal is met, or a human stops it. There is no budget: it runs until it is done.

Status: plan + v0.1 implementation. Written before the code, kept as the design
record (the README is the user-facing half).

---

## 1. The ask

> "A plugin for goals, set by the agent or by the user, that gives the agent a goal
> and the agent keeps working until that entire goal is finished. It should not
> stop till it is finished."

Stated that way it sounds like a prompt. It is not: a prompt cannot make an agent
not stop, and every "keep going until you are done" instruction I have seen leak in
this harness leaks in the same place — **the model is the one who decides it is
done, and it decides early.**

So this document treats the ask as an engineering problem with four parts:

1. **Represent the goal** so "finished" is checkable rather than a feeling.
2. **Detect the stop** the moment the harness records it.
3. **Decide** whether that stop was legitimate — and if not, resume with enough
   context that the agent can act immediately.
4. **Bound** the loop, because an unstoppable agent is a bug, not a feature.

## 2. What already exists (checked before building)

This matters: three parts of the ask are already solved elsewhere in karucode, and
a plan that rebuilt them would be wrong.

| Existing thing | What it does | Why it is not this |
| --- | --- | --- |
| `limits.auto_continue` + `max_continues` (`kc-core/src/config.rs`) | When a run exhausts its **iteration** budget, the runtime emits a notice and keeps going in the same run (default 50 refreshes × 200 iterations, 86400 s ceiling). | It only covers the budget running out. The failure we care about is a model that *returns a final answer* after 4 tool calls and calls it a day. Nothing in the runtime vetoes that — the run has ended, successfully. |
| Missions (`/api/missions`, `kc-server/src/routes/missions.rs`, `mission_coordinator`) | Project-owned DAG of tasks; each task dispatches its **own session** with its own prompt, dependencies, verification tasks, provider-attempt budget, pause/resume/cancel. | Orchestration *across sessions*. A mission task ends when its run ends. It does not keep one conversation going, and it requires explicit review + `start`. |
| `kc-todo` | A task list the agent maintains, with evidence-required statuses and a `before_prompt` block. | A *list*, not a loop: nothing resumes a stopped run, and an item can sit `in_progress` forever. |
| `kc-deepseek-pace` | SSE watcher + `steer`/`cancel` ladder for turns that narrate instead of acting. | Adjacent failure mode (a turn that will not end), not this one (a run that ends too early). Its `api.rs`/`http.rs` are still the right template for talking to the server from a plugin. |
| Built-in `todo` tool + `spawn_agent` | Decomposition and delegation. | Goals compose with both; they do not replace either. |

**Conclusion:** the missing piece is session-level *continuation*, and the only
thing that can decide to continue is something outside the run.

## 3. Why a plugin, and which mechanism

A run ends when the model returns a turn with no tool calls. The plugin API
(protocol 1) exposes exactly four implemented hooks:

```
before_prompt   → text into the system prompt (byte-stable, cached)
on_run_end      → { session_id, run_id, status }   (fires per main run)
on_tool_result  → { session_id, tool, ok }
on_notify       → every notification the runtime emits
```

None of them can append a message to the conversation, and none of them can veto
the stop. But `on_run_end` fires **before** `RunFinished` is emitted and after the
run's status is persisted, which means a plugin can, at that instant, do what a
human would do: type the next message.

Three candidate mechanisms:

| # | Mechanism | Verdict |
| --- | --- | --- |
| A | Prompt-only: a `before_prompt` block that says "do not stop". | Cheap, already tried by every agent author. It does not survive a model that decides it is done. Kept as an *orientation* layer, not the mechanism. |
| B | **Plugin worker: `on_run_end` → decide → `POST /api/sessions/:id/messages`.** | Chosen. No core change, no privileged surface, and the server's own rules (409 when busy, queue limits, approval policy) still apply. |
| C | Core change: an `after_run` hook that may return a continuation message. | Correct long-term, and worth upstreaming. Rejected for v0.1: it forks the harness to do something the plugin API can already do. Documented in §13. |

### The two routes a continuation can take

`POST /api/sessions/:id/messages`:

* `?mode=send` (default) — start a run; **409** when a run is already active.
* `?mode=follow_up` — queue a message that starts the next run; **400** when
  *nothing* is active.

So the worker does: try `send`; on 409 → `follow_up`; on 400 → retry `send` after a
short sleep. That covers both sides of the race between "the run is gone" and "the
run is still registered", and it is exactly how a human using the composer behaves.

## 4. The model: goal, criteria, evidence, check, journal

A goal is not a string. It is a small document:

```jsonc
{
  "schema": 1,
  "session_id": "s_…",
  "active_goal": "g1",
  "goals": [{
    "id": "g1",
    "title": "Ship kc-goals v0.1",
    "statement": "Full detail, in the user's words. This is the contract.",
    "status": "active",              // active | paused | done | abandoned | blocked
    "source": "user",                // user | agent
    "created_at": 0, "updated_at": 0,
    "criteria": [
      { "id": "c1", "text": "plugin builds with zero warnings",
        "met": false, "evidence": null, "met_at": null }
    ],
    "journal": [                     // append-only, bounded
      { "at": 0, "note": "wrote the model + 12 tests", "next": "wire the worker" }
    ],
    "budget":   { "max_continuations": 0, "max_minutes": 0, "max_stalled_resumes": 5,
                  "max_failed_retries": 0 },   // 0 = no limit, and that is the default
    "progress": {
      "continuations": 0, "started_at": 0, "last_continuation_at": 0,
      "progress_since_continuation": 0, "no_progress_streak": 0,
      "failed_retries": 0, "last_run_id": null, "last_run_status": null
    },
    "pause_reason": null
  }]
}
```

Four decisions in that shape:

* **A criterion is the unit of done.** `goal_complete` refuses unless every
  criterion is `met` **and** carries `evidence`. "Done" is therefore a claim with
  receipts, not a vibe. This is the same discipline `kc-todo` uses for `done`, and
  it is the single most effective anti-"declare victory" measure available to a
  plugin (a tool result the model must read is a correction it cannot skip).
* **The journal is the goal's memory.** A goal can outlive its context window. On
  every continuation the worker re-states the goal *and the last few journal
  entries* in a fresh user message, so a long-running goal survives compaction,
  a model swap, or a restart. This is also why the journal is append-only and
  bounded rather than a free-form "notes" field.
* **Progress is a fact the plugin can check.** `goal_progress` bumps
  `progress_since_continuation`; the engine resets `no_progress_streak` only if
  that counter moved. Three continuations with no recorded progress → pause. That
  is the spin guard, and it is driven by data, not by a timer.
* **There is no budget by default.** Every ceiling is `0 = no limit`: no
  continuation count, no wall clock, no retry cap. The numbers exist so a human can
  add a guard rail on purpose, and they are stored per goal so a restart cannot
  reset one.
* **`check` is the only thing the agent cannot judge itself.** A criterion may
  carry a shell command; exit 0 marks it met and writes the command's own output
  as the evidence. `goal_complete` refuses while any check has not passed, and
  `goal_criterion` refuses to close one by hand.

## 5. Tools (agent-facing surface)

Nine tools (ten with command checking on), all `plugin__kc-goals__*`. Two are
read-only; the rest are ordinary `external` tools that go through the approval
policy unless the operator allowlists them. `goal_check` is not registered at all
unless `verify_commands` is on, because a tool that cannot work is noise in the
model's list.

| tool | purpose | refuses when |
| --- | --- | --- |
| `goal_set` | Create and activate a goal: title, statement, criteria (strings or `{text, check}`), optional budget, `replace`, `auto_start`. | an active goal exists and `replace` is absent; a `check` while verification is off |
| `goal_status` | Full state: criteria, journal tail, counters, **and why the engine will or will not continue** | — |
| `goal_criterion` | Mark one criterion met/unmet, with evidence when met | no such criterion; `met` without evidence |
| `goal_progress` | Append a journal note + next step; resets the no-progress streak | goal not active; note too short |
| `goal_check` | Run the criteria's check commands; exit 0 is the only pass | verification disabled (not registered); no criterion has a check |
| `goal_options` | Read or change one goal's limits without replacing it | the goal is closed |
| `goal_complete` | Close the goal and stop the loop | any criterion unmet; no evidence; a check that has not passed |
| `goal_pause` | Stop continuing, keep the goal (reason required) | already paused |
| `goal_resume` | Re-arm a paused goal | not paused |
| `goal_abandon` | Drop the goal (reason required) | already closed |
| `goal_list` | Every goal in the workspace, newest first | — |

Descriptions are written for a coding agent and kept short (the free-search lesson:
24 KB of upstream prose became 6 KB of curated text and got *better*). Each one
ends with the same rule: *a status you cannot prove is not a status*.

## 6. The continuation engine

### 6.1 Triggers (three, all idempotent on `run_id`)

1. **`on_run_end`** — primary. Payload `{session_id, run_id, status}`. The handler
   only records intent and wakes the worker (it must return inside the host's hook
   budget; no HTTP in a hook).
2. **Reconcile tick** (default 20 s) — every session with an active goal whose
   `last_run_id != runs.recent[0].id` is re-examined. This is the recovery path for
   a dropped event, a plugin restart, a server restart, a goal created while idle,
   and the POST that failed.
3. **Goal creation while idle** — falls out of (2) with no special case: active
   goal, session not running, `continuations == 0` → start.

### 6.2 Policy by run status

| run ended as | meaning | engine does |
| --- | --- | --- |
| `completed` | the model stopped itself — the case this plugin exists for | continue if the goal is still open |
| `limit_reached` | a *run* ceiling (wall clock / continue cap) | continue as a **new run** (fresh ceilings), counted against the goal budget, with a notice |
| `interrupted` | the server died mid-run | continue (that is what durable goals are for) |
| `failed` | provider/transport error | retry with backoff up to `max_failed_retries` (default 2), then pause + notify |
| `cancelled` | a human (or another plugin) pressed stop | **stop.** Pause the goal, notify once, do not fight the stop button. `goal_resume` re-arms. |

### 6.3 Guards, evaluated in order, before every continuation

```
1  plugin enabled, auto_continue on
2  goal.status == active
3  this run_id has not already been continued        (the idempotency key)
4  a cancelled run pauses instead of resuming        (the stop button wins)
5  continuation cap, if one was set (default: none)
6  wall clock, if one was set (default: none)
7  stalled resumes < max_stalled_resumes (default 5, 0 disables)
8  >= min_gap_secs since the last continuation, doubled per failed run, max 5 min
9  the continuation prompt is < max_prompt_chars     (5000)
```

Only 7 pauses a goal by default, and it is not a budget: it counts resumes whose
run made **no tool call and wrote no `goal_progress` note**, using the harness's
own tool-call counter (`GET /api/sessions/:id/info`), so a long compile or a
40-call refactor resets it. A model that answers "done" forever without acting
would otherwise spend a full prompt per cycle, indefinitely.

### 6.4 What the continuation message says

A user turn (append-only → prompt-cache friendly), capped and structured:

```
[kc-goals] Continuing goal g1 — Ship kc-goals v0.1. You stopped before it was done.

Goal: <statement, trimmed>
Remaining criteria (2/5):
  c3 — drive the plugin in a real harness
  c4 — zero-warning release build
Last progress: wrote the model + 12 tests  (next: wire the worker)
Budget: unlimited continuations; no deadline; 0/5 stalled resumes used.

Rules: do the next concrete step now. Record it with goal_progress when it lands.
Mark a criterion with goal_criterion only with evidence. When every criterion is
met, call goal_complete. If you are blocked, call goal_pause with the reason —
do not stop silently.
```

## 7. Storage and scoping

* Path: `<workspace>/.karucode/goals/<session_id>.json`, one file per session.
  Same shape as `kc-todo`'s `.karucode/todo/list.json`, so the workspace-confined
  file tools can read it, a human can edit it, and `git clean` takes it.
* Atomic writes (temp + rename) and a per-file lock around read-modify-write.
  A corrupt file is an error naming the path, never a silent reset.
* **Scope is the session, not the project.** The conversation that is working is
  the thing that continues; a second session in the same workspace has its own
  goal (and its own panel row). Two agents on one workspace therefore do not
  cross-continue each other.
* The panel reads all goals in the workspace. The index of which sessions have
  goals is just the directory listing, so there is no second source of truth.
* A deleted session leaves an orphan *file*; the engine detects the 404 and marks
  the goal `blocked` with reason `session no longer exists` instead of retrying.

## 8. The prompt surface (cache discipline)

`before_prompt` lands in **message 0** and is re-requested on every model call. Two
hard rules, both learned from this harness's own prompt-cache audit:

* the block is **byte-stable**: no timestamps, no counters, no "3 of 25 left".
  Everything dynamic goes in the continuation message or the tool result.
* it is emitted **only while an active goal exists** in this workspace, so a
  session without a goal pays zero bytes and zero invalidations. A goal costs one
  invalidation when it starts and one when it ends — and none in between, which is
  the whole point.

The block is ~300 bytes: a goal exists, do not stop early, call `goal_status`
before concluding anything, and completion requires evidence.

## 9. Config, permissions, install

```toml
[[plugins]]
name = "kc-goals"
command = ["…/kc-goals/target/release/kc-goals"]
enabled = true
# `shell` only for goal_check (a criterion judged by a command); drop it and
# that tool is not registered at all.
permissions = ["files.read", "files.write", "network", "shell"]
[plugins.settings]
server = "http://127.0.0.1:8787"   # loopback only; plain HTTP
token  = "…"                        # only if KARUCODE_AUTH_TOKEN is set
tick_secs = 20
auto_continue = true
# no budget: 0 means unlimited, and 0 is the default
max_continuations = 0
max_minutes = 0
max_failed_retries = 0
# not a budget: a stall detector (no tool call AND no progress note)
max_stalled_resumes = 5
# command-checked criteria (runs /bin/sh -c in the workspace; needs `shell`)
verify_commands = true
read_only_tools = ["goal_status", "goal_list"]   # operator-only, in config.toml
```

`files.write` is required (the goal file lives in the workspace); `network` is for
loopback; `shell` is only for `goal_check`, and only when `verify_commands` is on.
The two read-only allowlist entries are the only approval exemption, and they
cannot be set by the plugin's own manifest — the host ignores
`settings.trust`/`read_only_tools` from a `plugin.toml` in the plugin's directory,
which is exactly right.

## 10. Panel: a contract, not decoration

The inspector tab is the plugin's only human surface, and it has one constraint
that shapes everything: **a plugin cannot run UI and cannot call a tool.** The
frontend is a compile-time snapshot inside the binary, and there is no
`POST /api/tools/call`. A panel is JSON in, JSON out.

So the payload is a *description* a first-party renderer draws, versioned by
`schema: "kc-goals/panel/2"`:

| part | what it carries | why it is there |
| --- | --- | --- |
| `goals[]` | one card per goal: title, statement, status, score, `progress_pct`, counters, `pause_reason`, the engine's verdict, the next step | the "why didn't it resume?" answer |
| `goals[].criteria_detail[]` | id, text, met, evidence, and `check {command, ok, checked_at}` | what is done, what proved it, what a command said |
| `goals[].options[]` | the goal's four limits, each with `label`, `kind`, `unit` and **`zero_means`** | `0` must read as "no cap", never as "zero allowed" |
| `goals[].actions[]` | id, label, kind, `enabled`, hint, and **`instruction`** | see below |
| `engine.options[]` | the switch-level knobs: auto-continue, prompt block, command checks, tick, gap | an operator can see what the engine will do without reading the file |
| `log[]` | the last 20 decisions with their outcome | a decision with no reason is unfalsifiable |
| `error` | a plugin-side failure (a corrupt goal file, no server) | the tab says so instead of showing an empty list |

**Actions are messages.** Since no route executes a tool, each action carries the
exact user turn to post (`Call plugin__kc-goals__goal_pause for g1 …`), and the
renderer sends it with `send`, falling back to `follow_up` on 409 — the same
dance the engine itself does. The plugin stays the single definition of what each
operation means; the UI never invents wording.

**Two halves, one contract.** The renderer's source lives here in `ui/`
(`GoalsPanel.tsx`, its test, `goals.css`) and is copied into the app
(`web/src/components`, `web/src/styles`) with two one-line wirings. `check.sh`
fails when a copy drifts, because a field renamed in `src/render.rs` must break a
test on the other side rather than silently render a blank card.

**What was verified, in a browser.** Against a throwaway server running the built
bundle (`tests/live/hold_lab.py`), a real plugin process and a real goal file: the
tab drew the card (not the JSON fallback), the paused goal offered only
**Resume**, and clicking it posted the instruction as a user turn — confirmed in
the transcript (`runs: 3`). Two findings worth keeping:

1. A panel read without `?project=` gets the *default* workspace, so the payload
   resolves the session's own workspace first (already true for the data, and now
   for the tab a human opens).
2. Turning the stall detector off in that fixture produced **146 runs** in a
   minute: the renderer is fine, but it is a vivid reminder that
   `max_stalled_resumes = 0` means "loop forever on a do-nothing model".

## 11. Failure modes and the guard against each

This is the section that earns the plugin. Every row is a way the naive version
(SSE watcher + "post another message") fails.

| # | Failure | Guard |
| --- | --- | --- |
| 1 | **Premature victory** — model says "done" at 40 % | completion requires every criterion met *with evidence*; a refused `goal_complete` returns the exact missing list, so the next turn is corrective |
| 2 | **Infinite loop** — same work re-done forever | the stall detector: 5 resumes in a row that made no tool call and recorded nothing → pause (recoverable). Optional caps exist for anyone who wants a hard stop |
| 3 | **Fighting the stop button** — user presses Escape, plugin restarts the run | `cancelled` pauses the goal; it never auto-resumes |
| 4 | **Double continuation** — event + tick + restart all fire | idempotency key is `run_id`; one POST per run; `follow_up` fallback |
| 5 | **Race at the boundary** — POST while the run is deregistering | `send` → 409 → `follow_up` → 400 → retry `send` |
| 6 | **Plugin restart** (server restart, rebuild, hot reload) | state is on disk; tick reconciles from `runs.recent[0].id`; no in-memory truth |
| 7 | **Server restart mid-run** | run becomes `interrupted` → continue |
| 8 | **Prompt-cache thrash** | static block; dynamic state only in appended user turns |
| 9 | **Context exhaustion** on a long goal | journal + capped re-statement per continuation; v0.2 adds `goal_handoff` (continue in a fresh session seeded from the goal file) |
| 10 | **Cost blow-out** | *nothing automatic*, by request: a goal runs until it is finished. The panel and `goal_status` show continuations used, and the stall detector catches the pathological case (a model that only says "done"). A human can set `max_continuations`/`max_minutes` per goal |
| 11 | **Unattended run blocked on an approval** | documented: a goal needs `approvals.mode = "auto"` or an allowlist, or it parks at the prompt. v0.3: `on_notify` `needs_input` → pause + notify |
| 12 | **Subagent runs** trigger continuation | only `on_run_end` of the main run is used, and the tick reads `runs.recent[0]` (main runs), so a subagent finishing never continues a session |
| 13 | **Two sessions, one workspace** | per-session files; the engine never continues a session other than the goal's own |
| 14 | **Hand-edited / corrupt goal file** | parse error names the file and the fix; never reset silently; atomic writes |
| 15 | **Model swap or compaction mid-goal** | the goal file, not the transcript, is the contract; the continuation re-states it |
| 16 | **Goal set while idle, expecting work now** | `auto_start` (default on) → the first tick starts run #1 |
| 17 | **Goal deleted/abandoned but a queue entry survives** | `follow_up` carries the goal id; a run that starts after abandonment is a normal run — nothing to cancel |
| 18 | **A runaway plugin** (this one) | every action is an ordinary authenticated API call from a normal client; the human's stop button pauses the goal, and `max_stalled_resumes` catches a do-nothing loop |
| 19 | **Rewind hides the continuation message** | the goal is unaffected (state is on disk); the next tick may re-post once for that run id |
| 20 | **Notification spam** | at most one notification per terminal transition (paused / done / blocked / budget spent), never per continuation |

## 12. Test plan

1. **Unit** (crate, no server): model validation, criterion/evidence rules,
   `goal_complete` refusal text, budget arithmetic, status policy table, guard
   ladder, prompt rendering (byte-stability of the `before_prompt` block, cap on
   the continuation prompt), config parsing, atomic save/load round-trip, corrupt
   file error, panel payload shape.
2. **Live, deterministic**: a temp `karucode` server on a spare port, a scripted
   mock OpenAI endpoint, the plugin registered, one throwaway session. Scenario
   `stop-early`: run 1 calls `goal_set` then stops; the plugin must post a
   continuation; run 2 marks criteria and calls `goal_complete`; **no run 3**.
   Scenario `capped`: `max_continuations = 2` with a model that never progresses →
   exactly 2 continuations, one notification, goal paused.
   Scenario `unlimited`: six consecutive stops, all resumed, no cap recorded.
   Scenario `stall`: a model that only says "done" is paused after
   `max_stalled_resumes`.
   Scenario `checks`: two criteria judged by real commands close the goal, and the
   verdicts land in the transcript.
   Scenario `cancel`: cancel run 1 by hand → no continuation, goal paused.
3. **In the real harness**: install, `POST /api/plugins/reload`, confirm
   `status = ready`, tools listed, panel renders, and `plugin__kc-goals__goal_status`
   answers. No goal is created in a session I do not own.

## 13. What this deliberately does not do

* It is not a scheduler or a mission engine: no DAG, no cross-session dispatch, no
  new sessions. That is `/api/missions`, and goals should be able to *name* a
  mission (v0.2 link) rather than replace it.
* It does not bypass approvals, permissions, or `read_only` mode.
* It cannot see the model's reasoning and does not judge the *quality* of work.
  Without a `check` it only checks that the agent's own claims carry evidence; with
  one, the command — not the agent — decides, but the command is still written by
  an agent or a human, so a bad check proves a bad thing well.
* It does not make a run survive its own wall-clock ceiling; it starts a new run.
* It does not cap cost. By request there is no token or continuation budget; see
  §11 row 10 for what that means and what a human can set per goal.
* It does not modify karucode. If upstream ever adds `after_run` → `Option<String>`
  (a continuation message), this plugin's worker becomes unnecessary and the goal
  file becomes the payload of that hook. That is the v2 path.

## 14. Roadmap

* **v0.1** — goal model, 9 tools, `on_run_end` + tick engine, panel, config,
  install, unit + live tests. **Shipped, then revised (see §15b).**
* **v0.2** — shipped in this revision: `goal_check` (a criterion judged by a
  command, opt-in, approved like any shell tool) and the no-budget default with a
  stall detector. Still open: token *visibility* (not a cap) from
  `/api/sessions/:id/info`; `on_notify` `needs_input` → pause + notify;
  `goal_handoff` (fresh session seeded from goal + journal) for context
  exhaustion; a first-party panel renderer.
* **v0.3** — mission integration (`goal_set` may cite `mission_id` and reuse its
  task list as criteria), and an upstream `after_run` hook proposal so continuation
  needs no HTTP round-trip.

## 15. Definition of done — met

- [x] `cargo build --release` clean (0 warnings), `cargo clippy --all-targets` clean,
      `cargo test` green: **184 tests**; `npm test` in `web/`: 1262 tests, 19 of them
      the new renderer.
- [x] Tools: nine without command checking, ten with it. Each description is short
      and ends with the evidence rule; `goal_status` and `goal_list` are the two
      read-only ones, and `goal_check` is registered only when
      `verify_commands` is on.
- [x] `before_prompt` block is a `const` (byte-identical across calls, no digits)
      and is emitted only while an active goal exists — proven live: the scripted
      provider saw it in 5 of 7 requests, absent before the goal existed.
- [x] Engine: five-way status policy, the guard ladder, `run_id` idempotency,
      reconcile tick, send→409→follow_up→400→send delivery, failure backoff —
      each with a unit test.
- [x] **No budget by default**: continuations, wall clock and retries are all
      `0 = no limit`. The only automatic stop is the stall detector, and it pauses
      rather than giving up.
- [x] Live scenarios, 21 checks, `tests/live/drive.py`:
      `stop-early` (resume once, close, stop), `unlimited` (six resumes, no cap, no
      pause), `capped` (an explicit cap is honoured), `stall` (a do-nothing loop is
      paused), `checks` (two criteria judged by real commands), `cancel` (the stop
      button wins).
- [x] Installed in `~/.config/karucode/config.toml`, `status = ready` in the
      running harness with ten tools and `shell`, panel listed in `GET /api/config`
      as `plugin__kc-goals__goals`.
- [x] Proven against a **real model** twice: once with plain evidence
      (`goal_set` → stop → one continuation → a byte-verified file → done) and once
      with command-checked criteria (`goal_check` → `PASS c1` / `PASS c2` → done),
      both ending at exactly 2 runs with the session idle.
- [x] README documents install, config, the continuation policy, the stall
      detector, command checking, the panel contract and every limitation.
- [x] Panel: payload `kc-goals/panel/2` with the goal, criteria + check verdicts,
      per-goal limits (`zero_means`), engine options, actions as instructions and
      the decision log; a first-party renderer in `ui/` with 19 tests, `check.sh`
      guarding the copy, and a real-browser run proving the Resume button posts
      its instruction.

## 15a. What building it changed in this plan

Writing the code and driving the real harness corrected five things the paper
version had wrong. They are recorded here because each one is a trap for the next
person who writes a continuation plugin.

1. **`on_run_end` fires inside the run's teardown**, before `RunFinished` is
   emitted and after the status is persisted. That makes it the right trigger —
   the status is already true when the hook runs — but it also means HTTP there
   puts the plugin's network latency (and a wedged server) on the path of every
   run that ends. Hence: the hook only queues an intent; the thread does the work.
2. **`?mode=follow_up` answers 400 when nothing is running**, not silently. So the
   delivery order has to be `send → 409 → follow_up → 400 → send`, with one retry.
   A simpler "always follow_up" design loses continuations at the boundary.
3. **The panel's `workspace` is the *default* workspace**, not the session's, when
   `?project=` is absent. A plugin that trusts it shows another project's goals.
   Fixed: the panel resolves the session's own workspace through
   `GET /api/sessions/:id` and reports both (`workspace`, `requested_workspace`).
   Found live, not in review.
4. **A plugin tool is `plugin__<plugin>__<tool>` to the model.** A scripted mock
   that calls `goal_set` gets "unknown tool" and the run ends with the goal never
   recorded — which looks exactly like a plugin failure. The test driver now
   prefixes, and says so in a comment.
5. **Everything that spends a model turn gets in the way of a scripted test.**
   AI session titles are a second request the turn counter cannot distinguish from
   the real one; `KARUCODE_TITLES=0` is mandatory in the live lab.

## 15b. Second pass: "no budget", command-checked criteria, and two bugs

The first revision shipped with a budget (25 continuations / 180 min / 3 dry runs)
and agent-asserted evidence. The operator's answer was blunt and changed the spine
of the design: *"there is no budget, it should just keep going"*, and *"implement
[the recommended verification]"*.

**No budget.** Every ceiling now defaults to `0 = no limit`, and the resume gap
backs off (5 s doubling to 5 min) instead of a retry cap, so a dead provider is
retried forever but cheaply. What replaced the budget is not a smaller budget: it
is a **stall detector** that counts only resumes in which the run did *nothing at
all* — no tool call, no `goal_progress` — using the harness's own tool-call counter
rather than the agent's account of itself. It pauses (recoverable, notified) and a
single tool call resets it. It is disabled with `max_stalled_resumes = 0`.

**Command-checked criteria.** A criterion may carry `{text, check}`; `goal_check`
runs the commands with `/bin/sh -c` in the workspace and exit 0 is the only pass.
`goal_criterion` then refuses to close it by hand and `goal_complete` refuses until
every check has passed. Two independent switches, both operator-side:
`verify_commands` (the plugin does not even register the tool otherwise) and the
`shell` permission in the registration.

Two bugs only the live harness could find:

1. **The session index was global.** Both the real server's plugin and the test
   harness's plugin used the same default data dir, so each one's reconciler asked
   *its own* port about the *other's* sessions, got a 404, and blocked live goals
   (`status: blocked` on a goal that was fine). Fixed by namespacing the index per
   server: `sessions.<server-slug>.json`. This is a real-world hazard too — two
   servers during an upgrade would have fought the same way.
2. **A `shell`-class tool is not gated by the permission on the deployed build.**
   The declared class only gates exposure on a newer host; the binary here exposes
   it regardless of the config's `permissions`. So the plugin now decides for
   itself: `goal_check` is registered only when `verify_commands` is on, which is
   the behaviour the docs claimed the host provided.

## 16. Decisions (answered)

1. **Budget** — *none*. "It should go till the goal is completely finished." Every
   ceiling defaults to unlimited; the stall detector is the only automatic stop,
   and it pauses rather than giving up.
2. **`cancelled` semantics** — pause, recoverable. "Goals should be recoverable and
   fixed."
3. **Verification commands** — the recommended option, implemented and enabled:
   per-criterion `check`, opt-in via `verify_commands` + `shell`, exit code is the
   evidence, and a check criterion cannot be closed by hand.
4. **A completed goal stays in the file** as history (`done`; `goal_list` shows it).
   Unchanged: it costs nothing and a human can still read what was promised.
5. **Naming** — `kc-goals` / `plugin__kc-goals__goal_*`, as long as the model
   understands it. The descriptions name the tools explicitly, so it does.

Still open, for later: token *visibility* in the panel (not a cap); a
`needs_input` notification pausing a goal that is stuck at an approval prompt;
`goal_handoff` for a goal bigger than one context window.
