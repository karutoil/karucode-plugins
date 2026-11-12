# kc-todo

A TODO list an agent actually keeps up to date, plus a **Todo tab in the
inspector** that ships with the plugin.

Agents are good at *writing* TODO lists and bad at *maintaining* them. The usual
failure is not a missing list — it is a list that stops matching the code: items
left `in_progress` after the work landed, items ticked off that were never
verified, and a turn that ends with the list untouched. A human then reads a tidy
list that is quietly wrong.

This plugin is built around four mechanisms that make that failure mode either
impossible or **visible in the next prompt**.

---

## 1. What you get

**A task tree.** Tasks with subtasks, two levels deep.

**Seven statuses**, named for what they actually mean:

| wire value | UI label | requires |
| --- | --- | --- |
| `pending` | Not implemented | — |
| `in_progress` | In progress | — |
| `blocked` | Blocked | `reason` |
| `deferred` | Deferred | `reason` |
| `partial` | Implemented (limited) | `limitation` |
| `done` | Implemented | `evidence` |
| `cancelled` | Cancelled | `reason` |

`implemented` parses to `done` and `implemented_but_limited` to `partial`, so the
words a model naturally reaches for are accepted.

**Six tools:** `todo_plan`, `todo_add`, `todo_update`, `todo_remove`,
`todo_list`, `todo_audit`.

**Two hooks:** `before_prompt` (puts the list and what is unfinished in front of
the model) and `on_run_end` (records what was left open).

**One inspector panel** (`todo`) — the tab is automatic, because the plugin
declares it. See §4.

---

## 2. How it stops the usual failure

Each mechanism targets one specific way agents drop the ball.

### A status cannot be claimed without its proof

`done` requires `evidence`; `partial` requires `limitation`; `blocked`,
`deferred` and `cancelled` require `reason`. The refusal names the argument to
pass, so the model's next call can succeed instead of guessing:

```
marking `t2` as `done` requires `evidence`: what proves it is done (the command
that passed, the file and behaviour that now work, the test name). If that is not
true yet, use `partial` (implemented but limited) or leave it `in_progress`.
```

`partial` exists so that an honest half-finish never has to be dressed up as
`done`. That is the single most useful status for real agent work.

### The rules are enforced in code, not requested in the prompt

* a parent cannot be `done` or `cancelled` while a subtask is open;
* only one item is `in_progress` at a time, unless the caller passes
  `allow_parallel: true`;
* a subtask cannot be added to a closed task;
* unknown ids are refused with the call that lists the real ones.

Asking a model to respect these in a prompt is unreliable. A tool call that
*fails* is not.

### Unfinished work follows the agent into the next turn

This is the mechanism that closes the loop. `on_run_end` records what was still
`in_progress` or `pending`, and the very next `before_prompt` leads with it:

```
[TODO list]
3 of 4 item(s) are still open. Close each as you finish it, in the same turn as
the work: `todo_update {"id":"t1","status":"done","evidence":"..."}`.
Carried over from the last run: still `in_progress` — t4 (Add the Todo tab);
never started — t3 (Write tests), t5 (Docs).
```

The model reads that before every tool call it makes. It cannot start something
new without being told what it left behind.

### The list audits itself

An item marked `done` that was never `in_progress` is recorded as an *unverified*
completion. The prompt block and the tab both report it:

```
Unverified: t7 marked done without ever being in progress. If that was not
actually verified, add the evidence or set the honest status.
```

None of this can compel honesty. What it does is make every shortcut cost an
explicit, recorded claim instead of a silent omission — and put that claim in
front of both the model and the human.

### Prompt-cache safety

The `before_prompt` block is part of the system prompt, which providers cache. A
block that changed every iteration would miss the cache on every turn. So the
block is:

* **deterministic** — no timestamps, counters or randomness; the same list renders
  byte-for-byte the same text (there is a test for this);
* **bounded** — past `compact_from` items it collapses to per-status counts plus
  only the open items, and it is pinned under the host's 4 KiB hook cap by a test;
* **memoised** per session and list version, with the carry-over correctly
  invalidating that cache when the list version has not moved.

---

## 3. Install

```bash
cd kc-todo
cargo build --release
./install.sh            # writes the manifest, prints the next step
```

`install.sh` registers the plugin by writing a manifest to
`<data_dir>/plugins/kc-todo/plugin.toml`. Or add it to
`~/.config/karucode/config.toml` yourself:

