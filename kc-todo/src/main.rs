//! `kc-todo` — a TODO list an agent actually keeps up to date.
//!
//! # The problem this plugin is built around
//!
//! Agents are good at writing TODO lists and bad at maintaining them. The usual
//! failure is not a missing list: it is a list that stops matching the code —
//! items left `in_progress` after the work landed, items ticked off that were
//! never verified, and a run that ends with the list untouched. A human then
//! reads a tidy list that is quietly wrong.
//!
//! # The four mechanisms
//!
//! 1. **A status cannot be claimed without its proof.** `done` requires
//!    `evidence`, `partial` (implemented but limited) requires `limitation`, and
//!    `blocked`/`deferred`/`cancelled` require `reason`. The refusal names the
//!    argument to pass, so the model's next call can succeed.
//! 2. **The rules are enforced in the model, not in the prompt.** A parent
//!    cannot close while a subtask is open; only one item is `in_progress` at a
//!    time unless the caller explicitly allows parallel work. `todo_update`
//!    fails with a message that says which item to close first.
//! 3. **Unfinished work follows the agent into the next turn.** `on_run_end`
//!    records what was still open, and the very next `before_prompt` leads with
//!    it — the literal sentence "t3 was still `in_progress` when the last run
//!    ended". This is the mechanism that closes the loop, because it reaches the
//!    model before it can start anything new.
//! 4. **The list audits itself.** `done` without ever being `in_progress` is
//!    recorded as an *unverified* completion and reported in the prompt and the
//!    UI, so a shortcut is visible rather than silent.
//!
//! None of this can compel honesty. What it does is make every shortcut cost an
//! explicit, recorded claim instead of a silent omission — and put that claim in
//! front of both the model and the human reading the Todo tab.
//!
//! # Tools
//!
//! | tool | purpose |
//! | --- | --- |
//! | `todo_plan` | replace the list with a task/subtask tree |
//! | `todo_add` | add one task or subtask |
//! | `todo_update` | change one item, enforcing the status rules |
//! | `todo_remove` | delete a task and its subtasks |
//! | `todo_list` | read the list and its audit |
//! | `todo_audit` | just the audit: what a reviewer would flag |
//!
//! All of them accept an optional `workspace` to pin the project when two
//! sessions run at once (see `session.rs` for why that is needed).
//!
//! # Hooks
//!
//! * `before_prompt` — injects the list plus what is unfinished (memoised per
//!   session and list version, so the prompt cache stays warm).
//! * `on_run_end` — records open work and queues the carry-over reminder.
//!
//! The list is stored at `<workspace>/.karucode/todo/list.json`, which the
//! workspace-confined file tools can read and `git` can ignore.

// The vendored modules expose more API than this plugin uses; without these
// allowances every unused protocol type would warn on the first build.
#[allow(dead_code)]
mod protocol;
#[allow(dead_code)]
mod sdk;

mod config;
mod model;
mod render;
mod session;

use model::{List, Patch, Status};
use sdk::{
    hooks, permissions, PanelRequest, PluginBuilder, PluginToolResult, PluginToolResultExt,
    ToolRequest,
};
use serde_json::{json, Value};

/// The process-wide session map. The SDK's tool handlers are `Fn`, not `FnMut`,
/// so shared state has to be an explicit static rather than a captured closure.
static SESSIONS: std::sync::OnceLock<session::Store> = std::sync::OnceLock::new();

fn sessions() -> &'static session::Store {
    SESSIONS.get_or_init(session::Store::new)
}

/// A tool failure the model can act on. The text is the whole point: it names
/// the argument or the call that fixes the problem.
fn fail(message: impl std::fmt::Display) -> PluginToolResult {
    PluginToolResult::err(message.to_string())
}

/// Where one call's list lives: a workspace and the session inside it.
struct Scope {
    workspace: String,
    session: String,
}

impl Scope {
    /// Path of this scope's list file, for tool results and the panel.
    fn file(&self) -> String {
        config::session_file(&self.workspace, &self.session)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| format!("{}/{}", config::sessions_dir(), self.session))
    }
}

