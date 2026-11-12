//! Everything a human or a model reads: tool results, the system-prompt block and
//! the inspector panel's payload.
//!
//! Kept apart from the rules so the wording can change without touching the
//! policy, and so the one thing that must never change - the `before_prompt`
//! block's bytes - is a constant rather than a string built at runtime.

use serde_json::{json, Value};

use crate::config::Settings;
use crate::engine;
use crate::model::{Budget, Criterion, Goal, GoalFile, Status};
use crate::worker::LogEntry;

/// The orientation block. **Byte-stable by construction**: no counter, no
/// timestamp, no goal text.
///
/// `before_prompt` output is message 0, and message 0 is what the provider's
/// prefix cache keys on. A block that changed between calls - "3 of 25
/// continuations left" - would make every iteration re-bill the whole context
/// instead of reading it from the cache. The live state goes in the continuation
/// message (an appended user turn) and in `goal_status`, never here.
pub const PROMPT_BLOCK: &str = "[kc-goals] This session has an active goal with acceptance \
criteria. The goal is not finished until every criterion is met and carries evidence. Before \
concluding anything, call goal_status: it states what is left and whether this goal will be \
resumed after a run ends. Record work that lands with goal_progress, and close the goal with \
goal_complete (which refuses while any criterion is unmet). A turn that simply stops does not \
end the goal - the plugin resumes it - so do not stop early, and if you are genuinely blocked \
call goal_pause with the reason rather than ending the turn silently.";

/// `3 min ago`, `2 h 10 min ago`, `just now`.
pub fn ago(then: i64, now: i64) -> String {
    if then <= 0 {
        return "unknown".to_string();
    }
    let secs = ((now - then).max(0)) / 1_000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{} min ago", secs / 60),
        3_600..=172_799 => {
            let hours = secs / 3_600;
            let minutes = (secs % 3_600) / 60;
            if minutes == 0 {
                format!("{hours} h ago")
            } else {
                format!("{hours} h {minutes} min ago")
            }
        }
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// How a criterion's check last went.
fn check_verdict(criterion: &Criterion) -> String {
    match criterion.check_ok {
        Some(true) => "passed".to_string(),
        Some(false) => "FAILED".to_string(),
        None => "never run".to_string(),
    }
}

/// The budget, in words rather than in fractions of a number nobody set.
pub fn budget_line(goal: &Goal, now: i64) -> String {
    let continuations = match goal.continuations_left() {
        None => format!("{} continuations, no cap", goal.progress.continuations),
        Some(left) => format!(
            "{} of {} continuations, {left} left",
            goal.progress.continuations, goal.budget.max_continuations
        ),
    };
    let clock = match goal.minutes_left(now) {
        None => "no deadline".to_string(),
        Some(left) => format!("{left} min left of {}", goal.budget.max_minutes),
    };
    let stalls = if goal.budget.max_stalled_resumes == 0 {
        "stall pause off".to_string()
    } else {
        format!(
            "{}/{} stalled resumes",
            goal.progress.stalled_streak, goal.budget.max_stalled_resumes
        )
    };
    format!("{continuations}; {clock}; {stalls}")
}

/// One goal, in full. This is what `goal_status` returns because it is what the
/// model needs to decide its next action without guessing.
pub fn status_text(goal: &Goal, now: i64, cfg: &Settings) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "goal {} — {} [{}]  (set by {}, {}; updated {})\n",
        goal.id,
        goal.title,
        goal.status.as_str(),
        goal.source.as_str(),
        ago(goal.created_at, now),
        ago(goal.updated_at, now)
    ));
    if goal.statement.trim() != goal.title.trim() {
        out.push_str(&format!("  contract: {}\n", goal.statement.trim()));
    }
    if let Some(reason) = &goal.pause_reason {
        out.push_str(&format!("  why it stopped: {reason}\n"));
    }
    out.push_str(&format!(
        "\ncriteria {}/{} met\n",
        goal.met_criteria(),
        goal.total_criteria()
    ));
    if goal.criteria.is_empty() {
        out.push_str("  (none — this goal cannot be completed until it has criteria)\n");
    }
    for criterion in &goal.criteria {
        out.push_str(&format!(
            "  [{}] {} — {}\n",
            if criterion.met { "x" } else { " " },
            criterion.id,
            criterion.text
        ));
        if let Some(check) = criterion.check.as_deref() {
            out.push_str(&format!(
                "      check: {}  → {}\n",
                check,
                check_verdict(criterion)
            ));
        }
        // Evidence is shown for a failed check too: "why it is not met" is the
        // most useful line in the whole status.
        if let Some(evidence) = criterion.evidence.as_deref() {
            out.push_str(&format!("      evidence: {evidence}\n"));
        } else if criterion.met {
            out.push_str("      evidence: (none recorded)\n");
        }
    }
    out.push_str(&format!("\nbudget: {}\n", budget_line(goal, now)));
    if goal.progress.continuations > 0 || goal.progress.last_run_status.is_some() {
        out.push_str(&format!(
            "runs: {} resumptions; last run {} ({})\n",
            goal.progress.continuations,
            goal.progress.last_run_id.as_deref().unwrap_or("none"),
            goal.progress.last_run_status.as_deref().unwrap_or("unknown")
        ));
    }
    out.push('\n');
    out.push_str(&engine::explain(goal, now, cfg));
    out.push('\n');

    if !goal.journal.is_empty() {
        out.push_str(&format!(
            "\njournal ({} entries, last 5):\n",
            goal.journal.len()
        ));
        for entry in goal.journal.iter().rev().take(5).rev() {
            match entry.next.as_deref() {
                Some(next) => out.push_str(&format!(
                    "  - [{}] {} (next: {})\n",
                    ago(entry.at, now),
                    entry.note,
                    next
                )),
                None => out.push_str(&format!("  - [{}] {}\n", ago(entry.at, now), entry.note)),
            }
        }
    }

    out.push_str("\nnext: ");
    out.push_str(&next_hint(goal));
    out.push('\n');
    out
}