```toml
[[plugins]]
name = "kc-todo"
command = ["/absolute/path/to/kc-todo/target/release/kc-todo"]
enabled = true
# The list lives in the workspace, so the plugin reads and writes files.
permissions = ["files.read", "files.write"]
# Every plugin tool is `external` and therefore prompts for approval. The two
# read-only tools are safe to skip; the mutating ones keep the prompt.
[plugins.settings]
read_only_tools = ["todo_list", "todo_audit"]
```

**A new plugin needs a server restart.** Discovery runs once at startup
(`PluginHost::discover`), so editing the config does not add a plugin to a running
server — `POST /api/plugins/:name/enable` only starts a plugin the host already
knows about. Confirm it loaded:

```bash
curl -s localhost:8787/api/plugins | jq '.plugins[] | select(.name=="kc-todo") | {status, tools: [.tools[].name]}'
```

You should see six tools, three hooks and one panel.

### Settings

All optional, under `settings`:

| key | default | meaning |
| --- | --- | --- |
| `max_in_progress` | `1` | concurrent in-progress items before `allow_parallel` is needed |
| `carry_over_reminder` | `true` | put unfinished work in the next prompt |
| `compact_from` | `24` | item count where the prompt block summarises |
| `sessions_dir` | `.karucode/todo/sessions` | where per-session lists are stored, workspace-relative |
| `list_path` | `.karucode/todo/list.json` | the old project-wide list, read once to seed a session |
| `workspace` | none | pin the workspace when the host does not send call context |
| `session` | none | pin the session when the host does not send call context |
| `read_only_tools` | — | host setting: tools that skip the approval prompt |

### Where lists live

**One list per session:**

```text
<workspace>/.karucode/todo/sessions/<session-id>.json
```

Per session, not per project. Two conversations about the same repository are two
different pieces of work, and a single shared list made each one inherit the
other's items. The tab and every tool are scoped by the session id the host
forwards (`context.session_id` on a tool call, `?session=` on the panel route), so
switching conversation in the UI switches the list.

Being inside the workspace means the workspace-confined file tools can read it,
`git` can ignore it, and deleting a project takes its lists with it:

```gitignore
.karucode/
```

#### Upgrading from the per-project layout

Before this change there was one shared `<workspace>/.karucode/todo/list.json`.
The first session to read after the upgrade inherits a **copy** of it and owns its
own file from then on, so an existing conversation does not appear to lose its
work. The legacy file is left in place, which means a second session started right
after the upgrade also inherits it — delete that file when you no longer want new
sessions seeded with it.

---

## 4. The Todo tab

The plugin declares a panel and the **tab appears by itself**: karucode's
inspector appends one tab per panel a ready plugin declares
(`GET /api/config` → `plugin_panels`), and reads it over
`GET /api/plugins/kc-todo/panels/todo`.

Nothing needs to be installed for that. A plugin contributes **data, never
markup** — the payload is opaque JSON the host forwards untouched, rendered by a
first-party component. With no component registered you get a read-only JSON view
of the payload, which is honest but ugly.

This repository's karucode checkout already has the renderer installed, so the tab
is drawn as a task tree (see `ui/preview.png`). The files live in two places:

| file | karucode path |
| --- | --- |
| `ui/TodoPanel.tsx` | `web/src/components/TodoPanel.tsx` |
| `ui/TodoPanel.test.tsx` | `web/src/components/TodoPanel.test.tsx` |
| `ui/todo.css` | `web/src/styles/todo.css` |

plus `web/src/components/todoPanelRegister.ts` (which calls
`registerPluginPanelRenderer`), an import of it from `web/src/main.tsx`, and
`@import './todo.css';` in `web/src/styles/index.css`. `ui/` is the source of
truth; copy from it after editing either side.

**Rebuild both halves after changing the renderer**, because the UI is embedded in
the binary at compile time:

```bash
cd web && npm run build        # emits web/dist
cd .. && cargo build --release # embeds web/dist
```

Setting `KARUCODE_WEB_DIR=web/dist` on the server skips the second step during
iteration.

Registering the **hosted** id (`plugin__kc-todo__todo`), not the bare `todo`,
means another plugin with a panel of the same name keeps its own rendering.

The renderer validates the payload before drawing it, declines anything it does
not recognise (the shell then falls back to JSON), and never lets a title reach
`dangerouslySetInnerHTML` — there is a test for the `<img onerror>` case. That
matters because panel data is untrusted input.

