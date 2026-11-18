//! The nine tools, their schemas and their wording.
//!
//! Descriptions are written for a coding agent that will read them once, at the
//! top of a long context, and then act: short, imperative, and ending with the
//! rule that matters (a status you cannot prove is not a status). The free-search
//! plugin's experience is the guide here - 24 KB of upstream prose became 6 KB of
//! curated text and the model used the tools *better*, because the short version
//! said which tool to call instead of documenting every parameter twice.
//!
//! Every handler resolves the session and workspace from the call context rather
//! than from remembered state, so two sessions in one process cannot cross-talk.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::model::{now_ms, truncate, Criterion, Goal, GoalFile, Source, MAX_CRITERIA};
use crate::render;
use crate::sdk::{PluginBuilder, PluginToolResult, PluginToolResultExt, ToolRequest};
use crate::worker::App;

/// The rule every description ends with. One sentence, repeated 9 times on
/// purpose: it is the cheap defence against a model that writes optimistic
/// statuses.
const RULE: &str = "A status you cannot prove is not a status: evidence or it is not done.";

fn desc(text: &str) -> String {
    format!("{text}\n\n{RULE}")
}

struct Ctx {
    session: String,
    workspace: String,
}

fn ctx(req: &ToolRequest) -> Result<Ctx, String> {
    let session = req
        .session_id()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "this host did not send a tool-call context, so kc-goals cannot tell which session \
             the goal belongs to. Goals need karucode with the `context` field on tools/call."
                .to_string()
        })?;
    let workspace = req
        .workspace()
        .filter(|w| !w.is_empty())
        .ok_or_else(|| {
            "this host did not send a workspace for the call, so kc-goals cannot find where to \
             keep the goal"
                .to_string()
        })?;
    if !crate::store::Store::valid_session_id(session) {
        return Err(format!("`{session}` is not a usable session id"));
    }
    Ok(Ctx {
        session: session.to_string(),
        workspace: workspace.to_string(),
    })
}

fn trimmed(req: &ToolRequest, key: &str) -> String {
    req.str_arg(key).trim().to_string()
}

fn no_empty(value: String, key: &str) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("`{key}` is required and cannot be empty"))
    } else {
        Ok(value)
    }
}

fn bool_or(req: &ToolRequest, key: &str, default: bool) -> bool {
    req.arguments
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// The goal a call means: the one it names, or the session's active one.
fn target(file: &GoalFile, req: &ToolRequest) -> Result<Goal, String> {
    let named = trimmed(req, "goal");
    if !named.is_empty() {
        return file
            .goal(&named)
            .cloned()
            .ok_or_else(|| format!("this session has no goal `{named}`"));
    }
    if let Some(goal) = file.active() {
        return Ok(goal.clone());
    }
    match file.newest_first().first() {
        Some(goal) => Ok((*goal).clone()),
        None => Err(
            "this session has no goal yet. Set one with `goal_set` (a title, the contract as \
             `statement`, and at least one criterion that proves it is done)."
                .to_string(),
        ),
    }
}

fn criteria_from(value: &Value, verify_commands: bool) -> Result<Vec<Criterion>, String> {
    let Some(items) = value.as_array() else {
        return Err(
            "`criteria` is required: an array of what must be true for this goal to be \
             finished, one sentence each (for example \"`cargo test` passes\")."
                .to_string(),
        );
    };
    if items.is_empty() {
        return Err(
            "`criteria` cannot be empty: a goal with no criteria cannot be completed, because \
             nothing would prove it is done."
                .to_string(),
        );
    }
    if items.len() > MAX_CRITERIA {
        return Err(format!(
            "`criteria` has {} entries; at most {MAX_CRITERIA} are allowed. A definition of done \
             with more than that is a wish list - split the work into goals.",
            items.len()
        ));
    }
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let (text, check) = match item {
            Value::String(text) => (text.trim().to_string(), None),
            Value::Object(map) => (
                map.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                map.get("check")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|check| !check.is_empty())
                    .map(str::to_string),
            ),
            _ => (String::new(), None),
        };
        if text.is_empty() {
            return Err(format!(
                "criterion #{} is empty: every entry must say what must be true",
                index + 1
            ));
        }
        if check.is_some() && !verify_commands {
            return Err(format!(
                "criterion #{} has a `check` command, but this plugin is not allowed to run \
                 commands. Either drop the check (and let the evidence be a command you ran \
                 yourself) or, as the operator, set `verify_commands = true` and grant the \
                 plugin the `shell` permission.",
                index + 1
            ));
        }
        let id = format!("c{}", index + 1);
        out.push(match check {
            Some(check) => Criterion::with_check(id, truncate(&text, 300), truncate(&check, 500)),
            None => Criterion::new(id, truncate(&text, 300)),
        });
    }
    Ok(out)
}

pub type AppRef = fn() -> &'static Arc<App>;

pub fn register(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    let builder = register_set(builder, app);
    let builder = register_status(builder, app);
    let builder = register_criterion(builder, app);
    let builder = register_progress(builder, app);
    let builder = register_complete(builder, app);
    let builder = register_pause(builder, app);
    let builder = register_resume(builder, app);
    let builder = register_abandon(builder, app);
    let builder = register_options(builder, app);
    // `goal_check` declares the `shell` class, so the host withholds it unless
    // the registration grants `shell`; at runtime it also checks
    // `app().cfg.verify_commands` so toggling the UI setting works cleanly.
    let builder = register_check(builder, app);
    register_list(builder, app)
}