/// The one thing to do next, derived from the goal's own state. A status read
/// that ends with a concrete instruction is worth more than one that ends with a
/// summary.
pub fn next_hint(goal: &Goal) -> String {
    match goal.status {
        Status::Done => "nothing - this goal is done. Set the next one with `goal_set`.".into(),
        Status::Abandoned => "nothing - it was abandoned. Set the next one with `goal_set`.".into(),
        Status::Paused | Status::Blocked => {
            "call `goal_resume` if work should continue, or leave it stopped.".into()
        }
        Status::Active => {
            if goal.criteria.is_empty() {
                return "add criteria with `goal_set` (replace: true) - nothing can prove this goal \
                        is done yet."
                    .into();
            }
            if goal.remaining().is_empty() {
                return "every criterion is met: call `goal_complete`.".into();
            }
            if let Some(pending) = goal.criteria.iter().find(|c| c.check_pending()) {
                return format!(
                    "run `goal_check` for `{}` (the command `{}` decides), then close the goal.",
                    pending.id,
                    pending.check.as_deref().unwrap_or("")
                );
            }
            let first = goal.remaining()[0].clone();
            match goal.journal.last().and_then(|e| e.next.as_deref()) {
                Some(next) => format!(
                    "{next} — then mark `{}` met with its evidence when it is true.",
                    first.id
                ),
                None => format!(
                    "do the work for `{}` ({}), record it with `goal_progress`, then mark it met.",
                    first.id, first.text
                ),
            }
        }
    }
}

pub fn set_text(goal: &Goal, now: i64, cfg: &Settings) -> String {
    let mut out = format!(
        "goal {} created — {} [{}]\n  criteria: {}\n  budget: {}\n\n",
        goal.id,
        goal.title,
        goal.status.as_str(),
        goal.criteria.len(),
        goal.budget.describe()
    );
    for criterion in &goal.criteria {
        match criterion.check.as_deref() {
            Some(check) => out.push_str(&format!(
                "  {} — {}  (checked by `{}`)\n",
                criterion.id, criterion.text, check
            )),
            None => out.push_str(&format!("  {} — {}\n", criterion.id, criterion.text)),
        }
    }
    out.push('\n');
    out.push_str(&engine::explain(goal, now, cfg));
    out.push_str(
        "\nThe goal is now recorded. Work on it in this turn; when the turn ends with the goal \
         open, the plugin resumes it automatically.\n",
    );
    out
}

