//! The continuation policy: given a goal and how a run ended, decide whether to
//! resume it, pause it, or do nothing - and, if resuming, what to say.
//!
//! This module is pure on purpose. `plan` is the entire rulebook and it takes no
//! clock, no socket and no file, so every rule in the plan document has a test
//! that runs in microseconds. The worker (`worker.rs`) is then only plumbing:
//! load, plan, deliver, apply.
//!
//! ## There is no budget by default
//!
//! A goal runs until it is finished. `Budget` defaults to zero everywhere, and
//! zero means "no limit": no continuation cap, no wall clock, no deadline. A
//! number in a goal's budget is a guard rail somebody added on purpose.
//!
//! The one thing that is still on by default is not a budget. It is a **stall
//! detector**: a run in which the model made no tool call *and* wrote no
//! `goal_progress` note did nothing, and five of those in a row is a loop, not
//! work. It pauses the goal (recoverable, notified) instead of resuming it again
//! - and any single tool call resets it, so a long compile is not a stall.

use crate::config::Settings;
use crate::model::{truncate, Goal, Status};

/// How many journal entries a continuation re-states. Enough to carry the thread
/// of the work; small enough that the message stays a reminder, not a transcript.
pub const JOURNAL_TAIL: usize = 3;

/// Ceiling on the backoff after failed runs, however many of them there are.
pub const MAX_BACKOFF_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Post `prompt` as the next user turn.
    Continue { prompt: String },
    /// Stop resuming this goal, with a reason a human can read.
    Pause { reason: String },
    /// Do nothing now. Most of these re-appear on the next tick.
    Skip { reason: String },
}

impl Plan {
    /// Why this plan was chosen, in words a log or a test can assert on.
    #[allow(dead_code)]
    pub fn reason(&self) -> String {
        match self {
            Plan::Continue { .. } => "the goal is still open".to_string(),
            Plan::Pause { reason } => reason.clone(),
            Plan::Skip { reason } => reason.clone(),
        }
    }
}

/// Did the run that just ended do nothing at all?
///
/// `tool_calls` is the session's total, from the harness - not the agent's own
/// account of itself. `None` means the count could not be read, in which case the
/// answer is "not stalled": a plugin that cannot see the evidence must not pause
/// a goal on a guess.
pub fn did_stall(goal: &Goal, tool_calls: Option<u64>) -> bool {
    if goal.progress.pending_progress > 0 {
        return false;
    }
    match tool_calls {
        Some(total) => goal.tool_calls_since_resume(total) == 0,
        None => false,
    }
}

/// How long to wait before resuming. `min_gap_secs` normally; after failed runs it
/// doubles, so an endpoint that is down is retried forever but cheaply.
pub fn gap_ms(goal: &Goal, run_status: &str, cfg: &Settings) -> i64 {
    let base = cfg.min_gap_secs;
    if run_status != "failed" {
        return base as i64 * 1_000;
    }
    let doublings = goal.progress.failed_retries.min(6);
    let grown = base.saturating_mul(1 << doublings).min(MAX_BACKOFF_SECS);
    grown as i64 * 1_000
}