fn register_set(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_set",
        &desc(
            "Record what the user wants finished, and how anyone can tell it is finished. Call \
             this as soon as the user states a goal; the plugin then keeps this session working \
             until every criterion is met and evidenced, resuming it whenever a run ends early. \
             `criteria` is the definition of done (1-20 entries); an entry may be a string or \
             `{text, check}` where `check` is a command whose exit code proves it. Replaces an open \
             goal only with `replace: true`.",
        ),
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "One line. Shown in the panel and in every continuation."},
                "statement": {"type": "string", "description": "The contract in the user's own words: what exactly must be true at the end. Defaults to the title."},
                "criteria": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "What must be true for the goal to be finished, one sentence each. These are checked one by one and each needs evidence."
                },
                "budget": {
                    "type": "object",
                    "properties": {
                        "max_continuations": {"type": "integer", "description": "Times the plugin may resume this goal (default 25)."},
                        "max_minutes": {"type": "integer", "description": "Wall-clock ceiling in minutes (default 180)."},
                        "max_no_progress": {"type": "integer", "description": "Resumes in a row with no `goal_progress` before the goal is paused (default 3)."},
                        "max_failed_retries": {"type": "integer", "description": "Retries after a failed run before pausing (default 2)."}
                    }
                },
                "replace": {"type": "boolean", "description": "Replace the goal that is currently open. Without this, an open goal is protected."},
                "auto_start": {"type": "boolean", "description": "Default true: the engine starts working on it immediately. false records the goal as paused."}
            },
            "required": ["title", "criteria"]
        }),
        move |req| set(app(), req),
    )
}

fn set(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let title = match no_empty(trimmed(&req, "title"), "title") {
        Ok(title) => truncate(&title, 120),
        Err(error) => return PluginToolResult::err(error),
    };
    let criteria = match criteria_from(
        req.arguments.get("criteria").unwrap_or(&Value::Null),
        app.cfg.verify_commands,
    ) {
        Ok(criteria) => criteria,
        Err(error) => return PluginToolResult::err(error),
    };
    let budget = match req.arguments.get("budget") {
        Some(value) if value.is_object() => app.cfg.default_budget.merged_with(value).clamped(),
        _ => app.cfg.default_budget,
    };
    let statement = truncate(&trimmed(&req, "statement"), 2_000);
    let auto_start = bool_or(&req, "auto_start", true);
    let replace = bool_or(&req, "replace", false);
    let now = now_ms();

    let result = app.store.update(&ctx.workspace, &ctx.session, |file| {
        if let Some(open) = file.active().cloned() {
            if !replace {
                return Err(format!(
                    "goal {} ({}) is still open - {} of {} criteria met. Keep working on it, or \
                     pass `replace: true` if this instruction genuinely supersedes it.",
                    open.id,
                    truncate(&open.title, 80),
                    open.met_criteria(),
                    open.total_criteria()
                ));
            }
            if let Some(goal) = file.goal_mut(&open.id) {
                goal.abandon("replaced by a new goal", now);
            }
        }
        let id = file.next_goal_id();
        let mut goal = Goal::new(
            id.clone(),
            &title,
            &statement,
            criteria,
            budget,
            Source::User,
            now,
        );
        if !auto_start {
            goal.pause("created paused (`auto_start: false`)", now);
        }
        file.goals.push(goal);
        file.refresh_active();
        Ok((id, file.active().cloned()))
    });

    match result {
        Ok((id, Some(goal))) => {
            app.store.remember(&ctx.session, &ctx.workspace);
            // A goal set while the session is idle has no run end to react to.
            // Waking the engine makes the first tick start the work now.
            app.wake();
            PluginToolResult::ok(render::set_text(&goal, now, &app.cfg)).with_meta(json!({
                "goal": id,
                "session_id": ctx.session,
                "criteria": goal.total_criteria(),
                "auto_start": auto_start,
            }))
        }
        Ok((id, None)) => PluginToolResult::ok(format!(
            "goal {id} recorded but not active ({})",
            "`auto_start: false`"
        )),
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_status(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_status",
        &desc(
            "Read the current goal: every criterion, its evidence, the journal, the budget left, \
             and whether the engine will resume this session after the run ends. Call it before \
             concluding that the work is finished, and again after a long stretch of work.",
        ),
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": "Goal id (default: the open one)."}
            }
        }),
        move |req| status(app(), req),
    )
}