pub fn criterion_text(goal: &Goal, criterion_id: &str) -> String {
    let criterion = goal.criterion(criterion_id);
    let met = goal.met_criteria();
    match criterion {
        Some(c) if c.met => format!(
            "criterion {} is met — {} ({} of {} criteria met)\n  evidence: {}\n\n{}",
            c.id,
            c.text,
            met,
            goal.total_criteria(),
            c.evidence.as_deref().unwrap_or("(none)"),
            if goal.remaining().is_empty() {
                "Every criterion is met: call `goal_complete`.".to_string()
            } else {
                format!(
                    "Still unmet: {}. Do the next one.",
                    goal.remaining()
                        .iter()
                        .map(|c| c.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ),
        Some(c) => format!(
            "criterion {} is NOT met — {} ({} of {} criteria met)",
            c.id,
            c.text,
            met,
            goal.total_criteria()
        ),
        None => format!("no criterion `{criterion_id}` on goal {}", goal.id),
    }
}

pub fn progress_text(goal: &Goal, _now: i64) -> String {
    let last = goal.journal.last();
    format!(
        "progress recorded on goal {} ({} of {} criteria met)\n  note: {}\n  next: {}\n  \
         journal: {} entries; the stall counter is back to {}/{} (any tool call resets it)\n\n\
         Keep going: {}",
        goal.id,
        goal.met_criteria(),
        goal.total_criteria(),
        last.map(|e| e.note.as_str()).unwrap_or(""),
        last.and_then(|e| e.next.as_deref())
            .unwrap_or("(not stated)"),
        goal.journal.len(),
        goal.progress.stalled_streak,
        if goal.budget.max_stalled_resumes == 0 {
            "∞".to_string()
        } else {
            goal.budget.max_stalled_resumes.to_string()
        },
        next_hint(goal)
    )
}

/// The result of running a criterion's check command.
pub fn check_text(goal: &Goal, results: &[(String, crate::check::CheckOutcome)]) -> String {
    let mut out = String::new();
    for (id, outcome) in results {
        let verdict = if outcome.ok { "PASS" } else { "FAIL" };
        out.push_str(&format!("{verdict}  {id} — {}\n", outcome.summary()));
        if !outcome.output.trim().is_empty() {
            for line in outcome.output.trim().lines().take(12) {
                out.push_str(&format!("      {line}\n"));
            }
        }
    }
    out.push_str(&format!(
        "\n{} of {} criteria met on goal {}.\n",
        goal.met_criteria(),
        goal.total_criteria(),
        goal.id
    ));
    let failed: Vec<&str> = results
        .iter()
        .filter(|(_, o)| !o.ok)
        .map(|(id, _)| id.as_str())
        .collect();
    if failed.is_empty() {
        out.push_str("Every check passed. If that is all of them, call `goal_complete`.\n");
    } else {
        out.push_str(&format!(
            "Still failing: {}. Fix the work, then run `goal_check` again - a criterion with a \
             check cannot be marked met any other way.\n",
            failed.join(", ")
        ));
    }
    out.push_str(&format!("\nnext: {}", next_hint(goal)));
    out
}

pub fn complete_text(goal: &Goal, now: i64) -> String {
    let mut out = format!(
        "goal {} is DONE — {}\n  {} of {} criteria met, each with evidence:\n",
        goal.id,
        goal.title,
        goal.met_criteria(),
        goal.total_criteria()
    );
    for criterion in goal.met() {
        out.push_str(&format!(
            "  [x] {} — {} | {}\n",
            criterion.id,
            criterion.text,
            criterion.evidence.as_deref().unwrap_or("(none)")
        ));
    }
    out.push_str(&format!(
        "\nThe engine will not resume this goal ({} continuations, closed {}). Nothing is running \
         on it any more; report the outcome to the user.\n",
        goal.progress.continuations,
        ago(goal.updated_at, now)
    ));
    out
}

pub fn list_text(files: &[(String, GoalFile)], _now: i64, cfg: &Settings) -> String {
    if files.is_empty() {
        return "No goals in this workspace. Set one with `goal_set` (title, statement and at \
                least one criterion that proves it is done)."
            .to_string();
    }
    let mut out = String::new();
    let mut open = 0;
    for (session, file) in files {
        for goal in file.newest_first() {
            if goal.status.is_open() {
                open += 1;
            }
            out.push_str(&format!(
                "{}  [{}]  {}/{}  {}  — session {}  ({} continuations)\n",
                goal.id,
                goal.status.as_str(),
                goal.met_criteria(),
                goal.total_criteria(),
                goal.title,
                session,
                goal.progress.continuations
            ));
        }
    }
    out.push_str(&format!(
        "\n{} goal(s) in this workspace, {} open. Engine: {}.",
        files.iter().map(|(_, f)| f.goals.len()).sum::<usize>(),
        open,
        if cfg.enabled && cfg.auto_continue {
            "resuming stopped runs"
        } else {
            "off (auto-continue disabled)"
        }
    ));
    out.push('\n');
    out
}

/// The panel payload's version marker.
///
/// A renderer keys off this, so a plugin and a renderer can be upgraded
/// independently: the app draws a payload it knows and falls back to the JSON
/// view for one it does not. Bump it when a field changes meaning, not when one
/// is added.
pub const PANEL_SCHEMA: &str = "kc-goals/panel/2";

/// One knob, as a UI needs it: what it is called, what it is now, and what its
/// numbers mean. `kind` is a hint ("switch", "number", "text"), never markup.
fn option(
    key: &str,
    label: &str,
    kind: &str,
    value: Value,
    unit: &str,
    zero_means: Option<&str>,
) -> Value {
    let mut out = json!({
        "key": key,
        "label": label,
        "kind": kind,
        "value": value,
    });
    let object = out.as_object_mut().expect("json object");
    if !unit.is_empty() {
        object.insert("unit".into(), json!(unit));
    }
    if let Some(zero) = zero_means {
        object.insert("zero_means".into(), json!(zero));
    }
    out
}

/// The engine's knobs, for a UI that wants to show (or explain) them.
fn engine_options(cfg: &Settings) -> Vec<Value> {
    let budget = cfg.default_budget;
    vec![
        option(
            "auto_continue",
            "Resume stopped runs",
            "switch",
            json!(cfg.auto_continue),
            "",
            None,
        ),
        option(
            "prompt",
            "Orientation block in the prompt",
            "switch",
            json!(cfg.prompt),
            "",
            None,
        ),
        option(
            "verify_commands",
            "Command-checked criteria",
            "switch",
            json!(cfg.verify_commands),
            "",
            None,
        ),
        option(
            "max_continuations",
            "Continuation cap",
            "number",
            json!(budget.max_continuations),
            "resumes",
            Some("no cap"),
        ),
        option(
            "max_minutes",
            "Wall clock",
            "number",
            json!(budget.max_minutes),
            "minutes",
            Some("no deadline"),
        ),
        option(
            "max_stalled_resumes",
            "Pause after",
            "number",
            json!(budget.max_stalled_resumes),
            "stalled resumes",
            Some("never"),
        ),
        option(
            "max_failed_retries",
            "Retry a failed run",
            "number",
            json!(budget.max_failed_retries),
            "times",
            Some("forever"),
        ),
        option(
            "tick_secs",
            "Reconcile every",
            "number",
            json!(cfg.tick_secs),
            "seconds",
            None,
        ),
        option(
            "min_gap_secs",
            "Minimum gap",
            "number",
            json!(cfg.min_gap_secs),
            "seconds",
            None,
        ),
    ]
}

/// A goal's own limits, in the same shape, because they are what a UI would let
/// a human change for one goal.
fn goal_options(goal: &Goal) -> Vec<Value> {
    vec![
        option(
            "max_continuations",
            "Continuation cap",
            "number",
            json!(goal.budget.max_continuations),
            "resumes",
            Some("no cap"),
        ),
        option(
            "max_minutes",
            "Wall clock",
            "number",
            json!(goal.budget.max_minutes),
            "minutes",
            Some("no deadline"),
        ),
        option(
            "max_stalled_resumes",
            "Pause after",
            "number",
            json!(goal.budget.max_stalled_resumes),
            "stalled resumes",
            Some("never"),
        ),
        option(
            "max_failed_retries",
            "Retry a failed run",
            "number",
            json!(goal.budget.max_failed_retries),
            "times",
            Some("forever"),
        ),
    ]
}

/// What a UI can offer for one goal.
///
/// There is **no route that executes a tool**: a panel cannot call `goal_pause`.
/// The only way a client can make the agent act is to send it a message, so each
/// action carries the exact `instruction` to post to
/// `POST /api/sessions/:id/messages`. A renderer draws a button and sends that
/// text; it never invents the wording itself, so the plugin stays the one place
/// that knows what each operation is called.
fn actions(goal: &Goal) -> Vec<Value> {
    let mut out = Vec::new();
    let mut push = |id: &str,
                    label: &str,
                    kind: &str,
                    enabled: bool,
                    hint: &str,
                    instruction: String| {
        out.push(json!({
            "id": id,
            "label": label,
            "kind": kind,
            "enabled": enabled,
            "hint": hint,
            "instruction": instruction,
        }));
    };

    if goal.status.is_open() {
        push(
            "status",
            "What is left?",
            "secondary",
            true,
            "Asks the agent for a status read; costs one turn.",
            format!(
                "Call plugin__kc-goals__goal_status for {} and continue the work from there.",
                goal.id
            ),
        );
        if goal.criteria.iter().any(|c| c.check_pending()) {
            push(
                "check",
                "Run the checks",
                "primary",
                true,
                "Runs the criteria's check commands and records the verdicts.",
                format!("Call plugin__kc-goals__goal_check for {} now.", goal.id),
            );
        }
        let can_complete = !goal.criteria.is_empty()
            && goal.remaining().is_empty()
            && !goal.criteria.iter().any(|c| c.check_pending());
        push(
            "complete",
            "Complete",
            "primary",
            can_complete,
            if can_complete {
                "Closes the goal and stops the engine."
            } else {
                "Disabled until every criterion is met and its check passed."
            },
            format!(
                "Call plugin__kc-goals__goal_complete for {} with a one-line summary.",
                goal.id
            ),
        );
        push(
            "pause",
            "Pause",
            "secondary",
            true,
            "Stops resuming; the goal is kept and can be resumed.",
            format!(
                "Call plugin__kc-goals__goal_pause for {} with the reason you are stopping.",
                goal.id
            ),
        );
        push(
            "abandon",
            "Abandon",
            "danger",
            true,
            "Drops the goal for good. It stays in the file as history.",
            format!(
                "Call plugin__kc-goals__goal_abandon for {} because it no longer applies.",
                goal.id
            ),
        );
    } else if goal.status != Status::Done {
        push(
            "resume",
            "Resume",
            "primary",
            true,
            "Re-arms the goal so the engine resumes it after the next run.",
            format!(
                "Call plugin__kc-goals__goal_resume for {} and continue the work.",
                goal.id
            ),
        );
    }
    out
}

/// One goal as panel JSON. The panel is *data*; the frontend renders it.
pub fn card(goal: &Goal, session_id: &str, now: i64, cfg: &Settings, current_session: &str) -> Value {
    let remaining: Vec<Value> = goal
        .remaining()
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "text": c.text,
                "check": c.check,
                "check_ok": c.check_ok,
            })
        })
        .collect();
    let criteria: Vec<Value> = goal
        .criteria
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "text": c.text,
                "met": c.met,
                "evidence": c.evidence,
                "met_at": c.met_at,
                "check": c.check.as_ref().map(|command| json!({
                    "command": command,
                    "ok": c.check_ok,
                    "checked_at": c.checked_at,
                })),
            })
        })
        .collect();
    let journal: Vec<Value> = goal
        .journal
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(|entry| {
            json!({
                "at": entry.at,
                "ago": ago(entry.at, now),
                "note": entry.note,
                "next": entry.next,
            })
        })
        .collect();

    json!({
        "goal": goal.id,
        "session_id": session_id,
        "is_current_session": session_id == current_session,
        "title": goal.title,
        "statement": goal.statement,
        "status": goal.status.as_str(),
        "source": goal.source.as_str(),
        "met": goal.met_criteria(),
        "criteria": goal.total_criteria(),
        "score": goal.score(),
        "progress_pct": if goal.total_criteria() == 0 {
            0
        } else {
            (goal.met_criteria() * 100 / goal.total_criteria()) as u64
        },
        "created": ago(goal.created_at, now),
        "updated": ago(goal.updated_at, now),
        "continuations": goal.progress.continuations,
        "stalled_resumes": goal.progress.stalled_streak,
        "minutes_left": goal.minutes_left(now),
        "pause_reason": goal.pause_reason,
        "engine_note": goal.progress.engine_note,
        "engine": engine::explain(goal, now, cfg),
        "budget_text": budget_line(goal, now),
        "last_progress": goal.journal.last().map(|e| e.note.clone()),
        "next": next_hint(goal),
        "checks": goal.criteria.iter().filter(|c| c.check.is_some()).count(),
        "checks_pending": goal.criteria.iter().filter(|c| c.check_pending()).count(),
        "remaining": remaining,
        "criteria_detail": criteria,
        "options": goal_options(goal),
        "actions": actions(goal),
        "journal": journal,
    })
}