/// The whole rulebook.
pub fn plan(
    goal: &Goal,
    run_id: &str,
    run_status: &str,
    tool_calls: Option<u64>,
    now: i64,
    cfg: &Settings,
) -> Plan {
    if !cfg.enabled || !cfg.auto_continue {
        return Plan::Skip {
            reason: "auto-continue is off in this plugin's settings".into(),
        };
    }
    if goal.status != Status::Active {
        return Plan::Skip {
            reason: format!("the goal is {}", goal.status.as_str()),
        };
    }
    if run_id.is_empty() {
        return Plan::Skip {
            reason: "no run id to key a continuation off".into(),
        };
    }
    if goal.progress.last_run_id.as_deref() == Some(run_id) {
        return Plan::Skip {
            reason: "this run has already been continued".into(),
        };
    }

    // Status rules first: they explain *why* a goal stops better than a number
    // does, and a cancelled run must not be blamed on a ceiling.
    match run_status {
        "cancelled" => {
            return Plan::Pause {
                reason: "the run was cancelled, so the goal stopped with it. Call \
                         `goal_resume` when work should carry on"
                    .into(),
            }
        }
        "failed"
            if goal.budget.max_failed_retries > 0
                && goal.progress.failed_retries >= goal.budget.max_failed_retries =>
        {
            return Plan::Pause {
                reason: format!(
                    "{} runs in a row failed, and this goal allows {} retries",
                    goal.progress.failed_retries, goal.budget.max_failed_retries
                ),
            }
        }
        _ => {}
    }

    // Optional ceilings. Zero means there is nothing to check.
    if goal.continuations_left() == Some(0) {
        return Plan::Pause {
            reason: format!(
                "this goal's continuation cap is spent ({} of {})",
                goal.progress.continuations, goal.budget.max_continuations
            ),
        };
    }
    if goal.minutes_left(now) == Some(0) {
        return Plan::Pause {
            reason: format!(
                "this goal's wall clock is spent ({} of {} minutes)",
                goal.elapsed_minutes(now),
                goal.budget.max_minutes
            ),
        };
    }

    // The stall detector, which is not a budget: it counts runs that did nothing,
    // and one tool call resets it.
    let stalled = did_stall(goal, tool_calls);
    let streak = goal.progress.stalled_streak + u32::from(stalled);
    if goal.budget.max_stalled_resumes > 0 && streak >= goal.budget.max_stalled_resumes {
        return Plan::Pause {
            reason: format!(
                "{} resumes in a row did nothing at all - no tool call and no `goal_progress` \
                 note. The goal is paused rather than looping. Do the next step, record it, then \
                 `goal_resume`",
                streak
            ),
        };
    }

    if let Some(last) = goal.progress.last_continuation_at {
        let gap = gap_ms(goal, run_status, cfg);
        if now >= last && now - last < gap {
            return Plan::Skip {
                reason: format!(
                    "only {}s since the last continuation (floor is {}s)",
                    (now - last) / 1_000,
                    gap / 1_000
                ),
            };
        }
    }

    let prompt = continuation_prompt(goal, now, cfg);
    if prompt.chars().count() > cfg.max_prompt_chars {
        // Not a reason to stop the goal: a prompt this long is a bug here, and
        // sending a 40 KB "reminder" would be worse than skipping a tick.
        return Plan::Skip {
            reason: format!(
                "the continuation prompt came out at {} chars (cap {}); not sending it",
                prompt.chars().count(),
                cfg.max_prompt_chars
            ),
        };
    }
    Plan::Continue { prompt }
}

/// Apply a continuation to the goal's bookkeeping. Called only after the message
/// was actually accepted by the server, so a failed POST is retried by the next
/// tick instead of being silently swallowed.
pub fn note_continued(
    goal: &mut Goal,
    run_id: &str,
    run_status: &str,
    tool_calls: Option<u64>,
    now: i64,
) {
    goal.progress.continuations = goal.progress.continuations.saturating_add(1);
    goal.progress.last_continuation_at = Some(now);
    note_run(goal, run_id, run_status, tool_calls, now);
}

/// Record what the run did, independently of whether we continue: this is what
/// makes the *next* decision (and a human reading `goal_status`) honest.
pub fn note_run(goal: &mut Goal, run_id: &str, run_status: &str, tool_calls: Option<u64>, now: i64) {
    if did_stall(goal, tool_calls) {
        goal.progress.stalled_streak = goal.progress.stalled_streak.saturating_add(1);
    } else {
        goal.progress.stalled_streak = 0;
    }
    goal.progress.pending_progress = 0;
    if let Some(total) = tool_calls {
        goal.progress.tool_calls_at_last_resume = total;
    }
    if run_status == "failed" {
        goal.progress.failed_retries = goal.progress.failed_retries.saturating_add(1);
    } else {
        goal.progress.failed_retries = 0;
    }
    goal.progress.last_run_id = Some(run_id.to_string());
    goal.progress.last_run_status = Some(run_status.to_string());
    goal.updated_at = now;
}