fn status(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let file = match app.store.load(&ctx.workspace, &ctx.session) {
        Ok(file) => file,
        Err(error) => return PluginToolResult::err(error),
    };
    match target(&file, &req) {
        Ok(goal) => PluginToolResult::ok(render::status_text(&goal, now_ms(), &app.cfg))
            .with_meta(json!({
                "goal": goal.id,
                "status": goal.status.as_str(),
                "met": goal.met_criteria(),
                "criteria": goal.total_criteria(),
                "continuations": goal.progress.continuations,
            })),
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_criterion(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_criterion",
        &desc(
            "Mark one criterion of the goal met or unmet. `met: true` requires `evidence`: the \
             command that passed, the test name, the file and the behaviour that now works. \
             Without evidence the call is refused, which is the point - a criterion is what makes \
             \"done\" checkable. A criterion that carries a `check` command cannot be marked this \
             way at all: run `goal_check` and let the command decide.",
        ),
        json!({
            "type": "object",
            "properties": {
                "criterion": {"type": "string", "description": "Criterion id, e.g. `c2` (see goal_status)."},
                "met": {"type": "boolean", "description": "True to mark it met, false to unmark it. Default true."},
                "evidence": {"type": "string", "description": "Required when met: what proves it. Quote the command and its result, or name the test and what it asserts."},
                "goal": {"type": "string", "description": "Goal id (default: the open one)."}
            },
            "required": ["criterion"]
        }),
        move |req| criterion(app(), req),
    )
}

fn criterion(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let criterion_id = match no_empty(trimmed(&req, "criterion"), "criterion") {
        Ok(id) => id,
        Err(error) => return PluginToolResult::err(error),
    };
    let met = bool_or(&req, "met", true);
    let evidence = if met {
        Some(trimmed(&req, "evidence"))
    } else {
        None
    };
    let now = now_ms();
    let select = trimmed(&req, "goal");
    let result = app.store.update(&ctx.workspace, &ctx.session, |file| {
        let goal = pick_mut(file, &select)?;
        goal.set_criterion(&criterion_id, met, evidence.as_deref(), now)?;
        Ok(goal.clone())
    });
    match result {
        Ok(goal) => {
            app.wake();
            PluginToolResult::ok(render::criterion_text(&goal, &criterion_id)).with_meta(json!({
                "goal": goal.id,
                "criterion": criterion_id,
                "met": met,
                "met_count": goal.met_criteria(),
                "total": goal.total_criteria(),
            }))
        }
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_progress(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_progress",
        &desc(
            "Record what actually changed and what is next. This is the goal's memory: it is \
             re-stated in every continuation, so a long goal survives compaction, a model change \
             or a restart. It is also what the engine counts as progress - three resumes in a row \
             without it and the goal pauses as a suspected loop.",
        ),
        json!({
            "type": "object",
            "properties": {
                "note": {"type": "string", "description": "One line: the file, command or test that changed. Not a plan."},
                "next": {"type": "string", "description": "The next concrete step, so a future turn can pick it up cold."},
                "goal": {"type": "string", "description": "Goal id (default: the open one)."}
            },
            "required": ["note"]
        }),
        move |req| progress(app(), req),
    )
}

fn progress(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let note = match no_empty(trimmed(&req, "note"), "note") {
        Ok(note) => note,
        Err(error) => return PluginToolResult::err(error),
    };
    let next = {
        let next = trimmed(&req, "next");
        if next.is_empty() {
            None
        } else {
            Some(next)
        }
    };
    let now = now_ms();
    let select = trimmed(&req, "goal");
    let result = app.store.update(&ctx.workspace, &ctx.session, |file| {
        let goal = pick_mut(file, &select)?;
        goal.record_progress(&note, next.as_deref(), now)?;
        Ok(goal.clone())
    });
    match result {
        Ok(goal) => {
            app.wake();
            PluginToolResult::ok(render::progress_text(&goal, now)).with_meta(json!({
                "goal": goal.id,
                "journal": goal.journal.len(),
                "met": goal.met_criteria(),
                "criteria": goal.total_criteria(),
            }))
        }
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_complete(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_complete",
        &desc(
            "Close the goal. Refused while any criterion is unmet or lacks evidence - the refusal \
             lists exactly what is missing. Call it only when the goal is genuinely finished; \
             then the plugin stops resuming the session.",
        ),
        json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string", "description": "Optional last journal line: what was delivered, in one sentence."},
                "goal": {"type": "string", "description": "Goal id (default: the open one)."}
            }
        }),
        move |req| complete(app(), req),
    )
}

fn complete(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let summary = trimmed(&req, "summary");
    let now = now_ms();
    let select = trimmed(&req, "goal");
    let result = app.store.update(&ctx.workspace, &ctx.session, |file| {
        let goal = pick_mut(file, &select)?;
        goal.complete(now)?;
        if !summary.is_empty() {
            let _ = goal.record_progress(&summary, None, now);
        }
        let done = goal.clone();
        file.refresh_active();
        Ok(done)
    });
    match result {
        Ok(goal) => {
            (app.notify)(
                Some(&ctx.session),
                crate::sdk::NotifyKind::RunFinished,
                &format!("Goal done: {}", truncate(&goal.title, 60)),
                &format!(
                    "{} of {} criteria met with evidence after {} continuation(s).",
                    goal.met_criteria(),
                    goal.total_criteria(),
                    goal.progress.continuations
                ),
            );
            app.wake();
            PluginToolResult::ok(render::complete_text(&goal, now)).with_meta(json!({
                "goal": goal.id,
                "status": "done",
                "met": goal.met_criteria(),
                "criteria": goal.total_criteria(),
            }))
        }
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_pause(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_pause",
        &desc(
            "Stop the engine from resuming this goal, and keep the goal. Use it when you are \
             genuinely blocked (a missing credential, a decision only the user can make) instead \
             of ending the turn silently. `goal_resume` re-arms it.",
        ),
        json!({
            "type": "object",
            "properties": {
                "reason": {"type": "string", "description": "Why work cannot continue right now, and what would unblock it."},
                "goal": {"type": "string", "description": "Goal id (default: the open one)."}
            },
            "required": ["reason"]
        }),
        move |req| pause(app(), req),
    )
}

fn pause(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let reason = match no_empty(trimmed(&req, "reason"), "reason") {
        Ok(reason) => reason,
        Err(error) => return PluginToolResult::err(error),
    };
    let now = now_ms();
    let select = trimmed(&req, "goal");
    let result = app.store.update(&ctx.workspace, &ctx.session, |file| {
        let goal = pick_mut(file, &select)?;
        if goal.status == crate::model::Status::Paused {
            return Err(format!("goal {} is already paused", goal.id));
        }
        goal.pause(&reason, now);
        Ok(goal.clone())
    });
    match result {
        Ok(goal) => {
            (app.notify)(
                Some(&ctx.session),
                crate::sdk::NotifyKind::NeedsInput,
                &format!("Goal paused: {}", truncate(&goal.title, 60)),
                &truncate(&reason, 240),
            );
            PluginToolResult::ok(format!(
                "goal {} is paused - the engine will not resume it.\n  reason: {}\n\nIt is kept. \
                 Call goal_resume when the obstacle is gone, or goal_abandon if the goal no \
                 longer applies. Report the blocker to the user now; this is the right way to \
                 stop.",
                goal.id, goal.pause_reason.as_deref().unwrap_or(&reason)
            ))
            .with_meta(json!({"goal": goal.id, "status": "paused"}))
        }
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_resume(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_resume",
        &desc(
            "Re-arm a paused or blocked goal so the engine resumes it again after the next run \
             ends. Also what to call after a human cancelled a run, which pauses the goal rather \
             than fighting the stop.",
        ),
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": "Goal id (default: the open one, else the newest)."}
            }
        }),
        move |req| resume(app(), req),
    )
}