pub fn panel(
    session_id: &str,
    workspace: &str,
    files: &[(String, GoalFile)],
    now: i64,
    cfg: &Settings,
    log: &[LogEntry],
) -> Value {
    let mut goals = Vec::new();
    let mut total = 0usize;
    for (session, file) in files {
        for goal in file.newest_first() {
            total += 1;
            goals.push(card(goal, session, now, cfg, session_id));
        }
    }
    // The session the tab was opened for comes first, so the tab answers the
    // question a reader actually has before it shows history.
    goals.sort_by_key(|g| !g["is_current_session"].as_bool().unwrap_or(false));
    let open = goals.iter().filter(|g| g["status"] == "active").count();

    json!({
        "schema": PANEL_SCHEMA,
        "session_id": session_id,
        "workspace": workspace,
        "engine": {
            "enabled": cfg.enabled,
            "auto_continue": cfg.auto_continue,
            "prompt": cfg.prompt,
            "tick_secs": cfg.tick_secs,
            "min_gap_secs": cfg.min_gap_secs,
            "server": cfg.server,
            "verify_commands": cfg.verify_commands,
            "check_timeout_secs": cfg.check_timeout_secs,
            "default_budget": {
                "max_continuations": cfg.default_budget.max_continuations,
                "max_minutes": cfg.default_budget.max_minutes,
                "max_stalled_resumes": cfg.default_budget.max_stalled_resumes,
                "max_failed_retries": cfg.default_budget.max_failed_retries,
            },
            "default_budget_text": budget_summary(&cfg.default_budget),
            "options": engine_options(cfg),
            // Said once, here, so a renderer does not have to explain it: the
            // panel is read-only data, and every button posts a message.
            "actions_note": "A panel cannot call tools: each action sends its instruction to the session as a user turn.",
        },
        "open": open,
        "total": total,
        "goals": goals,
        "log": log.iter().rev().take(20).map(|entry| json!({
            "at": ago(entry.at_ms, now),
            "session": entry.session_id,
            "run": entry.run_id,
            "kind": entry.kind,
            "outcome": entry.outcome,
        })).collect::<Vec<_>>(),
    })
}