The tab is **read-only on purpose.** The visible list and the agent's working
list must be the same list; if a person could tick an item in the tab, the two
would drift and the tab would become a second, stale source of truth. Edits go
through the agent, which records evidence with the status.

---

## 5. Using it from an agent's point of view

The plugin is designed to be discoverable from the tool descriptions alone. A
typical session:

```jsonc
// plan
todo_plan {"tasks": [
  {"title": "Add the parser", "detail": "src/prompt.rs owns the syntax",
   "children": [{"title": "Handle CRLF"}, {"title": "Write tests"}]},
  {"title": "Document it"}
]}

// start one — refused if another is already in progress
todo_update {"id": "t2", "status": "in_progress"}

// finish it — one call, both transitions, evidence required
todo_update {"id": "t2", "status": ["in_progress", "done"],
             "evidence": "cargo test: 12 cases pass"}

// honest half-finish
todo_update {"id": "t1", "status": "partial",
             "limitation": "no error recovery yet"}
```

A parent closes only once its subtasks are closed:

```jsonc
todo_update {"id": "t1", "status": "done", "evidence": "merged"}
// -> `t1` still has open subtasks: t3 (pending), t5 (pending). Close those first
//    (each needs its own status and evidence), or mark `t1` `partial` and say
//    what the gap is.
```

---

## 6. Layout

```
kc-todo/
  Cargo.toml
  src/
    main.rs        tools, hooks, the panel handler, and wiring
    model.rs       statuses, items, and the rules (the core of this plugin)
    render.rs      prompt block, tool results, and the panel's payload
    session.rs     workspace memory and the per-version prompt cache
    config.rs      settings and the atomic on-disk store
    protocol.rs    vendored wire types (refresh with `karucode plugin sdk .`)
    sdk.rs         vendored SDK
  ui/
    TodoPanel.tsx      the panel renderer for the inspector
    TodoPanel.test.tsx
    todo.css
    preview.png        what it looks like
```

`model.rs` is where the value is: `List::update` is the function that refuses a
status without its proof, a parent with open children, and a second in-progress
item. Everything else exists to surface its decisions —
`render::to_view` is the contract the panel renderer reads.

---

## 7. Development

```bash
./check.sh                    # tests, clippy, fmt and the vendored-SDK check
```

That runs `cargo test --release` (70 tests), `cargo clippy --release
--all-targets -- -D warnings`, `rustfmt --check` over the sources this repo owns,
and `karucode plugin sdk . --check`.

`src/protocol.rs` and `src/sdk.rs` are vendored copies of the harness's SDK and
must stay **byte-identical** to what `karucode plugin sdk` emits. Two consequences
worth knowing:

* do not run `cargo fmt` over the whole crate — it reformats them and the SDK
  check then fails;
* `rustfmt` follows `mod protocol;` from `main.rs` into those copies, so a
  per-file check needs `--config skip_children=true` (`--skip-children` is not a
  flag). `check.sh` does this for you.

Drive the protocol without the server:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"hooks/run","params":{"hook":"before_prompt","payload":{"session_id":"s1","workspace":"/tmp/ws"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"todo_plan","arguments":{"tasks":[{"title":"Ship it"}]}}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"todo_update","arguments":{"id":"t1","status":"done"}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"panels/read","params":{"panel":"todo","session_id":"s1","workspace":"/tmp/ws"}}' \
  | ./target/release/kc-todo
```

The third call fails on purpose: `done` without `evidence` is refused.

After upgrading karucode, refresh the vendored SDK:

```bash
karucode plugin sdk .
```

---

## 8. Known limits

* **A new session starts with an empty list**, or a copy of the legacy shared one
  if `<workspace>/.karucode/todo/list.json` still exists (delete it to stop that).
  Carrying work between conversations is deliberate, not automatic: ask the agent
  to re-plan.
* **The tab does not live-update.** It reads when opened or refreshed. `refresh_ms`
  is available, but a poller for a list that changes only when the agent calls a
  tool would be waste; `0` (manual) is the default on purpose.
* **Nothing forces an agent to use the tools at all.** A run that never calls
  `todo_plan` has no list, and the tab stays empty. The `before_prompt` block only
  appears once a list exists, so this plugin nudges maintenance but cannot
  mandate planning.
* **Without the renderer the tab shows raw JSON.** It still works and still shows
  the audit, just without the styling in `preview.png`.