fn resume(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let now = now_ms();
    let select = trimmed(&req, "goal");
    let result = app.store.update(&ctx.workspace, &ctx.session, |file| {
        let goal = pick_mut(file, &select)?;
        goal.resume(now)?;
        let resumed = goal.clone();
        file.refresh_active();
        Ok(resumed)
    });
    match result {
        Ok(goal) => {
            app.wake();
            PluginToolResult::ok(render::status_text(&goal, now, &app.cfg)).with_meta(json!({
                "goal": goal.id,
                "status": "active",
            }))
        }
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_abandon(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_abandon",
        &desc(
            "Drop the goal for good: it stops being resumed and leaves the open list. Use it when \
             the goal no longer applies, not when it is merely hard - `goal_pause` keeps it.",
        ),
        json!({
            "type": "object",
            "properties": {
                "reason": {"type": "string", "description": "Why the goal no longer applies."},
                "goal": {"type": "string", "description": "Goal id (default: the open one)."}
            },
            "required": ["reason"]
        }),
        move |req| abandon(app(), req),
    )
}

fn abandon(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let reason = match no_empty(trimmed(&req, "reason"), "reason") {
        Ok(reason) => reason,
        Err(error) => return PluginToolResult::err(error),
    };
    let now = now_ms();
    let select = trimmed(&req, "goal");
    let result = app.store.update(&ctx.workspace, &ctx.session, |file| {
        let goal = pick_mut(file, &select)?;
        if goal.status == crate::model::Status::Abandoned {
            return Err(format!("goal {} is already abandoned", goal.id));
        }
        goal.abandon(&reason, now);
        let dropped = goal.clone();
        file.refresh_active();
        Ok(dropped)
    });
    match result {
        Ok(goal) => {
            app.wake();
            PluginToolResult::ok(format!(
                "goal {} abandoned - the engine will not resume it.\n  reason: {}\n\nThe goal \
                 stays in the file as history (goal_list shows it).",
                goal.id,
                goal.pause_reason.as_deref().unwrap_or(&reason)
            ))
            .with_meta(json!({"goal": goal.id, "status": "abandoned"}))
        }
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_options(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_options",
        &desc(
            "Read or change this goal's own limits without replacing it: continuation cap, \
             wall clock, stalled-resume threshold and failed-run retries. `0` means no limit, \
             and no limit is the default. Call it with no numbers to see the current limits; \
             pass numbers to change them (omitted ones are left alone).",
        ),
        json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": "Goal id (default: the open one)."},
                "max_continuations": {"type": "integer", "description": "Resumes before the goal pauses. 0 = no cap (default)."},
                "max_minutes": {"type": "integer", "description": "Wall clock in minutes. 0 = no deadline (default)."},
                "max_stalled_resumes": {"type": "integer", "description": "Resumes in a row that do nothing before pausing. 0 = never pause."},
                "max_failed_retries": {"type": "integer", "description": "Retries after a failed run. 0 = retry forever (default)."}
            }
        }),
        move |req| options(app(), req),
    )
}