/// Apply a pause. The engine's reason goes in `engine_note` as well as
/// `pause_reason`, so `goal_status` can tell "the engine stopped this" from "a
/// human stopped this".
pub fn note_paused(goal: &mut Goal, reason: &str, now: i64) {
    let reason = truncate(reason, 300);
    goal.pause(&reason, now);
    goal.progress.engine_note = Some(reason);
}

/// Apply a block: the obstacle is outside the goal (the session was deleted), so
/// it is `blocked` rather than `paused`, and it is still resumable.
pub fn note_blocked(goal: &mut Goal, reason: &str, now: i64) {
    let reason = truncate(reason, 300);
    goal.block(&reason, now);
    goal.progress.engine_note = Some(reason);
}

/// The message the engine posts. A *user* turn, not a system note: it is
/// appended to the conversation, so it does not invalidate the prompt prefix the
/// way a changing `before_prompt` block would.
pub fn continuation_prompt(goal: &Goal, now: i64, cfg: &Settings) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[kc-goals] Continuing goal {} — {}. The previous run ended before the goal was met.\n\n",
        goal.id,
        truncate(&goal.title, 120)
    ));
    out.push_str(&format!("Goal: {}\n\n", truncate(&goal.statement, 800)));

    let remaining = goal.remaining();
    out.push_str(&format!(
        "Remaining criteria ({}/{} met):\n",
        goal.met_criteria(),
        goal.total_criteria()
    ));
    if remaining.is_empty() {
        out.push_str("  none — every criterion is met; call goal_complete.\n");
    } else {
        for criterion in remaining.iter().take(12) {
            match criterion.check.as_deref() {
                Some(check) => out.push_str(&format!(
                    "  {} — {}  [checked by: {}]\n",
                    criterion.id,
                    truncate(&criterion.text, 200),
                    truncate(check, 120)
                )),
                None => out.push_str(&format!(
                    "  {} — {}\n",
                    criterion.id,
                    truncate(&criterion.text, 200)
                )),
            }
        }
        if remaining.len() > 12 {
            out.push_str(&format!(
                "  …and {} more (see goal_status)\n",
                remaining.len() - 12
            ));
        }
    }

    let tail: Vec<String> = goal
        .journal
        .iter()
        .rev()
        .take(JOURNAL_TAIL)
        .rev()
        .map(|entry| match entry.next.as_deref() {
            Some(next) => format!(
                "- {} (next: {})",
                truncate(&entry.note, 200),
                truncate(next, 120)
            ),
            None => format!("- {}", truncate(&entry.note, 200)),
        })
        .collect();
    if !tail.is_empty() {
        out.push_str("\nProgress so far, newest last:\n");
        for line in tail {
            out.push_str(&line);
            out.push('\n');
        }
    }

    let stalled = if goal.budget.max_stalled_resumes == 0 {
        "a pause on stalls is off".to_string()
    } else {
        format!(
            "{}/{} stalled resumes used",
            goal.progress.stalled_streak, goal.budget.max_stalled_resumes
        )
    };
    out.push_str(&format!(
        "\nBudget: {} continuations; {}; {}.\n",
        if goal.budget.max_continuations == 0 {
            "unlimited".to_string()
        } else {
            format!(
                "{} of {} used",
                goal.progress.continuations, goal.budget.max_continuations
            )
        },
        match goal.minutes_left(now) {
            None => "no deadline".to_string(),
            Some(left) => format!("{left} min left"),
        },
        stalled
    ));

    out.push_str(
        "\nRules for this turn: take the next concrete step now — a tool call, a file, a command. \
         When something actually lands, record it with goal_progress (one line, plus what is \
         next). Mark a criterion with goal_criterion only with the evidence that proves it, and if \
         a criterion lists a check command, run goal_check and let the command decide. When every \
         criterion is met, call goal_complete. If you are genuinely blocked, call goal_pause with \
         the reason instead of ending the turn silently. Do not re-plan work the journal above \
         already records as done.\n",
    );
    let _ = cfg;
    out
}