/// Resolve the workspace and session a call belongs to.
///
/// Both come from the host's `context` (which every `tools/call` carries), with
/// explicit `workspace`/`session` arguments taking precedence, and the last
/// `before_prompt` as a fallback for an older host. Lists are per session, so a
/// session that cannot be determined is an error naming the fix rather than a
/// guess: reading another conversation's list would be worse than failing.
fn scope_of(
    explicit_workspace: Option<&str>,
    explicit_session: Option<&str>,
    ctx: Option<&sdk::ToolCallContext>,
) -> Result<Scope, PluginToolResult> {
    let workspace = sessions()
        .workspace_from(explicit_workspace, ctx.map(|c| c.workspace.as_str()))
        .map_err(fail)?;
    let session = sessions()
        .session_from(explicit_session, ctx.map(|c| c.session_id.as_str()))
        .map_err(fail)?;
    Ok(Scope { workspace, session })
}

/// The `session` argument, if the model passed one.
fn explicit_session(req: &ToolRequest) -> Option<&str> {
    req.arguments.get("session").and_then(Value::as_str)
}

fn explicit_workspace(req: &ToolRequest) -> Option<&str> {
    req.arguments.get("workspace").and_then(Value::as_str)
}

/// Load the list for the session a call resolves to.
fn load(req: &ToolRequest) -> Result<(Scope, List), PluginToolResult> {
    let scope = scope_of(
        explicit_workspace(req),
        explicit_session(req),
        req.context(),
    )?;
    let list = config::read_for(&scope.workspace, &scope.session).map_err(fail)?;
    Ok((scope, list))
}

/// Save the list and hand back a tool result built from the new state.
fn save(scope: &Scope, list: &List, action: &str, ids: &[String]) -> PluginToolResult {
    let path = match config::write_for(&scope.workspace, &scope.session, list) {
        Ok(path) => path,
        Err(e) => return fail(e),
    };
    let mut text = render::render_change(action, list, ids);
    text.push_str(&format!("\nList: {} (v{})", path.display(), list.version));
    PluginToolResult::ok(text)
}

fn ts() -> u64 {
    config::now_ms()
}

// ---------------------------------------------------------------------------
// todo_plan
// ---------------------------------------------------------------------------

/// Replace the list with a plan. Each entry is `{title, detail?, children?: []}`.
fn plan(req: ToolRequest) -> PluginToolResult {
    let (scope, mut list) = match load(&req) {
        Ok(pair) => pair,
        Err(e) => return e,
    };

    let Some(entries) = req.arguments.get("tasks").and_then(Value::as_array) else {
        return fail(
            "`tasks` is required: an array of `{title, detail?, children?: []}`. \
             Pass an empty array to clear the list.",
        );
    };
    if entries.is_empty()
        && !req
            .arguments
            .get("confirm_clear")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return fail(
            "`tasks` is empty, which would delete the whole list. Pass \
             `confirm_clear: true` if that is really what you want.",
        );
    }

    list.clear(ts());
    let mut ids = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(title) = entry.get("title").and_then(Value::as_str) else {
            return fail(format!(
                "`tasks[{index}]` has no `title`. Every entry needs one: \
                 {{\"title\": \"...\"}}."
            ));
        };
        let detail = entry.get("detail").and_then(Value::as_str).unwrap_or("");
        let parent_id = match list.add(title, detail, None, ts()) {
            Ok(id) => id,
            Err(e) => return fail(e),
        };
        ids.push(parent_id.clone());

        if let Some(children) = entry.get("children").and_then(Value::as_array) {
            for (child_index, child) in children.iter().enumerate() {
                let Some(child_title) = child.get("title").and_then(Value::as_str) else {
                    return fail(format!(
                        "`tasks[{index}].children[{child_index}]` has no `title`."
                    ));
                };
                let child_detail = child.get("detail").and_then(Value::as_str).unwrap_or("");
                match list.add(child_title, child_detail, Some(&parent_id), ts()) {
                    Ok(id) => ids.push(id),
                    Err(e) => return fail(e),
                }
            }
        }
    }

    save(&scope, &list, "Planned", &ids)
}

// ---------------------------------------------------------------------------
// todo_add
// ---------------------------------------------------------------------------