fn options(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let select = trimmed(&req, "goal");
    let numbers: Vec<(&str, u64)> = ["max_continuations", "max_minutes", "max_stalled_resumes"]
        .into_iter()
        .chain(std::iter::once("max_failed_retries"))
        .filter_map(|key| {
            req.arguments
                .get(key)
                .and_then(Value::as_u64)
                .map(|value| (key, value))
        })
        .collect();
    let now = now_ms();

    let result = app.store.update(&ctx.workspace, &ctx.session, |file| {
        let goal = pick_mut(file, &select)?;
        if goal.status.is_closed() {
            return Err(format!(
                "goal {} is {}; its limits no longer matter. Set a new goal with `goal_set`.",
                goal.id,
                goal.status.as_str()
            ));
        }
        if !numbers.is_empty() {
            let mut budget = goal.budget;
            for (key, value) in &numbers {
                match *key {
                    "max_continuations" => budget.max_continuations = *value as u32,
                    "max_minutes" => budget.max_minutes = *value,
                    "max_stalled_resumes" => budget.max_stalled_resumes = *value as u32,
                    "max_failed_retries" => budget.max_failed_retries = *value as u32,
                    other => return Err(format!("`{other}` is not a limit this goal has")),
                }
            }
            // A changed limit that no longer holds must not leave the goal in a
            // state the engine would immediately pause: the human just said what
            // they want, so the counters that fed the old limit are cleared.
            goal.budget = budget.clamped();
            if goal.continuations_left() != Some(0) {
                goal.progress.stalled_streak = 0;
            }
            goal.updated_at = now;
        }
        Ok(goal.clone())
    });

    match result {
        Ok(goal) => {
            let changed = if numbers.is_empty() {
                "no change (no numbers given)".to_string()
            } else {
                format!("updated {}", numbers.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", "))
            };
            app.wake();
            PluginToolResult::ok(format!(
                "goal {} limits — {} ({} continuations used, {} stalled resumes so far)\n  {}\n\n\
                 Limits are written to the goal file. The panel's Options section shows the same \
                 numbers.",
                goal.id,
                goal.budget.describe(),
                goal.progress.continuations,
                goal.progress.stalled_streak,
                changed
            ))
            .with_meta(json!({
                "goal": goal.id,
                "max_continuations": goal.budget.max_continuations,
                "max_minutes": goal.budget.max_minutes,
                "max_stalled_resumes": goal.budget.max_stalled_resumes,
                "max_failed_retries": goal.budget.max_failed_retries,
            }))
        }
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_check(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    // Declared with the `shell` class on purpose: the host withholds the tool
    // unless the registration grants `shell`, which makes the permission the
    // switch that turns verification on rather than a line of documentation.
    builder.tool_with_class(
        "goal_check",
        &desc(
            "Run the check commands attached to the goal's criteria and record their verdicts. \
             Exit code 0 is the only pass. A criterion with a check cannot be marked met any \
             other way, and goal_complete refuses until every check has passed. Use it after the \
             work is done, and again after a fix.",
        ),
        json!({
            "type": "object",
            "properties": {
                "criterion": {"type": "string", "description": "Run only this criterion's check (default: every criterion that has one)."},
                "goal": {"type": "string", "description": "Goal id (default: the open one)."}
            }
        }),
        Some("shell"),
        move |req| check(app(), req),
    )
}

fn check(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    if !app.cfg.verify_commands {
        return PluginToolResult::err(
            "verification is disabled in this plugin's settings, so check commands are neither \
             run nor trusted. Mark criteria with `goal_criterion` and evidence instead. (An \
             operator can enable it with verify_commands = true and the `shell` permission.)",
        );
    }
    let select = trimmed(&req, "goal");
    let only = trimmed(&req, "criterion");
    // Snapshot the commands first, then run them *outside* the store lock: a
    // check that takes two minutes must not block goal_status.
    let file = match app.store.load(&ctx.workspace, &ctx.session) {
        Ok(file) => file,
        Err(error) => return PluginToolResult::err(error),
    };
    let goal = match target(&file, &req) {
        Ok(goal) => goal,
        Err(error) => return PluginToolResult::err(error),
    };
    if goal.status.is_closed() {
        return PluginToolResult::err(format!(
            "goal {} is {}; it takes no more checks",
            goal.id,
            goal.status.as_str()
        ));
    }
    if !only.is_empty() && goal.criterion(&only).is_none() {
        return PluginToolResult::err(format!("no criterion `{only}` on goal {}", goal.id));
    }
    let targets: Vec<(String, String)> = goal
        .criteria
        .iter()
        .filter(|c| c.check.is_some())
        .filter(|c| only.is_empty() || c.id.eq_ignore_ascii_case(&only))
        .map(|c| (c.id.clone(), c.check.clone().unwrap_or_default()))
        .collect();
    if targets.is_empty() {
        return PluginToolResult::err(if only.is_empty() {
            format!(
                "goal {} has no criteria with a `check` command, so there is nothing to verify. \
                 `goal_criterion` with evidence is the way to close those.",
                goal.id
            )
        } else {
            format!("criterion `{only}` has no `check` command")
        });
    }

    let timeout = Duration::from_secs(app.cfg.check_timeout_secs);
    let mut results = Vec::new();
    for (id, command) in targets {
        match crate::check::run(&ctx.workspace, &command, timeout) {
            Ok(outcome) => results.push((id, outcome)),
            Err(error) => {
                results.push((
                    id,
                    crate::check::CheckOutcome {
                        ok: false,
                        code: None,
                        output: error,
                        timed_out: false,
                        ms: 0,
                    },
                ));
            }
        }
    }

    let now = now_ms();
    let recorded = app.store.update(&ctx.workspace, &ctx.session, |file| {
        let goal = pick_mut(file, &select)?;
        for (id, outcome) in &results {
            if let Some(criterion) = goal
                .criteria
                .iter_mut()
                .find(|c| c.id.eq_ignore_ascii_case(id))
            {
                criterion.record_check(outcome.ok, &outcome.output, now)?;
            }
        }
        Ok(goal.clone())
    });
    match recorded {
        Ok(goal) => {
            app.wake();
            let passed = results.iter().filter(|(_, o)| o.ok).count();
            PluginToolResult::ok(render::check_text(&goal, &results)).with_meta(json!({
                "goal": goal.id,
                "checked": results.len(),
                "passed": passed,
                "met": goal.met_criteria(),
                "criteria": goal.total_criteria(),
            }))
        }
        Err(error) => PluginToolResult::err(error),
    }
}

fn register_list(builder: PluginBuilder, app: AppRef) -> PluginBuilder {
    builder.tool(
        "goal_list",
        &desc(
            "Every goal in this workspace, newest first, with status and score. Use it to see what \
             other sessions are working towards, or to recover a goal you were told about but \
             cannot see.",
        ),
        json!({"type": "object", "properties": {}}),
        move |req| list(app(), req),
    )
}

fn list(app: &App, req: ToolRequest) -> PluginToolResult {
    let ctx = match ctx(&req) {
        Ok(ctx) => ctx,
        Err(error) => return PluginToolResult::err(error),
    };
    let files = app.store.list(&ctx.workspace);
    PluginToolResult::ok(render::list_text(&files, now_ms(), &app.cfg)).with_meta(json!({
        "workspace": ctx.workspace,
        "sessions": files.len(),
        "goals": files.iter().map(|(_, f)| f.goals.len()).sum::<usize>(),
    }))
}

/// Resolve a goal for a mutation: the named one, else the open one. Unlike
/// [`target`], a closed goal is returned as-is so the model methods can explain
/// why it refuses.
fn pick_mut<'a>(file: &'a mut GoalFile, select: &str) -> Result<&'a mut Goal, String> {
    let id = if select.is_empty() {
        file.active_goal
            .clone()
            .or_else(|| file.goals.last().map(|g| g.id.clone()))
    } else {
        Some(select.to_string())
    };
    let Some(id) = id else {
        return Err(
            "this session has no goal yet. Set one with `goal_set` (a title, the contract as \
             `statement`, and at least one criterion that proves it is done)."
                .to_string(),
        );
    };
    file.goal_mut(&id)
        .ok_or_else(|| format!("this session has no goal `{id}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{App, SessionView, Transport};

    struct NoServer;

    impl Transport for NoServer {
        fn session(&self, _id: &str) -> Result<SessionView, String> {
            Ok(SessionView {
                exists: true,
                workspace: "/w".into(),
                tool_calls: 1,
                ..SessionView::default()
            })
        }
        fn post(&self, _id: &str, _text: &str, _mode: crate::worker::PostMode) -> Result<crate::worker::Post, String> {
            Err("no server in this test".into())
        }
    }

    fn app() -> (Arc<App>, std::path::PathBuf) {
        app_with(false)
    }

    /// `verify = true` is the operator opting into `goal_check` running commands.
    fn app_with(verify: bool) -> (Arc<App>, std::path::PathBuf) {
        use crate::config::Settings;
        let root = crate::model::temp_dir("kc-goals-tools");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let cfg = Settings {
            state_dir: root.join("state").to_string_lossy().to_string(),
            verify_commands: verify,
            ..Settings::default()
        };
        let app = App::with_transport(cfg, Box::new(NoServer));
        (app, root)
    }

    fn request(app: &App, name: &str, args: Value) -> ToolRequest {
        let _ = app;
        ToolRequest {
            name: name.into(),
            arguments: args,
            context: Some(crate::sdk::ToolCallContext {
                session_id: "s_1".into(),
                workspace: {
                    // The workspace is threaded through the context in
                    // production; in tests the harness sets it per call.
                    String::new()
                },
                ..Default::default()
            }),
        }
    }

    /// A tool request with the workspace filled in, the way the host sends one.
    fn call(app: &App, workspace: &str, name: &str, args: Value) -> PluginToolResult {
        let mut req = request(app, name, args);
        if let Some(context) = req.context.as_mut() {
            context.workspace = workspace.to_string();
        }
        match name {
            "goal_set" => set(app, req),
            "goal_status" => status(app, req),
            "goal_criterion" => criterion(app, req),
            "goal_progress" => progress(app, req),
            "goal_complete" => complete(app, req),
            "goal_pause" => pause(app, req),
            "goal_resume" => resume(app, req),
            "goal_abandon" => abandon(app, req),
            "goal_options" => options(app, req),
            "goal_check" => check(app, req),
            "goal_list" => list(app, req),
            other => panic!("unknown tool {other}"),
        }
    }

    #[test]
    fn goal_set_records_a_goal_with_numbered_criteria() {
        let (app, root) = app();
        let ws = root.join("ws");
        let result = call(
            &app,
            ws.to_str().unwrap(),
            "goal_set",
            json!({"title": "Ship it", "statement": "Ship it end to end", "criteria": ["builds", "tested"]}),
        );
        assert!(result.ok, "{}", result.content);
        assert!(result.content.contains("goal g1 created"), "{}", result.content);
        assert!(result.content.contains("c1 — builds"), "{}", result.content);
        assert!(result.content.contains("will resume"), "{}", result.content);
        let file = app.store.load(ws.to_str().unwrap(), "s_1").unwrap();
        assert_eq!(file.active_goal.as_deref(), Some("g1"));
        assert_eq!(file.goals[0].criteria.len(), 2);
        assert_eq!(file.goals[0].criteria[1].id, "c2");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_open_goal_is_protected_without_replace() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "first", "criteria": ["x"]}));
        let second = call(&app, &ws, "goal_set", json!({"title": "second", "criteria": ["y"]}));
        assert!(!second.ok);
        assert!(second.content.contains("replace: true"), "{}", second.content);
        let replaced = call(
            &app,
            &ws,
            "goal_set",
            json!({"title": "second", "criteria": ["y"], "replace": true}),
        );
        assert!(replaced.ok, "{}", replaced.content);
        let file = app.store.load(&ws, "s_1").unwrap();
        assert_eq!(file.goals.len(), 2);
        assert_eq!(file.active_goal.as_deref(), Some("g2"));
        assert_eq!(file.goals[0].status, crate::model::Status::Abandoned);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn goal_set_refuses_an_empty_or_oversized_criteria_list() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        let none = call(&app, &ws, "goal_set", json!({"title": "x", "criteria": []}));
        assert!(!none.ok);
        assert!(none.content.contains("cannot be empty"), "{}", none.content);
        let many: Vec<String> = (0..25).map(|i| format!("c{i}")).collect();
        let too_many = call(&app, &ws, "goal_set", json!({"title": "x", "criteria": many}));
        assert!(!too_many.ok);
        assert!(too_many.content.contains("at most"), "{}", too_many.content);
        let missing = call(&app, &ws, "goal_set", json!({"criteria": ["x"]}));
        assert!(!missing.ok);
        assert!(missing.content.contains("`title` is required"), "{}", missing.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn goal_set_honours_a_budget_and_auto_start_false() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        let result = call(
            &app,
            &ws,
            "goal_set",
            json!({
                "title": "parked",
                "criteria": ["x"],
                "auto_start": false,
                "budget": {"max_continuations": 3}
            }),
        );
        assert!(result.ok, "{}", result.content);
        let file = app.store.load(&ws, "s_1").unwrap();
        let goal = &file.goals[0];
        assert_eq!(goal.status, crate::model::Status::Paused);
        assert_eq!(goal.budget.max_continuations, 3);
        // Untouched ceilings stay unlimited.
        assert_eq!(goal.budget.max_minutes, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_criterion_can_only_be_met_with_evidence() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        let refused = call(&app, &ws, "goal_criterion", json!({"criterion": "c1", "met": true}));
        assert!(!refused.ok);
        assert!(refused.content.contains("requires `evidence`"), "{}", refused.content);
        let met = call(
            &app,
            &ws,
            "goal_criterion",
            json!({"criterion": "c1", "met": true, "evidence": "cargo build --release"}),
        );
        assert!(met.ok, "{}", met.content);
        assert!(met.content.contains("criterion c1 is met"), "{}", met.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn completion_is_refused_until_every_criterion_is_met() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(
            &app,
            &ws,
            "goal_set",
            json!({"title": "t", "criteria": ["builds", "tested"]}),
        );
        call(
            &app,
            &ws,
            "goal_criterion",
            json!({"criterion": "c1", "evidence": "build ok"}),
        );
        let refused = call(&app, &ws, "goal_complete", json!({}));
        assert!(!refused.ok);
        assert!(refused.content.contains("1/2 criteria are unmet"), "{}", refused.content);
        assert!(refused.content.contains("c2 - tested"), "{}", refused.content);
        call(
            &app,
            &ws,
            "goal_criterion",
            json!({"criterion": "c2", "evidence": "12 tests pass"}),
        );
        let done = call(&app, &ws, "goal_complete", json!({"summary": "shipped"}));
        assert!(done.ok, "{}", done.content);
        assert!(done.content.contains("is DONE"), "{}", done.content);
        let file = app.store.load(&ws, "s_1").unwrap();
        assert!(file.active_goal.is_none());
        assert_eq!(file.goals[0].status, crate::model::Status::Done);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn progress_appends_to_the_journal() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        let result = call(
            &app,
            &ws,
            "goal_progress",
            json!({"note": "wrote the worker", "next": "test it"}),
        );
        assert!(result.ok, "{}", result.content);
        assert!(result.content.contains("wrote the worker"), "{}", result.content);
        let file = app.store.load(&ws, "s_1").unwrap();
        assert_eq!(file.goals[0].journal.len(), 1);
        assert_eq!(
            file.goals[0].journal[0].next.as_deref(),
            Some("test it")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pause_and_resume_move_the_goal_and_are_reported() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        let paused = call(&app, &ws, "goal_pause", json!({"reason": "waiting on a credential"}));
        assert!(paused.ok, "{}", paused.content);
        assert!(paused.content.contains("will not resume"), "{}", paused.content);
        let again = call(&app, &ws, "goal_pause", json!({"reason": "again"}));
        assert!(!again.ok);
        let resumed = call(&app, &ws, "goal_resume", json!({}));
        assert!(resumed.ok, "{}", resumed.content);
        let file = app.store.load(&ws, "s_1").unwrap();
        assert_eq!(file.goals[0].status, crate::model::Status::Active);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn abandon_closes_the_goal_and_clears_the_pointer() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        let dropped = call(&app, &ws, "goal_abandon", json!({"reason": "no longer wanted"}));
        assert!(dropped.ok, "{}", dropped.content);
        let file = app.store.load(&ws, "s_1").unwrap();
        assert!(file.active_goal.is_none());
        assert_eq!(file.goals[0].status, crate::model::Status::Abandoned);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn status_on_a_session_with_no_goal_explains_how_to_set_one() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        let result = call(&app, &ws, "goal_status", json!({}));
        assert!(!result.ok);
        assert!(result.content.contains("goal_set"), "{}", result.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn list_shows_the_workspace_goals() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        let result = call(&app, &ws, "goal_list", json!({}));
        assert!(result.ok);
        assert!(result.content.contains("g1"), "{}", result.content);
        assert!(result.content.contains("[active]"), "{}", result.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_call_without_a_context_is_refused_with_an_explanation() {
        let (app, _root) = app();
        let result = set(
            &app,
            ToolRequest {
                name: "goal_set".into(),
                arguments: json!({"title": "t", "criteria": ["x"]}),
                context: None,
            },
        );
        assert!(!result.ok);
        assert!(result.content.contains("tool-call context"), "{}", result.content);
    }

    #[test]
    fn goal_ids_are_unique_across_goals_in_a_session() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "one", "criteria": ["a"]}));
        call(
            &app,
            &ws,
            "goal_set",
            json!({"title": "two", "criteria": ["b"], "replace": true}),
        );
        let file = app.store.load(&ws, "s_1").unwrap();
        assert_eq!(file.goals.len(), 2);
        assert_eq!(file.goals[1].id, "g2");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_budget_of_zero_means_unlimited_not_disabled() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(
            &app,
            &ws,
            "goal_set",
            json!({"title": "t", "criteria": ["x"], "budget": {"max_continuations": 0, "max_minutes": 0}}),
        );
        let file = app.store.load(&ws, "s_1").unwrap();
        let goal = &file.goals[0];
        assert_eq!(goal.continuations_left(), None);
        assert_eq!(goal.minutes_left(0), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_check_criterion_cannot_be_marked_met_by_the_agent() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        // Verification is off in this harness, so a check is refused at creation.
        let refused = call(
            &app,
            &ws,
            "goal_set",
            json!({"title": "t", "criteria": [{"text": "builds", "check": "true"}]}),
        );
        assert!(!refused.ok);
        assert!(refused.content.contains("verify_commands"), "{}", refused.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn goal_options_reads_and_changes_a_goals_limits() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        let read = call(&app, &ws, "goal_options", json!({}));
        assert!(read.ok, "{}", read.content);
        assert!(read.content.contains("unlimited continuations"), "{}", read.content);

        let changed = call(
            &app,
            &ws,
            "goal_options",
            json!({"max_continuations": 3, "max_stalled_resumes": 0}),
        );
        assert!(changed.ok, "{}", changed.content);
        let file = app.store.load(&ws, "s_1").unwrap();
        let goal = &file.goals[0];
        assert_eq!(goal.budget.max_continuations, 3);
        assert_eq!(goal.budget.max_stalled_resumes, 0);
        // An omitted limit is left alone.
        assert_eq!(goal.budget.max_minutes, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn goal_options_refuses_a_closed_goal() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        call(&app, &ws, "goal_abandon", json!({"reason": "not wanted"}));
        let refused = call(&app, &ws, "goal_options", json!({"max_continuations": 2}));
        assert!(!refused.ok);
        assert!(refused.content.contains("no longer matter"), "{}", refused.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn goal_check_refuses_when_the_operator_has_not_enabled_it() {
        let (app, root) = app();
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        let refused = call(&app, &ws, "goal_check", json!({}));
        assert!(!refused.ok);
        assert!(refused.content.contains("verify_commands"), "{}", refused.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_check_command_decides_a_criterion_instead_of_the_agent() {
        let (app, root) = app_with(true);
        let ws = root.join("ws").to_string_lossy().to_string();
        let created = call(
            &app,
            &ws,
            "goal_set",
            json!({"title": "t", "criteria": [
                {"text": "the build is green", "check": "true"},
                {"text": "the tests pass", "check": "false"}
            ]}),
        );
        assert!(created.ok, "{}", created.content);
        let checked = call(&app, &ws, "goal_check", json!({}));
        assert!(checked.ok, "{}", checked.content);
        assert!(checked.content.contains("PASS  c1"), "{}", checked.content);
        assert!(checked.content.contains("FAIL  c2"), "{}", checked.content);
        assert!(checked.content.contains("Still failing: c2"), "{}", checked.content);

        let file = app.store.load(&ws, "s_1").unwrap();
        let goal = &file.goals[0];
        assert!(goal.criteria[0].met, "a passing check marks it met");
        assert!(goal.criteria[0]
            .evidence
            .as_deref()
            .unwrap()
            .contains("verified by `true`"));
        assert!(!goal.criteria[1].met);
        assert_eq!(goal.criteria[1].check_ok, Some(false));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_check_criterion_cannot_be_marked_by_hand() {
        let (app, root) = app_with(true);
        let ws = root.join("ws").to_string_lossy().to_string();
        call(
            &app,
            &ws,
            "goal_set",
            json!({"title": "t", "criteria": [{"text": "green", "check": "false"}]}),
        );
        let refused = call(
            &app,
            &ws,
            "goal_criterion",
            json!({"criterion": "c1", "evidence": "I swear it works"}),
        );
        assert!(!refused.ok);
        assert!(refused.content.contains("goal_check"), "{}", refused.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn goal_complete_refuses_while_a_check_has_not_passed() {
        let (app, root) = app_with(true);
        let ws = root.join("ws").to_string_lossy().to_string();
        call(
            &app,
            &ws,
            "goal_set",
            json!({"title": "t", "criteria": [
                {"text": "green", "check": "true"},
                {"text": "tested", "check": "false"}
            ]}),
        );
        call(&app, &ws, "goal_check", json!({}));
        let refused = call(&app, &ws, "goal_complete", json!({}));
        assert!(!refused.ok);
        assert!(refused.content.contains("judged by a command"), "{}", refused.content);
        assert!(refused.content.contains("last run failed"), "{}", refused.content);
        // Fix the failing command and re-check: now it closes.
        let file_path = crate::store::Store::goal_path(&ws, "s_1");
        let mut doc: crate::model::GoalFile =
            serde_json::from_str(&std::fs::read_to_string(&file_path).unwrap()).unwrap();
        doc.goals[0].criteria[1].check = Some("true".into());
        std::fs::write(&file_path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        let checked = call(&app, &ws, "goal_check", json!({"criterion": "c2"}));
        assert!(checked.ok, "{}", checked.content);
        let done = call(&app, &ws, "goal_complete", json!({}));
        assert!(done.ok, "{}", done.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_goal_without_checks_says_there_is_nothing_to_verify() {
        let (app, root) = app_with(true);
        let ws = root.join("ws").to_string_lossy().to_string();
        call(&app, &ws, "goal_set", json!({"title": "t", "criteria": ["builds"]}));
        let nothing = call(&app, &ws, "goal_check", json!({}));
        assert!(!nothing.ok);
        assert!(nothing.content.contains("no criteria with a `check`"), "{}", nothing.content);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_tool_description_carries_the_evidence_rule_and_stays_short() {
        // The schemas are registered by `register`, which needs a builder; the
        // check that matters is on the text, so it is asserted on the constants
        // the builder is given.
        let descriptions = [
            desc("a"),
            desc("b"),
        ];
        for text in descriptions {
            assert!(text.contains("evidence or it is not done"), "{text}");
        }
        assert!(RULE.len() < 120);
    }
}