/// The budget, for a settings screen: `unlimited` reads better than `0`.
pub fn budget_summary(budget: &Budget) -> String {
    budget.describe()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Criterion, Source};

    const NOW: i64 = 1_800_000_000_000;

    fn goal() -> Goal {
        Goal::new(
            "g1",
            "Ship it",
            "Ship it end to end",
            vec![
                Criterion::new("c1", "builds"),
                Criterion::new("c2", "tested"),
            ],
            Budget::default(),
            Source::User,
            NOW - 600_000,
        )
    }

    #[test]
    fn the_prompt_block_is_stable_and_mentions_its_own_contract() {
        assert!(PROMPT_BLOCK.starts_with("[kc-goals]"));
        assert!(PROMPT_BLOCK.contains("goal_status"));
        assert!(PROMPT_BLOCK.contains("goal_complete"));
        assert!(PROMPT_BLOCK.contains("goal_pause"));
        assert!(PROMPT_BLOCK.len() < 1_200, "the block is {} bytes", PROMPT_BLOCK.len());
        // No numbers that could drift between calls.
        assert!(!PROMPT_BLOCK.chars().any(|c| c.is_ascii_digit()));
    }

    #[test]
    fn status_lists_every_criterion_with_evidence_and_the_engine_verdict() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("cargo build"), NOW).unwrap();
        let text = status_text(&g, NOW, &Settings::default());
        assert!(text.contains("[x] c1 — builds"), "{text}");
        assert!(text.contains("evidence: cargo build"), "{text}");
        assert!(text.contains("[ ] c2 — tested"), "{text}");
        assert!(text.contains("1/2 met"), "{text}");
        assert!(text.contains("engine: will resume"), "{text}");
        assert!(text.contains("next:"), "{text}");
    }

    #[test]
    fn an_unlimited_budget_reads_as_unlimited_and_not_as_zero() {
        let g = goal();
        let text = status_text(&g, NOW, &Settings::default());
        assert!(text.contains("no cap"), "{text}");
        assert!(text.contains("no deadline"), "{text}");
        assert!(!text.contains("0 of 0"), "{text}");
    }

    #[test]
    fn a_criterion_with_a_check_shows_its_command_and_verdict() {
        let mut g = goal();
        g.criteria[0] = Criterion::with_check("c1", "builds", "cargo build -q");
        g.criteria[1] = Criterion::with_check("c2", "tested", "cargo test -q");
        g.criteria[1].record_check(false, "3 tests failed", NOW).unwrap();
        let text = status_text(&g, NOW, &Settings::default());
        assert!(text.contains("check: cargo build -q  → never run"), "{text}");
        assert!(text.contains("check: cargo test -q  → FAILED"), "{text}");
        assert!(text.contains("verification FAILED"), "{text}");
    }

    #[test]
    fn the_next_hint_asks_for_a_check_when_one_is_pending() {
        let mut g = goal();
        g.criteria[0] = Criterion::with_check("c1", "builds", "cargo build -q");
        g.criteria[0].record_check(true, "", NOW).unwrap();
        g.criteria[1] = Criterion::with_check("c2", "tested", "cargo test -q");
        let hint = next_hint(&g);
        assert!(hint.contains("goal_check"), "{hint}");
        assert!(hint.contains("c2"), "{hint}");
    }

    #[test]
    fn a_done_goal_says_there_is_nothing_next() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("build"), NOW).unwrap();
        g.set_criterion("c2", true, Some("tests"), NOW).unwrap();
        g.complete(NOW).unwrap();
        let text = status_text(&g, NOW, &Settings::default());
        assert!(text.contains("[done]"), "{text}");
        assert!(text.contains("not resuming"), "{text}");
        assert!(text.contains("nothing - this goal is done"), "{text}");
    }

    #[test]
    fn the_next_hint_follows_the_first_unmet_criterion_then_the_journal() {
        let mut g = goal();
        assert!(next_hint(&g).contains("c1"));
        g.set_criterion("c1", true, Some("build"), NOW).unwrap();
        assert!(next_hint(&g).contains("c2"));
        g.record_progress("wrote the tests", Some("run them"), NOW).unwrap();
        assert!(next_hint(&g).contains("run them"), "{}", next_hint(&g));
        g.set_criterion("c2", true, Some("green"), NOW).unwrap();
        assert!(next_hint(&g).contains("goal_complete"));
    }

    #[test]
    fn a_goal_with_no_criteria_is_told_what_to_do_about_it() {
        let g = Goal::new(
            "g1",
            "vague",
            "vague",
            vec![],
            Budget::default(),
            Source::Agent,
            NOW - 600_000,
        );
        assert!(next_hint(&g).contains("add criteria"));
        assert!(status_text(&g, NOW, &Settings::default()).contains("cannot be completed"));
    }

    #[test]
    fn ago_reads_like_a_person_wrote_it() {
        assert_eq!(ago(NOW, NOW), "just now");
        assert_eq!(ago(NOW - 120_000, NOW), "2 min ago");
        assert_eq!(ago(NOW - 3_600_000, NOW), "1 h ago");
        assert_eq!(ago(NOW - 5_400_000, NOW), "1 h 30 min ago");
        assert_eq!(ago(NOW - 86_400_000 * 2, NOW), "2 days ago");
        assert_eq!(ago(0, NOW), "unknown");
    }

    #[test]
    fn a_progress_result_echoes_the_note_and_the_next_step() {
        let mut g = goal();
        g.record_progress("wrote the engine", Some("wire the worker"), NOW)
            .unwrap();
        let text = progress_text(&g, NOW);
        assert!(text.contains("wrote the engine"), "{text}");
        assert!(text.contains("wire the worker"), "{text}");
        assert!(text.contains("stall counter"), "{text}");
    }

    #[test]
    fn a_check_result_shows_each_verdict_and_says_what_to_do() {
        let mut g = goal();
        g.criteria[0] = Criterion::with_check("c1", "builds", "true");
        g.criteria[1] = Criterion::with_check("c2", "tested", "false");
        let results = vec![
            (
                "c1".to_string(),
                crate::check::CheckOutcome {
                    ok: true,
                    code: Some(0),
                    output: "ok".into(),
                    timed_out: false,
                    ms: 3,
                },
            ),
            (
                "c2".to_string(),
                crate::check::CheckOutcome {
                    ok: false,
                    code: Some(1),
                    output: "2 tests failed".into(),
                    timed_out: false,
                    ms: 5,
                },
            ),
        ];
        let text = check_text(&g, &results);
        assert!(text.contains("PASS  c1"), "{text}");
        assert!(text.contains("FAIL  c2"), "{text}");
        assert!(text.contains("2 tests failed"), "{text}");
        assert!(text.contains("Still failing: c2"), "{text}");
    }

    #[test]
    fn a_completed_result_shows_every_evidence_and_stops_the_engine() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("build ok"), NOW).unwrap();
        g.set_criterion("c2", true, Some("12 tests"), NOW).unwrap();
        g.complete(NOW).unwrap();
        let text = complete_text(&g, NOW);
        assert!(text.contains("is DONE"), "{text}");
        assert!(text.contains("build ok") && text.contains("12 tests"), "{text}");
        assert!(text.contains("will not resume"), "{text}");
    }

    #[test]
    fn an_empty_list_explains_how_to_start_one() {
        let text = list_text(&[], NOW, &Settings::default());
        assert!(text.contains("No goals in this workspace"), "{text}");
        assert!(text.contains("goal_set"), "{text}");
    }

    #[test]
    fn the_panel_payload_names_the_session_the_workspace_and_the_engine() {
        let mut file = GoalFile::new("s_1");
        file.goals.push(goal());
        file.refresh_active();
        let files = vec![("s_1".to_string(), file)];
        let data = panel("s_1", "/w", &files, NOW, &Settings::default(), &[]);
        assert_eq!(data["session_id"], "s_1");
        assert_eq!(data["workspace"], "/w");
        assert_eq!(data["open"], 1);
        assert_eq!(data["goals"][0]["goal"], "g1");
        assert_eq!(data["goals"][0]["score"], "0/2");
        assert_eq!(data["engine"]["auto_continue"], true);
        assert_eq!(data["engine"]["verify_commands"], false);
        assert!(data["goals"][0]["engine"].as_str().unwrap().contains("will resume"));
        assert!(data["goals"][0]["max_continuations"].is_null() || data["goals"][0]["max_continuations"] == 0);
    }

    #[test]
    fn the_panel_puts_the_requested_session_first() {
        let mut other = GoalFile::new("s_2");
        other.goals.push(Goal::new(
            "g1",
            "other",
            "other",
            vec![Criterion::new("c1", "x")],
            Budget::default(),
            Source::User,
            NOW - 600_000,
        ));
        other.refresh_active();
        let mut mine = GoalFile::new("s_1");
        mine.goals.push(goal());
        mine.refresh_active();
        let files = vec![("s_1".to_string(), mine), ("s_2".to_string(), other)];
        let data = panel("s_2", "/w", &files, NOW, &Settings::default(), &[]);
        assert_eq!(data["goals"][0]["session_id"], "s_2");
    }

    #[test]
    fn the_panel_is_a_versioned_contract() {
        let data = panel("s_1", "/w", &[], NOW, &Settings::default(), &[]);
        assert_eq!(data["schema"], PANEL_SCHEMA);
        assert!(PANEL_SCHEMA.starts_with("kc-goals/panel/"));
        assert_eq!(data["total"], 0);
        assert_eq!(data["open"], 0);
        assert!(data["engine"]["actions_note"]
            .as_str()
            .unwrap()
            .contains("sends its instruction"));
    }

    #[test]
    fn the_engine_options_carry_what_a_ui_needs_to_show_them() {
        let cfg = Settings {
            verify_commands: true,
            ..Settings::default()
        };
        let data = panel("s_1", "/w", &[], NOW, &cfg, &[]);
        let options = data["engine"]["options"].as_array().unwrap();
        let by_key = |key: &str| {
            options
                .iter()
                .find(|o| o["key"] == key)
                .unwrap_or_else(|| panic!("no option {key}"))
                .clone()
        };
        assert_eq!(by_key("auto_continue")["value"], true);
        assert_eq!(by_key("verify_commands")["value"], true);
        // 0 is rendered as "no cap", not as a literal zero a human would read as
        // "cannot continue".
        assert_eq!(by_key("max_continuations")["value"], 0);
        assert_eq!(by_key("max_continuations")["zero_means"], "no cap");
        assert_eq!(by_key("max_stalled_resumes")["value"], 5);
        assert_eq!(by_key("tick_secs")["unit"], "seconds");
        // Every option is labelled: a UI must not have to prettify a key.
        for option in options {
            assert!(option["label"].as_str().unwrap().len() > 3, "{option}");
            assert!(option["kind"].is_string(), "{option}");
        }
    }

    #[test]
    fn the_goal_card_exposes_criteria_checks_options_and_actions() {
        let mut g = goal();
        g.criteria[0] = Criterion::with_check("c1", "builds", "cargo build -q");
        g.criteria[0].record_check(true, "ok", NOW).unwrap();
        g.criteria[1] = Criterion::with_check("c2", "tested", "cargo test -q");
        g.record_progress("wired the renderer", Some("check it"), NOW)
            .unwrap();
        let mut file = GoalFile::new("s_1");
        file.goals.push(g);
        file.refresh_active();
        let data = panel("s_1", "/w", &[("s_1".to_string(), file)], NOW, &Settings::default(), &[]);
        let card = &data["goals"][0];

        // the goal
        assert_eq!(card["goal"], "g1");
        assert_eq!(card["status"], "active");
        assert_eq!(card["score"], "1/2");
        assert_eq!(card["progress_pct"], 50);
        assert_eq!(card["is_current_session"], true);
        assert!(card["next"].as_str().unwrap().contains("goal_check"));

        // the criteria, with the check verdict and the evidence
        let criteria = card["criteria_detail"].as_array().unwrap();
        assert_eq!(criteria.len(), 2);
        assert_eq!(criteria[0]["met"], true);
        assert_eq!(criteria[0]["check"]["command"], "cargo build -q");
        assert_eq!(criteria[0]["check"]["ok"], true);
        assert_eq!(criteria[1]["check"]["ok"], Value::Null, "never run");
        assert_eq!(card["checks"], 2);
        assert_eq!(card["checks_pending"], 1);

        // the options a UI may show for one goal
        let options = card["options"].as_array().unwrap();
        assert_eq!(options[0]["key"], "max_continuations");
        assert_eq!(options[0]["zero_means"], "no cap");

        // the actions, each one a message to send
        let actions = card["actions"].as_array().unwrap();
        let ids: Vec<&str> = actions.iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["status", "check", "complete", "pause", "abandon"]);
        for action in actions {
            assert!(
                action["instruction"]
                    .as_str()
                    .unwrap()
                    .contains("plugin__kc-goals__"),
                "{action}"
            );
            assert!(action["hint"].as_str().unwrap().len() > 5, "{action}");
        }
        // Complete is disabled while a check is pending, and says why.
        let complete = actions.iter().find(|a| a["id"] == "complete").unwrap();
        assert_eq!(complete["enabled"], false);
        assert!(complete["hint"].as_str().unwrap().contains("check"));
        // The journal travels with the card so a UI can show progress.
        assert_eq!(card["journal"][0]["note"], "wired the renderer");
        assert_eq!(card["journal"][0]["next"], "check it");
    }

    #[test]
    fn a_paused_goal_offers_resume_and_not_pause() {
        let mut g = goal();
        g.pause("blocked on a credential", NOW);
        let mut file = GoalFile::new("s_1");
        file.goals.push(g);
        let data = panel("s_1", "/w", &[("s_1".to_string(), file)], NOW, &Settings::default(), &[]);
        let actions = data["goals"][0]["actions"].as_array().unwrap();
        let ids: Vec<&str> = actions.iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["resume"]);
        assert_eq!(data["open"], 0);
        assert_eq!(data["goals"][0]["pause_reason"], "blocked on a credential");
    }

    #[test]
    fn a_done_goal_offers_no_actions_at_all() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("build"), NOW).unwrap();
        g.set_criterion("c2", true, Some("tests"), NOW).unwrap();
        g.complete(NOW).unwrap();
        let mut file = GoalFile::new("s_1");
        file.goals.push(g);
        let data = panel("s_1", "/w", &[("s_1".to_string(), file)], NOW, &Settings::default(), &[]);
        assert_eq!(data["goals"][0]["actions"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_completable_goal_enables_complete() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("build"), NOW).unwrap();
        g.set_criterion("c2", true, Some("tests"), NOW).unwrap();
        let mut file = GoalFile::new("s_1");
        file.goals.push(g);
        let data = panel("s_1", "/w", &[("s_1".to_string(), file)], NOW, &Settings::default(), &[]);
        let actions = data["goals"][0]["actions"].as_array().unwrap();
        let complete = actions.iter().find(|a| a["id"] == "complete").unwrap();
        assert_eq!(complete["enabled"], true);
        assert!(!actions.iter().any(|a| a["id"] == "check"), "nothing to check");
    }

    #[test]
    fn the_panel_marks_which_goal_belongs_to_the_open_session() {
        let mut mine = GoalFile::new("s_1");
        mine.goals.push(goal());
        mine.refresh_active();
        let mut other = GoalFile::new("s_2");
        other.goals.push(Goal::new(
            "g1",
            "other",
            "other",
            vec![Criterion::new("c1", "x")],
            Budget::default(),
            Source::User,
            NOW - 600_000,
        ));
        other.refresh_active();
        let files = vec![("s_1".to_string(), mine), ("s_2".to_string(), other)];
        let data = panel("s_2", "/w", &files, NOW, &Settings::default(), &[]);
        assert_eq!(data["goals"][0]["session_id"], "s_2");
        assert_eq!(data["goals"][0]["is_current_session"], true);
        assert_eq!(data["goals"][1]["is_current_session"], false);
        assert_eq!(data["total"], 2);
        assert_eq!(data["open"], 2);
    }

    #[test]
    fn the_panel_shows_the_recent_engine_log() {
        let entries = vec![LogEntry {
            at_ms: NOW - 1_000,
            session_id: "s_1".into(),
            run_id: "r1".into(),
            kind: "continue".into(),
            outcome: "posted".into(),
        }];
        let data = panel("s_1", "/w", &[], NOW, &Settings::default(), &entries);
        assert_eq!(data["log"][0]["kind"], "continue");
        assert_eq!(data["log"][0]["outcome"], "posted");
    }
}