/// Why the engine would or would not resume right now, for `goal_status` and the
/// panel. It answers the question an agent (and a human) actually asks: "am I
/// done, or am I about to be put back to work?"
pub fn explain(goal: &Goal, now: i64, cfg: &Settings) -> String {
    if !cfg.enabled || !cfg.auto_continue {
        return "engine: off (this plugin's settings disable auto-continue), so a stopped run \
                stays stopped"
            .to_string();
    }
    match goal.status {
        Status::Active => {}
        Status::Paused => {
            return format!(
                "engine: not resuming — the goal is paused ({}). Call goal_resume to carry on.",
                goal.pause_reason.as_deref().unwrap_or("no reason recorded")
            )
        }
        Status::Done => {
            return "engine: not resuming — every criterion is met and the goal is done".into()
        }
        Status::Abandoned => {
            return format!(
                "engine: not resuming — the goal was abandoned ({})",
                goal.pause_reason.as_deref().unwrap_or("no reason recorded")
            )
        }
        Status::Blocked => {
            return format!(
                "engine: not resuming — the goal is blocked ({}). Call goal_resume when the \
                 obstacle is gone.",
                goal.pause_reason.as_deref().unwrap_or("no reason recorded")
            )
        }
    }
    if goal.continuations_left() == Some(0) {
        return format!(
            "engine: not resuming — this goal's continuation cap is spent ({} of {})",
            goal.progress.continuations, goal.budget.max_continuations
        );
    }
    if goal.minutes_left(now) == Some(0) {
        return format!(
            "engine: not resuming — this goal's wall clock is spent ({} of {} min)",
            goal.elapsed_minutes(now),
            goal.budget.max_minutes
        );
    }
    if goal.budget.max_stalled_resumes > 0
        && goal.progress.stalled_streak >= goal.budget.max_stalled_resumes
    {
        return format!(
            "engine: not resuming — {} resumes in a row did nothing at all",
            goal.progress.stalled_streak
        );
    }
    format!(
        "engine: will resume this goal whenever a run ends with it still open ({} continuations, \
         {}, {} of {} stalled resumes used). Ending the turn early does not end the goal.",
        match goal.budget.max_continuations {
            0 => "no cap".to_string(),
            n => format!("{n} cap"),
        },
        match goal.minutes_left(now) {
            None => "no deadline".to_string(),
            Some(left) => format!("{left} min left"),
        },
        goal.progress.stalled_streak,
        if goal.budget.max_stalled_resumes == 0 {
            "∞".to_string()
        } else {
            goal.budget.max_stalled_resumes.to_string()
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Budget, Criterion, Source};

    const NOW: i64 = 1_200_000;

    fn settings() -> Settings {
        Settings::default()
    }

    fn goal() -> Goal {
        let mut g = Goal::new(
            "g1",
            "Ship the thing",
            "Ship the thing, end to end, with tests",
            vec![
                Criterion::new("c1", "builds"),
                Criterion::new("c2", "tested"),
                Criterion::new("c3", "documented"),
            ],
            Budget::default(),
            Source::User,
            1_000_000,
        );
        g.record_progress("wrote the model", Some("wire the worker"), 1_100_000)
            .unwrap();
        g
    }

    #[test]
    fn a_completed_run_on_an_open_goal_continues() {
        let plan = plan(&goal(), "r1", "completed", Some(4), NOW, &settings());
        assert!(matches!(plan, Plan::Continue { .. }), "{plan:?}");
    }

    #[test]
    fn a_goal_that_is_not_active_is_left_alone() {
        let mut g = goal();
        g.pause("user asked", NOW);
        let plan = plan(&g, "r1", "completed", Some(4), NOW, &settings());
        assert!(matches!(plan, Plan::Skip { .. }));
        assert!(plan.reason().contains("paused"), "{}", plan.reason());
    }

    #[test]
    fn a_cancelled_run_pauses_the_goal_instead_of_fighting_the_stop_button() {
        let plan = plan(&goal(), "r1", "cancelled", Some(4), NOW, &settings());
        match plan {
            Plan::Pause { reason } => assert!(reason.contains("cancelled"), "{reason}"),
            other => panic!("expected a pause, got {other:?}"),
        }
    }

    #[test]
    fn the_same_run_is_never_continued_twice() {
        let mut g = goal();
        note_continued(&mut g, "r1", "completed", Some(9), NOW);
        let plan = plan(&g, "r1", "completed", Some(9), NOW + 60_000, &settings());
        assert!(matches!(plan, Plan::Skip { .. }));
        assert!(
            plan.reason().contains("already been continued"),
            "{}",
            plan.reason()
        );
    }

    #[test]
    fn a_new_run_after_the_last_one_is_continued() {
        let mut g = goal();
        note_continued(&mut g, "r1", "completed", Some(9), NOW);
        let plan = plan(&g, "r2", "completed", Some(12), NOW + 60_000, &settings());
        assert!(matches!(plan, Plan::Continue { .. }), "{plan:?}");
    }

    #[test]
    fn with_no_budget_a_goal_is_continued_forever() {
        // The whole point of the default: nothing here counts up to a cap.
        let mut g = goal();
        g.progress.continuations = 1_000;
        g.progress.started_at = 1_000_000;
        g.progress.pending_progress = 1;
        let ten_years = 10 * 365 * 24 * 60 * 60 * 1_000;
        let plan = plan(&g, "r1", "completed", Some(999_999), ten_years, &settings());
        assert!(matches!(plan, Plan::Continue { .. }), "{plan:?}");
    }

    #[test]
    fn an_explicit_continuation_cap_still_pauses() {
        let mut g = goal();
        g.budget.max_continuations = 2;
        g.progress.continuations = 2;
        let plan = plan(&g, "r9", "completed", Some(30), NOW, &settings());
        match plan {
            Plan::Pause { reason } => assert!(reason.contains("continuation cap"), "{reason}"),
            other => panic!("expected a pause, got {other:?}"),
        }
    }

    #[test]
    fn an_explicit_wall_clock_still_pauses() {
        let mut g = goal();
        g.budget.max_minutes = 5;
        let plan = plan(&g, "r1", "completed", Some(30), 1_000_000 + 6 * 60_000, &settings());
        match plan {
            Plan::Pause { reason } => assert!(reason.contains("wall clock"), "{reason}"),
            other => panic!("expected a pause, got {other:?}"),
        }
    }

    #[test]
    fn a_run_that_did_nothing_counts_as_stalled_and_five_of_them_pause() {
        let mut g = goal();
        g.progress.pending_progress = 0;
        // The baseline is already 7, and every run reports the same total: no call
        // was made during any of them.
        g.progress.tool_calls_at_last_resume = 7;
        for i in 0..4 {
            note_run(&mut g, &format!("r{i}"), "completed", Some(7), NOW);
        }
        assert_eq!(g.progress.stalled_streak, 4);
        let plan = plan(&g, "r5", "completed", Some(7), NOW + 60_000, &settings());
        match plan {
            Plan::Pause { reason } => {
                assert!(reason.contains("did nothing"), "{reason}");
                assert!(reason.contains("goal_resume"), "recoverable: {reason}");
            }
            other => panic!("expected a pause, got {other:?}"),
        }
    }

    #[test]
    fn one_tool_call_is_not_a_stall_even_with_no_progress_note() {
        // A run that works for an hour without writing a note is work, not a loop.
        let mut g = goal();
        g.progress.pending_progress = 0;
        for i in 0..10 {
            note_run(&mut g, &format!("r{i}"), "completed", Some(7 + i as u64), NOW);
        }
        assert_eq!(g.progress.stalled_streak, 0);
        let plan = plan(&g, "r99", "completed", Some(40), NOW + 60_000, &settings());
        assert!(matches!(plan, Plan::Continue { .. }), "{plan:?}");
    }

    #[test]
    fn a_missing_tool_count_never_pauses_a_goal() {
        let mut g = goal();
        g.progress.pending_progress = 0;
        for i in 0..10 {
            note_run(&mut g, &format!("r{i}"), "completed", None, NOW);
        }
        assert_eq!(g.progress.stalled_streak, 0, "no evidence, no stall");
        assert!(!did_stall(&g, None));
    }

    #[test]
    fn the_stall_detector_can_be_turned_off() {
        let mut g = goal();
        g.budget.max_stalled_resumes = 0;
        g.progress.stalled_streak = 50;
        let plan = plan(&g, "r1", "completed", Some(0), NOW, &settings());
        assert!(matches!(plan, Plan::Continue { .. }), "{plan:?}");
    }

    #[test]
    fn a_recorded_progress_note_is_progress_even_with_no_tool_calls() {
        let mut g = goal(); // pending_progress = 2 by now
        note_run(&mut g, "r1", "completed", Some(5), NOW);
        assert_eq!(g.progress.stalled_streak, 0);
        assert_eq!(g.progress.pending_progress, 0, "consumed");
    }

    #[test]
    fn failed_runs_are_retried_forever_by_default() {
        let mut g = goal();
        g.progress.failed_retries = 40;
        g.progress.pending_progress = 1;
        let plan = plan(&g, "r41", "failed", Some(1), NOW + 10 * 60 * 1_000, &settings());
        assert!(matches!(plan, Plan::Continue { .. }), "{plan:?}");
    }

    #[test]
    fn an_explicit_failure_cap_still_pauses() {
        let mut g = goal();
        g.budget.max_failed_retries = 1;
        g.progress.failed_retries = 1;
        let plan = plan(&g, "r1", "failed", Some(1), NOW + 60_000, &settings());
        match plan {
            Plan::Pause { reason } => assert!(reason.contains("retries"), "{reason}"),
            other => panic!("expected a pause, got {other:?}"),
        }
    }

    #[test]
    fn failures_back_off_so_a_dead_endpoint_is_retried_cheaply() {
        let mut g = goal();
        let cfg = Settings {
            min_gap_secs: 5,
            ..Settings::default()
        };
        assert_eq!(gap_ms(&g, "completed", &cfg), 5_000);
        g.progress.failed_retries = 0;
        assert_eq!(gap_ms(&g, "failed", &cfg), 5_000);
        g.progress.failed_retries = 3;
        assert_eq!(gap_ms(&g, "failed", &cfg), 40_000);
        g.progress.failed_retries = 50;
        assert_eq!(gap_ms(&g, "failed", &cfg), MAX_BACKOFF_SECS as i64 * 1_000);
    }

    #[test]
    fn a_failure_just_after_a_resume_waits_out_its_backoff() {
        let mut g = goal();
        g.progress.failed_retries = 2;
        note_continued(&mut g, "r1", "failed", Some(3), NOW);
        // note_continued counts the failure, so failed_retries is 3 and the floor
        // is 5s * 2^3 = 40s.
        let cfg = Settings {
            min_gap_secs: 5,
            ..Settings::default()
        };
        let plan = plan(&g, "r2", "failed", Some(3), NOW + 1_000, &cfg);
        assert!(matches!(plan, Plan::Skip { .. }), "{plan:?}");
        assert!(plan.reason().contains("floor is 40s"), "{}", plan.reason());
    }

    #[test]
    fn a_successful_run_resets_the_failure_counter() {
        let mut g = goal();
        g.progress.failed_retries = 5;
        note_run(&mut g, "r1", "completed", Some(9), NOW);
        assert_eq!(g.progress.failed_retries, 0);
    }

    #[test]
    fn an_interrupted_run_is_continued() {
        let plan = plan(&goal(), "r1", "interrupted", Some(4), NOW, &settings());
        assert!(matches!(plan, Plan::Continue { .. }), "{plan:?}");
    }

    #[test]
    fn a_limit_reached_run_is_continued_as_a_new_run() {
        let plan = plan(&goal(), "r1", "limit_reached", Some(4), NOW, &settings());
        assert!(matches!(plan, Plan::Continue { .. }), "{plan:?}");
    }

    #[test]
    fn a_continuation_too_soon_after_the_last_one_is_skipped() {
        let mut g = goal();
        note_continued(&mut g, "r1", "completed", Some(4), NOW);
        let plan = plan(&g, "r2", "completed", Some(5), NOW + 1_000, &settings());
        assert!(matches!(plan, Plan::Skip { .. }), "{plan:?}");
        assert!(plan.reason().contains("floor is"), "{}", plan.reason());
    }

    #[test]
    fn turning_auto_continue_off_skips_everything() {
        let mut cfg = settings();
        cfg.auto_continue = false;
        let plan = plan(&goal(), "r1", "completed", Some(4), NOW, &cfg);
        assert!(matches!(plan, Plan::Skip { .. }));
    }

    #[test]
    fn a_plan_without_a_run_id_does_nothing() {
        let plan = plan(&goal(), "", "completed", Some(4), NOW, &settings());
        assert!(matches!(plan, Plan::Skip { .. }));
    }

    #[test]
    fn the_prompt_states_the_goal_the_remaining_criteria_the_journal_and_the_budget() {
        let g = goal();
        let prompt = continuation_prompt(&g, NOW, &settings());
        assert!(prompt.starts_with("[kc-goals] Continuing goal g1"), "{prompt}");
        assert!(prompt.contains("Ship the thing, end to end"));
        assert!(prompt.contains("c1 — builds"), "{prompt}");
        assert!(
            prompt.contains("wrote the model (next: wire the worker)"),
            "{prompt}"
        );
        assert!(prompt.contains("unlimited continuations"), "{prompt}");
        assert!(prompt.contains("no deadline"), "{prompt}");
        assert!(prompt.contains("goal_complete"));
        assert!(prompt.contains("goal_pause"));
        assert!(prompt.contains("goal_check"), "{prompt}");
    }

    #[test]
    fn the_prompt_names_the_check_command_of_a_criterion_that_has_one() {
        let mut g = goal();
        g.criteria[0] = Criterion::with_check("c1", "builds", "cargo build -q");
        let prompt = continuation_prompt(&g, NOW, &settings());
        assert!(prompt.contains("[checked by: cargo build -q]"), "{prompt}");
    }

    #[test]
    fn the_prompt_is_bounded_for_a_huge_goal() {
        let mut g = goal();
        g.statement = "word ".repeat(4000);
        for i in 0..20 {
            g.criteria
                .push(Criterion::new(format!("x{i}"), "word ".repeat(200)));
        }
        let cfg = Settings::default();
        let prompt = continuation_prompt(&g, NOW, &cfg);
        assert!(
            prompt.chars().count() < cfg.max_prompt_chars,
            "{} chars",
            prompt.chars().count()
        );
    }

    #[test]
    fn a_prompt_over_the_cap_is_skipped_not_sent() {
        let mut g = goal();
        g.statement = "word ".repeat(4000);
        let mut cfg = settings();
        cfg.max_prompt_chars = 500; // the clamp floor
        let plan = plan(&g, "r1", "completed", Some(4), NOW, &cfg);
        assert!(matches!(plan, Plan::Skip { .. }), "{plan:?}");
        assert!(plan.reason().contains("cap"), "{}", plan.reason());
    }

    #[test]
    fn a_paused_goal_explains_itself_and_that_it_will_not_resume() {
        let mut g = goal();
        note_paused(&mut g, "budget spent", NOW);
        let text = explain(&g, NOW, &settings());
        assert!(text.contains("not resuming"), "{text}");
        assert!(text.contains("budget spent"), "{text}");
        assert_eq!(g.progress.engine_note.as_deref(), Some("budget spent"));
    }

    #[test]
    fn an_open_goal_says_it_will_resume_and_that_there_is_no_cap() {
        let text = explain(&goal(), NOW, &settings());
        assert!(text.contains("will resume"), "{text}");
        assert!(text.contains("no cap continuations"), "{text}");
        assert!(text.contains("no deadline"), "{text}");
    }

    #[test]
    fn explain_reports_an_off_engine() {
        let mut cfg = settings();
        cfg.enabled = false;
        assert!(explain(&goal(), NOW, &cfg).contains("off"));
    }
}