fn add(req: ToolRequest) -> PluginToolResult {
    let (scope, mut list) = match load(&req) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    let title = req.str_arg("title");
    if title.trim().is_empty() {
        return fail("`title` is required: say what the task is.");
    }
    let detail = req.str_arg("detail");
    let parent = req
        .arguments
        .get("parent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty());

    match list.add(title, detail, parent, ts()) {
        Ok(id) => {
            let ids = vec![id];
            save(&scope, &list, "Added", &ids)
        }
        Err(e) => fail(e),
    }
}

// ---------------------------------------------------------------------------
// todo_update
// ---------------------------------------------------------------------------

/// Read an optional string argument, distinguishing "absent" from "empty".
fn opt_string(req: &ToolRequest, key: &str) -> Option<String> {
    req.arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn update(req: ToolRequest) -> PluginToolResult {
    let (scope, mut list) = match load(&req) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    let id = req.str_arg("id").trim().to_string();
    if id.is_empty() {
        return fail("`id` is required (e.g. `t2`). Call `todo_list` to see the ids that exist.");
    }

    // `status` may be a word or a list. A list is applied in order, which is what
    // makes the honest `in_progress` -> `done` sequence one call instead of two.
    let statuses: Vec<Status> = match req.arguments.get("status") {
        Some(Value::String(raw)) => match Status::parse(raw) {
            Some(status) => vec![status],
            None => {
                return fail(format!(
                    "`{raw}` is not a status. Use one of: pending (not implemented), \
                     in_progress, blocked, deferred, partial (implemented but limited), \
                     done (implemented), cancelled."
                ))
            }
        },
        Some(Value::Array(entries)) => {
            let mut parsed = Vec::with_capacity(entries.len());
            for entry in entries {
                match entry.as_str().and_then(Status::parse) {
                    Some(status) => parsed.push(status),
                    None => {
                        return fail(format!(
                            "`{}` in `status` is not a status. Use one of: pending, \
                             in_progress, blocked, deferred, partial, done, cancelled.",
                            entry.as_str().unwrap_or("(not a string)")
                        ))
                    }
                }
            }
            parsed
        }
        Some(other) => {
            return fail(format!(
                "`status` must be a string or an array of strings, not {other}."
            ))
        }
        None => Vec::new(),
    };

    let base = Patch {
        title: opt_string(&req, "title"),
        detail: opt_string(&req, "detail"),
        evidence: opt_string(&req, "evidence"),
        limitation: opt_string(&req, "limitation"),
        reason: opt_string(&req, "reason"),
        allow_parallel: req
            .arguments
            .get("allow_parallel")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        status: None,
    };

    if statuses.is_empty() {
        // A fields-only update (retitle, add a note) still has to change
        // something, and must not go through the status rules.
        let has_fields = base.title.is_some()
            || base.detail.is_some()
            || base.evidence.is_some()
            || base.limitation.is_some()
            || base.reason.is_some();
        if !has_fields {
            return fail(
                "Nothing to change. Pass `status`, or one of `title`, `detail`, \
                 `evidence`, `limitation`, `reason`.",
            );
        }
        return match list.update(&id, base, ts()) {
            Ok((id, note)) => {
                let ids = vec![id];
                let mut result = save(&scope, &list, "Updated", &ids);
                if let Some(note) = note {
                    result.content = format!("{}\n{note}", result.content);
                }
                result
            }
            Err(e) => fail(e),
        };
    }

    // Apply the status sequence in order. Each step is validated by the model,
    // so `["in_progress", "done"]` legitimately marks the item started and then
    // requires evidence for the `done` step.
    let mut notes = Vec::new();
    for (index, status) in statuses.iter().enumerate() {
        let step = Patch {
            status: Some(*status),
            // Only the last status change needs the caller's field values; an
            // earlier step must not be handed evidence that belongs to a later
            // one. `allow_parallel` applies to every step.
            ..base.clone()
        };
        match list.update(&id, step, ts()) {
            Ok((_, note)) => {
                if let Some(note) = note {
                    notes.push(note);
                }
            }
            Err(e) => {
                let prefix = if statuses.len() > 1 {
                    format!(
                        "step {} of {} (`{status}`) failed. ",
                        index + 1,
                        statuses.len()
                    )
                } else {
                    String::new()
                };
                return fail(format!("{prefix}{}", e.0));
            }
        }
    }

    let mut result = save(&scope, &list, "Updated", &[id]);
    for note in notes {
        result.content = format!("{}\n{note}", result.content);
    }
    result
}

// ---------------------------------------------------------------------------
// todo_remove
// ---------------------------------------------------------------------------

fn remove(req: ToolRequest) -> PluginToolResult {
    let (scope, mut list) = match load(&req) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    let id = req.str_arg("id").trim().to_string();
    if id.is_empty() {
        return fail("`id` is required. Call `todo_list` to see the ids that exist.");
    }
    match list.remove(&id, ts()) {
        Ok(ids) => save(&scope, &list, "Removed", &ids),
        Err(e) => fail(e),
    }
}

// ---------------------------------------------------------------------------
// todo_list / todo_audit
// ---------------------------------------------------------------------------

fn list_tool(req: ToolRequest) -> PluginToolResult {
    let (scope, list) = match load(&req) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    let mut text = render::render_tool_list(&list, "TODO list");
    text.push_str(&format!(
        "\nWorkspace: {}\nSession: {}\nFile: {}",
        scope.workspace,
        scope.session,
        scope.file()
    ));
    PluginToolResult::ok(text).with_meta(render::to_view(&list))
}

fn audit_tool(req: ToolRequest) -> PluginToolResult {
    let (_, list) = match load(&req) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    let audit = list.audit();
    if audit.total == 0 {
        return PluginToolResult::ok(
            "The list is empty, so there is nothing to audit. Plan the work with `todo_plan`.",
        )
        .with_meta(render::to_view(&list));
    }

    let mut out = vec![format!("Audit of {} item(s):", audit.total)];
    if audit.is_clean() {
        out.push("Clean: every item is closed, and every completion has evidence.".to_string());
    } else {
        if !audit.open.is_empty() {
            out.push(format!("Still open: {}.", audit.open.join(", ")));
        }
        if !audit.in_progress.is_empty() {
            out.push(format!(
                "In progress right now: {}. Finish or explain these before starting \
                 anything else.",
                audit.in_progress.join(", ")
            ));
        }
        if !audit.unverified.is_empty() {
            out.push(format!(
                "Unverified completions (done without ever being in progress): {}. \
                 Add `evidence`, or set the honest status.",
                audit.unverified.join(", ")
            ));
        }
        if !audit.missing_evidence.is_empty() {
            out.push(format!(
                "Done without evidence: {}.",
                audit.missing_evidence.join(", ")
            ));
        }
        if !audit.missing_limitation.is_empty() {
            out.push(format!(
                "Partial without a stated limitation: {}.",
                audit.missing_limitation.join(", ")
            ));
        }
    }
    PluginToolResult::ok(out.join("\n")).with_meta(render::to_view(&list))
}

// ---------------------------------------------------------------------------
// The inspector panel
// ---------------------------------------------------------------------------

/// `panels/read` for the `todo` panel: the data the Todo tab draws.
///
/// A panel contributes **data, never markup** — the host forwards this JSON
/// untouched and the frontend renders it with a first-party component (or as
/// read-only JSON when the app has no renderer for it). So everything the tab
/// needs has to be in here: the items in display order, the derived audit, the
/// counts, and the carry-over from the last run.
///
/// Rendering decisions are deliberately *not* made here: this payload carries
/// facts (a status, its evidence, whether it was ever started) and the tab
/// decides how to draw them. That keeps the panel useful in the generic JSON
/// view too.
///
/// An empty or missing list is a legitimate payload, not an error: the tab needs
/// to say "no plan yet" rather than showing a failure.
fn panel_todo(req: PanelRequest) -> Value {
    // The panel request carries the workspace *and* the session directly, so
    // there is no guessing: the tab shows the list of the conversation it is
    // attached to, not the project's most recent one.
    let workspace = req.workspace.trim();
    let session = req.session_id.trim();
    if workspace.is_empty() {
        return json!({
            "error": "the panel was requested without a workspace, so there is no list to read",
            "items": [],
            "counts": {},
        });
    }
    if session.is_empty() {
        return json!({
            "error": "the panel was requested without a session; TODO lists are per session, \
                      so select a session and reopen the tab",
            "workspace": workspace,
            "items": [],
            "counts": {},
        });
    }

    let list = match config::read_for(workspace, session) {
        Ok(list) => list,
        Err(e) => {
            // A corrupt list is reported in the payload so the tab can show the
            // reason. Returning a refused panel instead would hide the very thing
            // the user needs to see.
            return json!({
                "error": e.to_string(),
                "workspace": workspace,
                "session": session,
                "items": [],
                "counts": {},
            });
        }
    };

    let mut view = render::to_view(&list);
    if let Some(object) = view.as_object_mut() {
        object.insert("workspace".into(), json!(workspace));
        object.insert("session".into(), json!(session));
        // The tab shows the file it is reading, so a user can open it directly.
        object.insert(
            "path".into(),
            json!(config::session_file(workspace, session)
                .map(|p| p.display().to_string())
                .unwrap_or_default()),
        );
        // Open-work count, computed here so every renderer agrees without
        // re-deriving it from `items`.
        object.insert("open_count".into(), json!(list.open_items().len()));
    }
    view
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// `before_prompt` — put the list and what is unfinished in front of the model.
///
/// The output is memoised per session and list version, so this is byte-stable
/// while the list is unchanged. That is a hard requirement: the block is part of
/// the system prompt, which providers cache.
fn before_prompt(payload: Value) -> Value {
    let Some(session_id) = payload.get("session_id").and_then(Value::as_str) else {
        return Value::Null;
    };
    let Some(workspace) = payload.get("workspace").and_then(Value::as_str) else {
        return Value::Null;
    };

    sessions().note_workspace(session_id, workspace);
    // The session's own list. `read_for` seeds it from the legacy shared list the
    // first time, so an upgrade does not blank an in-flight conversation.
    let Ok(list) = config::read_for(workspace, session_id) else {
        // A corrupt list must not break the run. The tools report the problem
        // when the agent calls them; the hook stays silent.
        return Value::Null;
    };

    match sessions().prompt_block(session_id, &list) {
        Some(block) => Value::String(block),
        None => match render::prompt_block(&list, &[]) {
            Some(block) => Value::String(block),
            None => Value::Null,
        },
    }
}

/// `on_run_end` — record what was left open and queue the reminder.
fn on_run_end(payload: Value) -> Value {
    let (Some(session_id), Some(run_id)) = (
        payload.get("session_id").and_then(Value::as_str),
        payload.get("run_id").and_then(Value::as_str),
    ) else {
        return Value::Null;
    };
    let workspace = match payload.get("workspace").and_then(Value::as_str) {
        Some(workspace) => workspace.to_string(),
        None => match sessions().session_workspace(session_id) {
            Some(workspace) => workspace,
            None => return Value::Null,
        },
    };

    let Ok(mut list) = config::read_for(&workspace, session_id) else {
        return Value::Null;
    };
    let Some(_line) = sessions().note_run_end(session_id, run_id, &mut list, config::now_ms())
    else {
        return Value::Null;
    };
    if config::write_for(&workspace, session_id, &list).is_err() {
        return Value::Null;
    }
    Value::Null
}

/// `on_tool_result` — notice when this plugin's tools are being used, so a run
/// that mutates the list re-renders the prompt block instead of serving a stale
/// one. Cheap and parameter-free: only the tool name and ok flag are available.
fn on_tool_result(payload: Value) -> Value {
    let Some(tool) = payload.get("tool").and_then(Value::as_str) else {
        return Value::Null;
    };
    if !tool.starts_with("plugin__kc-todo__") {
        return Value::Null;
    }
    if let Some(session_id) = payload.get("session_id").and_then(Value::as_str) {
        // The version bump already invalidates the cache; this keeps the UI's
        // "changed" indicator honest when the write came from elsewhere.
        sessions().invalidate(session_id);
    }
    Value::Null
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

fn main() -> std::io::Result<()> {
    let workspace_arg = json!({
        "type": "string",
        "description": "Absolute project path. Normally unnecessary: the host sends the \
                        call's workspace as context. Pass it only to read another \
                        project's list."
    });
    let session_arg = json!({
        "type": "string",
        "description": "Session id the list belongs to. Normally unnecessary: the host \
                        sends the call's session as context, and lists are per session. \
                        Pass it only to read another session's list deliberately."
    });

    PluginBuilder::new("kc-todo", env!("CARGO_PKG_VERSION"))
        .description(
            "A TODO list that stays true: task/subtask trees, statuses that require \
             proof (done needs evidence, partial needs a limitation), and unfinished \
             work carried into the next prompt.",
        )
        // The list lives in the workspace, so the plugin reads and writes files.
        .permission(permissions::FILES_READ)
        .permission(permissions::FILES_WRITE)
        .tool(
            "todo_plan",
            "Replace the TODO list with a plan. Each task may have subtasks (two levels \
             deep). Use this at the start of a multi-step task, then keep the list \
             current with `todo_update` as you finish each item.",
            json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "The whole plan. An empty array clears the list (needs confirm_clear).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string", "description": "What the task is." },
                                "detail": { "type": "string", "description": "Optional notes, files or acceptance criteria." },
                                "children": {
                                    "type": "array",
                                    "description": "Optional subtasks.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "title": { "type": "string" },
                                            "detail": { "type": "string" }
                                        },
                                        "required": ["title"]
                                    }
                                }
                            },
                            "required": ["title"]
                        }
                    },
                    "confirm_clear": {
                        "type": "boolean",
                        "description": "Required to replace a non-empty list with nothing."
                    },
                    "workspace": workspace_arg,
                    "session": session_arg
                },
                "required": ["tasks"]
            }),
            plan,
        )
        .tool(
            "todo_add",
            "Add one task, or a subtask by passing `parent`. Subtasks nest two levels \
             deep. Prefer `todo_plan` when you know the whole plan up front.",
            json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "What the task is." },
                    "detail": { "type": "string", "description": "Optional notes or acceptance criteria." },
                    "parent": { "type": "string", "description": "Parent task id (`t3`) to add this as a subtask." },
                    "workspace": workspace_arg,
                    "session": session_arg
                },
                "required": ["title"]
            }),
            add,
        )
        .tool(
            "todo_update",
            "Update one item. Statuses: pending (not implemented), in_progress, blocked, \
             deferred, partial (implemented but limited), done (implemented), cancelled. \
             `done` requires `evidence`, `partial` requires `limitation`, and \
             blocked/deferred/cancelled require `reason`. A task cannot be done while a \
             subtask is open, and only one item is in_progress at a time unless you pass \
             `allow_parallel`. Call this in the same turn as the work it records.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Item id, e.g. `t2`." },
                    "status": {
                        "description": "A status, or an array applied in order (e.g. [\"in_progress\",\"done\"]).",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ]
                    },
                    "evidence": {
                        "type": "string",
                        "description": "Required for `done`: what proves it (the command that passed, the behaviour that now works)."
                    },
                    "limitation": {
                        "type": "string",
                        "description": "Required for `partial`: what is still missing or imperfect."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Required for `blocked`, `deferred` and `cancelled`: why work stops."
                    },
                    "title": { "type": "string", "description": "Rename the item." },
                    "detail": { "type": "string", "description": "Replace the notes." },
                    "allow_parallel": {
                        "type": "boolean",
                        "description": "Allow a second in_progress item, for genuinely parallel work."
                    },
                    "workspace": workspace_arg,
                    "session": session_arg
                },
                "required": ["id"]
            }),
            update,
        )
        .tool(
            "todo_remove",
            "Delete one item and its subtasks. Prefer `cancelled` with a reason when the \
             work was considered and dropped, so the decision stays visible.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Item id, e.g. `t2`." },
                    "workspace": workspace_arg,
                    "session": session_arg
                },
                "required": ["id"]
            }),
            remove,
        )
        .tool(
            "todo_list",
            "Read the TODO list, with statuses, subtasks and an audit of anything a \
             reviewer would flag. Call this before deciding what to do next.",
            json!({
                "type": "object",
                "properties": { "workspace": workspace_arg,
                    "session": session_arg }
            }),
            list_tool,
        )
        .tool(
            "todo_audit",
            "Report only what is wrong with the list: open work, items left in progress, \
             and completions with no evidence. Use it before telling the user you are done.",
            json!({
                "type": "object",
                "properties": { "workspace": workspace_arg,
                    "session": session_arg }
            }),
            audit_tool,
        )
        .hook(hooks::BEFORE_PROMPT, before_prompt)
        .hook(hooks::ON_RUN_END, on_run_end)
        .hook(hooks::ON_TOOL_RESULT, on_tool_result)
        // The inspector tab. `plugin__kc-todo__todo` is the hosted id the
        // frontend registers its renderer against.
        .panel_with(
            "todo",
            "Todo",
            Some("check"),
            0,
            panel_todo,
        )
        .run()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("kc-todo-panel-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn panel_request(workspace: &str) -> PanelRequest {
        PanelRequest {
            panel: "todo".into(),
            session_id: "s_one".into(),
            project: None,
            workspace: workspace.into(),
        }
    }

    #[test]
    fn the_panel_payload_carries_what_the_tab_draws() {
        let workspace = temp_workspace("payload");
        let mut list = List::default();
        let task = list.add("Ship it", "the detail", None, 1).unwrap();
        list.add("Test it", "", Some(&task), 2).unwrap();
        list.update(
            &task,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            3,
        )
        .unwrap();
        config::write_for(&workspace, "s_one", &list).unwrap();

        let payload = panel_todo(panel_request(&workspace));

        assert_eq!(payload["workspace"], json!(workspace));
        assert_eq!(payload["session"], json!("s_one"));
        assert!(payload["path"].as_str().unwrap().ends_with("s_one.json"));
        assert_eq!(payload["open_count"], json!(2));
        // Items in display order, subtask after its parent, with the fields a
        // renderer reads.
        let items = payload["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], json!(task));
        assert_eq!(items[0]["status"], json!("in_progress"));
        assert_eq!(items[0]["label"], json!("In progress"));
        assert_eq!(items[1]["depth"], json!(1));
        assert_eq!(items[1]["parent"], json!(task));
        // And the derived audit, so a renderer does not have to re-derive it.
        assert_eq!(payload["audit"]["total"], json!(2));
        assert_eq!(payload["audit"]["clean"], json!(false));

        std::fs::remove_dir_all(&workspace).ok();
    }

    /// The panel is per session: opening the tab in a second conversation in the
    /// same project must not show the first conversation's items.
    #[test]
    fn the_panel_shows_only_the_requesting_sessions_list() {
        let workspace = temp_workspace("panel-split");
        let mut a = List::default();
        a.add("Session A work", "", None, 1).unwrap();
        config::write_for(&workspace, "s_a", &a).unwrap();
        let mut b = List::default();
        b.add("Session B work", "", None, 1).unwrap();
        config::write_for(&workspace, "s_b", &b).unwrap();

        let mut request = panel_request(&workspace);
        request.session_id = "s_a".into();
        let payload = panel_todo(request);
        let items = payload["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], json!("Session A work"));

        std::fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn a_session_less_panel_request_explains_itself() {
        // Lists are per session, so there is nothing sensible to show without one.
        let workspace = temp_workspace("no-session");
        let mut request = panel_request(&workspace);
        request.session_id = "  ".into();
        let payload = panel_todo(request);
        let error = payload["error"].as_str().unwrap_or_default();
        assert!(error.contains("per session"), "{payload}");
        assert_eq!(payload["items"].as_array().unwrap().len(), 0);
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn an_empty_session_answers_an_empty_list_not_an_error() {
        let workspace = temp_workspace("empty");
        let payload = panel_todo(panel_request(&workspace));
        assert!(payload.get("error").is_none(), "{payload}");
        assert_eq!(payload["items"].as_array().unwrap().len(), 0);
        assert_eq!(payload["open_count"], json!(0));
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn a_corrupt_list_reports_the_reason_in_the_payload() {
        let workspace = temp_workspace("corrupt");
        let file = config::session_file(&workspace, "s_one").unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{not json").unwrap();

        let payload = panel_todo(panel_request(&workspace));
        let error = payload["error"].as_str().unwrap_or_default();
        assert!(error.contains("not readable"), "{payload}");
        // The payload still has the shape the tab expects, so it can render the
        // error next to the rest of its chrome instead of a bare failure.
        assert!(payload["items"].as_array().is_some());
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn a_blank_workspace_reports_an_error_rather_than_a_guess() {
        let payload = panel_todo(panel_request("   "));
        let error = payload["error"].as_str().unwrap_or_default();
        assert!(error.contains("without a workspace"), "{payload}");
        assert_eq!(payload["items"].as_array().unwrap().len(), 0);
    }
}
