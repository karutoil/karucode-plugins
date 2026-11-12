//! The goal document: what "finished" means, and what the engine is allowed to do
//! about it.
//!
//! Everything here is pure: no files, no HTTP, no clocks except the `now` a caller
//! passes in. That is deliberate - the rules that decide whether an agent may
//! claim a goal is done are the rules most worth testing, and they are testable
//! only if nothing else is in the way.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Longest a journal keeps. It is the goal's memory across a context window, so
/// it is bounded by count rather than by age: the last few entries are what a
/// continuation re-states, and older ones are history a human can still read.
pub const MAX_JOURNAL: usize = 40;
/// Criteria a single goal may declare. Past this the "definition of done" has
/// stopped being a definition and become a wish list.
pub const MAX_CRITERIA: usize = 20;

/// A unique directory per test, so tests running in parallel cannot delete each
/// other's fixtures. `pid` alone is not enough: every test in one binary shares
/// it, and several can start inside the same millisecond.
#[cfg(test)]
pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("{tag}-{}-{nanos}-{n}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The engine resumes this goal when a run stops.
    Active,
    /// Kept, not resumed. `goal_resume` re-arms it.
    Paused,
    /// Every criterion is met and evidenced. The engine stops.
    Done,
    /// Dropped by a human or the agent. The engine stops.
    Abandoned,
    /// The goal cannot proceed (the session is gone, the budget is spent).
    Blocked,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Paused => "paused",
            Status::Done => "done",
            Status::Abandoned => "abandoned",
            Status::Blocked => "blocked",
        }
    }

    /// Whether the engine may resume work for this goal.
    pub fn is_open(&self) -> bool {
        matches!(self, Status::Active)
    }

    pub fn is_closed(&self) -> bool {
        matches!(self, Status::Done | Status::Abandoned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    User,
    Agent,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::User => "user",
            Source::Agent => "agent",
        }
    }
}

/// What stops this goal. Every field defaults to **unlimited**: a goal is
/// supposed to run until it is finished, and a number here is a guard rail a
/// human added on purpose, not a default.
///
/// `0` means "no limit" everywhere, which is why these are plain integers rather
/// than `Option`s - an empty budget in a hand-edited file is legible, and the
/// absence of a key gets the same meaning as a zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Times the engine may resume this goal. `0` = as often as it takes.
    #[serde(default)]
    pub max_continuations: u32,
    /// Wall-clock ceiling from the goal's first activation. `0` = no ceiling.
    #[serde(default)]
    pub max_minutes: u64,
    /// Resumes in a row in which the run did *nothing at all* - no tool call and
    /// no `goal_progress` note - before the goal is paused. `0` = never pause.
    ///
    /// This is not a budget: a run that works for an hour without writing a note
    /// is not a stall, and the counter is reset by any tool call. It exists to
    /// stop one specific runaway: a model that answers "done" without doing
    /// anything, forever, each cycle spending a full prompt.
    #[serde(default = "d_stalled", alias = "max_no_progress")]
    pub max_stalled_resumes: u32,
    /// Retries after a run ends `failed`. `0` = retry forever (with backoff).
    #[serde(default)]
    pub max_failed_retries: u32,
}

fn d_stalled() -> u32 {
    5
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_continuations: 0,
            max_minutes: 0,
            max_stalled_resumes: d_stalled(),
            max_failed_retries: 0,
        }
    }
}

impl Budget {
    /// Overlay a JSON object on these defaults. Unknown keys are ignored; a key
    /// of the wrong type is ignored rather than fatal, because a goal that cannot
    /// be created because one number was spelled wrong is a worse outcome than a
    /// goal with a default.
    pub fn merged_with(&self, value: &Value) -> Budget {
        let u = |k: &str| value.get(k).and_then(Value::as_u64);
        let mut next = *self;
        if let Some(v) = u("max_continuations") {
            next.max_continuations = v.min(u32::MAX as u64) as u32;
        }
        if let Some(v) = u("max_minutes") {
            next.max_minutes = v;
        }
        // `max_no_progress` is the v0.1 spelling; a goal file written by an older
        // build keeps meaning what it meant.
        if let Some(v) = u("max_stalled_resumes").or_else(|| u("max_no_progress")) {
            next.max_stalled_resumes = v.min(u32::MAX as u64) as u32;
        }
        if let Some(v) = u("max_failed_retries") {
            next.max_failed_retries = v.min(u32::MAX as u64) as u32;
        }
        next
    }

    /// Keep the numbers inside a shape the engine can act on. Zero survives:
    /// it means "unlimited", not "zero allowed".
    pub fn clamped(&self) -> Budget {
        Budget {
            max_continuations: self.max_continuations.min(100_000),
            max_minutes: self.max_minutes.min(10 * 365 * 24 * 60),
            max_stalled_resumes: self.max_stalled_resumes.min(1_000),
            max_failed_retries: self.max_failed_retries.min(10_000),
        }
    }

    pub fn describe(&self) -> String {
        let count = |n: u64, what: &str| {
            if n == 0 {
                format!("unlimited {what}")
            } else {
                format!("{n} {what}")
            }
        };
        format!(
            "{}, {}, pause after {} stalled resumes",
            count(self.max_continuations as u64, "continuations"),
            count(self.max_minutes, "minutes"),
            if self.max_stalled_resumes == 0 {
                "never".to_string()
            } else {
                self.max_stalled_resumes.to_string()
            }
        )
    }

    /// `None` = no ceiling.
    pub fn continuation_room(&self, used: u32) -> Option<u32> {
        if self.max_continuations == 0 {
            None
        } else {
            Some(self.max_continuations.saturating_sub(used))
        }
    }

    /// `None` = no ceiling.
    pub fn minute_room(&self, elapsed_minutes: u64) -> Option<u64> {
        if self.max_minutes == 0 {
            None
        } else {
            Some(self.max_minutes.saturating_sub(elapsed_minutes))
        }
    }
}

/// One thing that must be true for the goal to be finished, and the proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub met: bool,
    /// What proves it. Required as soon as `met` is true - "done" without a
    /// receipt is the failure mode this whole plugin exists to refuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub met_at: Option<i64>,
    /// A command the plugin runs to judge this criterion itself, when the operator
    /// opted into verification. Exit code 0 is the only passing result, and a
    /// criterion with a check cannot be marked met on the agent's word alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// The last recorded verdict of `check`. `None` = never run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
}

impl Criterion {
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            met: false,
            evidence: None,
            met_at: None,
            check: None,
            check_ok: None,
            checked_at: None,
        }
    }

    /// The same criterion, judged by a command instead of by the agent's word.
    pub fn with_check(id: impl Into<String>, text: impl Into<String>, check: impl Into<String>) -> Self {
        let mut criterion = Criterion::new(id, text);
        criterion.check = Some(check.into());
        criterion
    }

    /// Whether this criterion is waiting for a passing `goal_check`.
    pub fn check_pending(&self) -> bool {
        self.check.is_some() && self.check_ok != Some(true)
    }

    /// Record a verification verdict. A passing check writes its own evidence, so
    /// the record reads the same whether a human or a command proved it.
    pub fn record_check(&mut self, ok: bool, output: &str, now: i64) -> Result<(), String> {
        if self.check.is_none() {
            return Err(format!(
                "criterion `{}` has no `check` command; mark it with `goal_criterion` and evidence                  instead",
                self.id
            ));
        }
        self.check_ok = Some(ok);
        self.checked_at = Some(now);
        if ok {
            self.met = true;
            self.met_at = Some(now);
            self.evidence = Some(truncate(
                &format!(
                    "verified by `{}` (exit 0){}",
                    self.check.as_deref().unwrap_or(""),
                    if output.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", output.trim())
                    }
                ),
                800,
            ));
        } else {
            self.met = false;
            self.met_at = None;
            self.evidence = Some(truncate(
                &format!(
                    "verification FAILED: `{}` (non-zero exit){}",
                    self.check.as_deref().unwrap_or(""),
                    if output.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", output.trim())
                    }
                ),
                800,
            ));
        }
        Ok(())
    }
}

/// One line of the goal's memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub at: i64,
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Progress {
    /// Times the engine resumed this goal.
    #[serde(default)]
    pub continuations: u32,
    /// When the goal became active (and when the wall clock started).
    #[serde(default)]
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_continuation_at: Option<i64>,
    /// `goal_progress` calls since the last resume, consumed when the engine
    /// decides whether the last run made progress.
    #[serde(default)]
    pub pending_progress: u32,
    /// Resumes in a row whose run did nothing at all (see `Budget`).
    #[serde(default, alias = "no_progress_streak")]
    pub stalled_streak: u32,
    /// The session's tool-call count at the last resume, so "did this run do
    /// anything?" is answered by the harness rather than by the agent's notes.
    #[serde(default)]
    pub tool_calls_at_last_resume: u64,
    /// Consecutive `failed` runs, reset by any other outcome.
    #[serde(default)]
    pub failed_retries: u32,
    /// The run whose end has already been handled. The idempotency key: the same
    /// run is never continued twice, whichever path noticed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_status: Option<String>,
    /// Set when the engine itself paused or blocked the goal, so the reason is
    /// visible in `goal_status` and on the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    /// The contract, in the words it was given in. Not a summary.
    pub statement: String,
    pub status: Status,
    pub source: Source,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub criteria: Vec<Criterion>,
    #[serde(default)]
    pub journal: Vec<JournalEntry>,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub progress: Progress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
}

impl Goal {
    pub fn new(
        id: impl Into<String>,
        title: &str,
        statement: &str,
        criteria: Vec<Criterion>,
        budget: Budget,
        source: Source,
        now: i64,
    ) -> Goal {
        Goal {
            id: id.into(),
            title: title.trim().to_string(),
            statement: if statement.trim().is_empty() {
                title.trim().to_string()
            } else {
                statement.trim().to_string()
            },
            status: Status::Active,
            source,
            created_at: now,
            updated_at: now,
            criteria,
            journal: Vec::new(),
            budget,
            progress: Progress {
                started_at: now,
                // Creating the goal is itself the first piece of progress: the
                // first resume must not look like a dry run.
                pending_progress: 1,
                ..Progress::default()
            },
            pause_reason: None,
        }
    }

    pub fn total_criteria(&self) -> usize {
        self.criteria.len()
    }

    pub fn met_criteria(&self) -> usize {
        self.criteria.iter().filter(|c| c.met).count()
    }

    pub fn remaining(&self) -> Vec<&Criterion> {
        self.criteria.iter().filter(|c| !c.met).collect()
    }

    /// Criteria that are done, with their evidence, for a status read.
    pub fn met(&self) -> Vec<&Criterion> {
        self.criteria.iter().filter(|c| c.met).collect()
    }

    /// The single line a panel shows.
    pub fn score(&self) -> String {
        format!("{}/{}", self.met_criteria(), self.total_criteria())
    }

    pub fn elapsed_minutes(&self, now: i64) -> u64 {
        if self.progress.started_at <= 0 || now <= self.progress.started_at {
            return 0;
        }
        ((now - self.progress.started_at) / 60_000) as u64
    }

    /// `None` = no ceiling.
    pub fn minutes_left(&self, now: i64) -> Option<u64> {
        self.budget.minute_room(self.elapsed_minutes(now))
    }

    /// `None` = no ceiling.
    pub fn continuations_left(&self) -> Option<u32> {
        self.budget.continuation_room(self.progress.continuations)
    }

    pub fn tool_calls_since_resume(&self, now_total: u64) -> u64 {
        now_total.saturating_sub(self.progress.tool_calls_at_last_resume)
    }

    pub fn criterion(&self, id: &str) -> Option<&Criterion> {
        self.criteria.iter().find(|c| c.id == id)
    }

    /// Mark one criterion. A criterion may only become `met` with evidence, and
    /// an evidence-free "done" is refused with the argument to pass next time.
    pub fn set_criterion(
        &mut self,
        id: &str,
        met: bool,
        evidence: Option<&str>,
        now: i64,
    ) -> Result<(), String> {
        if self.status.is_closed() {
            return Err(format!(
                "goal `{}` is {}; criteria can no longer be changed (call `goal_set` for the next goal)",
                self.id,
                self.status.as_str()
            ));
        }
        let position = self
            .criteria
            .iter()
            .position(|c| c.id.eq_ignore_ascii_case(id.trim()))
            .ok_or_else(|| {
                let known: Vec<&str> = self.criteria.iter().map(|c| c.id.as_str()).collect();
                format!(
                    "no criterion `{}`; this goal has: {}",
                    id.trim(),
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                )
            })?;
        if met {
            if let Some(check) = self.criteria[position].check.clone() {
                if self.criteria[position].check_ok != Some(true) {
                    return Err(format!(
                        "criterion `{}` has a check command (`{}`), so it cannot be marked met on                          your word. Call `goal_check` for `{}` and let the command judge it.",
                        id.trim(),
                        check,
                        id.trim()
                    ));
                }
            }
            let evidence = evidence.unwrap_or("").trim();
            if evidence.is_empty() {
                return Err(format!(
                    "marking `{}` as met requires `evidence`: what proves it (the command that \
                     passed, the test name, the file and the behaviour that now works). If that \
                     is not true yet, leave it unmet and do the work first.",
                    id.trim()
                ));
            }
            let evidence = truncate(evidence, 800);
            let criterion = &mut self.criteria[position];
            criterion.met = true;
            criterion.evidence = Some(evidence);
            criterion.met_at = Some(now);
        } else {
            let criterion = &mut self.criteria[position];
            criterion.met = false;
            criterion.evidence = None;
            criterion.met_at = None;
        }
        self.updated_at = now;
        Ok(())
    }

    /// Append to the goal's memory. This is the only thing that counts as
    /// progress for the spin guard.
    pub fn record_progress(
        &mut self,
        note: &str,
        next: Option<&str>,
        now: i64,
    ) -> Result<(), String> {
        if self.status.is_closed() {
            return Err(format!(
                "goal `{}` is {}; it can no longer take progress notes",
                self.id,
                self.status.as_str()
            ));
        }
        let note = note.trim();
        if note.chars().count() < 3 {
            return Err(
                "`note` is required: one line on what actually changed (a file, a command, a test)"
                    .to_string(),
            );
        }
        let next = next.map(str::trim).filter(|n| !n.is_empty());
        self.journal.push(JournalEntry {
            at: now,
            note: truncate(note, 600),
            next: next.map(|n| truncate(n, 300)),
        });
        let excess = self.journal.len().saturating_sub(MAX_JOURNAL);
        if excess > 0 {
            self.journal.drain(..excess);
        }
        self.progress.pending_progress = self.progress.pending_progress.saturating_add(1);
        self.updated_at = now;
        Ok(())
    }

    /// Close the goal. Refused unless every criterion is met and evidenced.
    pub fn complete(&mut self, now: i64) -> Result<(), String> {
        if self.status == Status::Done {
            return Err(format!("goal `{}` is already done", self.id));
        }
        if self.status == Status::Abandoned {
            return Err(format!("goal `{}` was abandoned", self.id));
        }
        if self.criteria.is_empty() {
            return Err(
                "this goal has no criteria, so there is nothing that proves it is done. Add \
                 them with `goal_set` (replace: true) first."
                    .to_string(),
            );
        }
        // A check that has not passed explains a criterion better than "unmet"
        // does, so that refusal comes first.
        let unverified: Vec<String> = self
            .criteria
            .iter()
            .filter(|c| c.check_pending())
            .map(|c| {
                format!(
                    "{} - `{}` ({})",
                    c.id,
                    c.check.as_deref().unwrap_or(""),
                    match c.check_ok {
                        Some(false) => "last run failed",
                        _ => "never run",
                    }
                )
            })
            .collect();
        if !unverified.is_empty() {
            return Err(format!(
                "cannot complete `{}`: these criteria are judged by a command that has not passed:\n  \
                 {}\nRun `goal_check` and let the commands decide.",
                self.id,
                unverified.join("\n  ")
            ));
        }
        let unmet: Vec<String> = self
            .remaining()
            .iter()
            .map(|c| format!("{} - {}", c.id, c.text))
            .collect();
        if !unmet.is_empty() {
            return Err(format!(
                "cannot complete `{}`: {}/{} criteria are unmet:\n  {}\nDo the work, then mark \
                 each with `goal_criterion` and its evidence.",
                self.id,
                unmet.len(),
                self.total_criteria(),
                unmet.join("\n  ")
            ));
        }
        if let Some(bare) = self
            .criteria
            .iter()
            .find(|c| c.met && c.evidence.as_deref().map(str::trim).unwrap_or("").is_empty())
        {
            return Err(format!(
                "cannot complete `{}`: criterion `{}` is met with no evidence",
                self.id, bare.id
            ));
        }
        self.status = Status::Done;
        self.progress.engine_note = None;
        self.pause_reason = None;
        self.updated_at = now;
        Ok(())
    }

    pub fn pause(&mut self, reason: &str, now: i64) {
        self.status = Status::Paused;
        self.pause_reason = Some(truncate(reason.trim(), 300));
        self.updated_at = now;
    }

    pub fn block(&mut self, reason: &str, now: i64) {
        self.status = Status::Blocked;
        self.pause_reason = Some(truncate(reason.trim(), 300));
        self.updated_at = now;
    }

    pub fn resume(&mut self, now: i64) -> Result<(), String> {
        match self.status {
            Status::Paused | Status::Blocked => {
                self.status = Status::Active;
                self.pause_reason = None;
                self.progress.engine_note = None;
                // A resumed goal gets a fresh spin streak: the human intervened,
                // which is new information the old streak cannot account for.
                self.progress.stalled_streak = 0;
                self.updated_at = now;
                Ok(())
            }
            Status::Active => Err(format!("goal `{}` is already active", self.id)),
            Status::Done => Err(format!(
                "goal `{}` is done; set the next one with `goal_set`",
                self.id
            )),
            Status::Abandoned => Err(format!(
                "goal `{}` was abandoned; set the next one with `goal_set`",
                self.id
            )),
        }
    }

    pub fn abandon(&mut self, reason: &str, now: i64) {
        self.status = Status::Abandoned;
        self.pause_reason = Some(truncate(reason.trim(), 300));
        self.updated_at = now;
    }
}

/// One session's goals. The file on disk is exactly this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalFile {
    pub schema: u32,
    pub session_id: String,
    /// The goal the engine may resume. `None` once every goal is closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_goal: Option<String>,
    #[serde(default)]
    pub goals: Vec<Goal>,
}

pub const SCHEMA: u32 = 1;

impl GoalFile {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            schema: SCHEMA,
            session_id: session_id.into(),
            active_goal: None,
            goals: Vec::new(),
        }
    }

    pub fn active(&self) -> Option<&Goal> {
        let id = self.active_goal.as_deref()?;
        self.goals
            .iter()
            .find(|g| g.id == id)
            .filter(|g| g.status.is_open())
    }

    pub fn active_mut(&mut self) -> Option<&mut Goal> {
        let id = self.active_goal.clone()?;
        self.goals
            .iter_mut()
            .find(|g| g.id == id)
            .filter(|g| g.status.is_open())
    }

    pub fn goal(&self, id: &str) -> Option<&Goal> {
        self.goals.iter().find(|g| g.id.eq_ignore_ascii_case(id))
    }

    pub fn goal_mut(&mut self, id: &str) -> Option<&mut Goal> {
        self.goals
            .iter_mut()
            .find(|g| g.id.eq_ignore_ascii_case(id))
    }

    /// The cleanup `goal_set`/`goal_complete`/`goal_abandon` all end with: the
    /// pointer follows whatever goal is actually open.
    pub fn refresh_active(&mut self) {
        self.active_goal = self
            .goals
            .iter()
            .rev()
            .find(|g| g.status.is_open())
            .map(|g| g.id.clone());
    }

    pub fn next_goal_id(&self) -> String {
        format!("g{}", self.goals.len() + 1)
    }

    pub fn has_open_goal(&self) -> bool {
        self.active().is_some()
    }

    /// Newest first, which is the order a reader wants.
    pub fn newest_first(&self) -> Vec<&Goal> {
        self.goals.iter().rev().collect()
    }
}

/// Cut a string to `max` characters on a word boundary when possible, and mark
/// the cut. Used on every field that comes from the model, so one runaway note
/// cannot make the file unreadable or the prompt enormous.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= max.saturating_sub(1) {
            break;
        }
        out.push(ch);
    }
    if let Some(cut) = out.rfind(char::is_whitespace) {
        if cut > max / 2 {
            out.truncate(cut);
        }
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal() -> Goal {
        Goal::new(
            "g1",
            "Ship it",
            "Ship the thing end to end",
            vec![
                Criterion::new("c1", "builds"),
                Criterion::new("c2", "tested"),
            ],
            Budget::default(),
            Source::User,
            1_000,
        )
    }

    #[test]
    fn a_new_goal_is_active_and_counts_its_creation_as_progress() {
        let g = goal();
        assert_eq!(g.status, Status::Active);
        assert_eq!(g.progress.pending_progress, 1);
        assert_eq!(g.score(), "0/2");
        // No budget by default: no cap on continuations, no deadline.
        assert_eq!(g.continuations_left(), None);
        assert_eq!(g.minutes_left(1_000), None);
    }

    #[test]
    fn met_requires_evidence() {
        let mut g = goal();
        let err = g.set_criterion("c1", true, None, 2_000).unwrap_err();
        assert!(err.contains("requires `evidence`"), "{err}");
        assert!(!g.criteria[0].met, "a refused mark must not stick");
        g.set_criterion("c1", true, Some("cargo build --release"), 2_000)
            .unwrap();
        assert!(g.criteria[0].met);
        assert_eq!(g.criteria[0].evidence.as_deref(), Some("cargo build --release"));
    }

    #[test]
    fn an_unknown_criterion_names_the_known_ones() {
        let mut g = goal();
        let err = g.set_criterion("c9", true, Some("x"), 2_000).unwrap_err();
        assert!(err.contains("c1, c2"), "{err}");
    }

    #[test]
    fn unmarking_a_criterion_drops_its_evidence() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("done"), 2_000).unwrap();
        g.set_criterion("c1", false, None, 3_000).unwrap();
        assert!(!g.criteria[0].met);
        assert!(g.criteria[0].evidence.is_none());
        assert!(g.criteria[0].met_at.is_none());
    }

    #[test]
    fn completion_refuses_while_any_criterion_is_unmet_and_says_which() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("cargo build"), 2_000).unwrap();
        let err = g.complete(3_000).unwrap_err();
        assert!(err.contains("1/2 criteria are unmet"), "{err}");
        assert!(err.contains("c2 - tested"), "{err}");
        assert_eq!(g.status, Status::Active);
    }

    #[test]
    fn completion_refuses_a_goal_with_no_criteria() {
        let mut g = Goal::new(
            "g1",
            "vague",
            "be better",
            vec![],
            Budget::default(),
            Source::Agent,
            1_000,
        );
        let err = g.complete(2_000).unwrap_err();
        assert!(err.contains("no criteria"), "{err}");
    }

    #[test]
    fn completion_succeeds_once_everything_is_evidenced() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("build ok"), 2_000).unwrap();
        g.set_criterion("c2", true, Some("12 tests pass"), 2_500).unwrap();
        g.complete(3_000).unwrap();
        assert_eq!(g.status, Status::Done);
        assert_eq!(g.score(), "2/2");
    }

    #[test]
    fn a_closed_goal_takes_no_more_edits() {
        let mut g = goal();
        g.set_criterion("c1", true, Some("build ok"), 2_000).unwrap();
        g.set_criterion("c2", true, Some("tests"), 2_000).unwrap();
        g.complete(3_000).unwrap();
        assert!(g.set_criterion("c1", false, None, 4_000).is_err());
        assert!(g.record_progress("still poking", None, 4_000).is_err());
        assert!(g.complete(4_000).is_err());
    }

    #[test]
    fn progress_notes_are_bounded_and_counted() {
        let mut g = goal();
        for i in 0..(MAX_JOURNAL + 5) {
            g.record_progress(&format!("step {i}"), Some("next one"), 2_000 + i as i64)
                .unwrap();
        }
        assert_eq!(g.journal.len(), MAX_JOURNAL);
        assert_eq!(g.progress.pending_progress, 1 + (MAX_JOURNAL + 5) as u32);
        // The oldest are the ones dropped: memory is the recent past.
        assert!(g.journal.last().unwrap().note.contains("step 44"));
    }

    #[test]
    fn a_too_short_note_is_refused() {
        let mut g = goal();
        assert!(g.record_progress("ok", None, 2_000).is_err());
    }

    #[test]
    fn pause_resume_and_abandon_move_through_the_right_states() {
        let mut g = goal();
        g.pause("user asked", 2_000);
        assert_eq!(g.status, Status::Paused);
        assert_eq!(g.pause_reason.as_deref(), Some("user asked"));
        assert!(g.resume(3_000).is_ok());
        assert_eq!(g.status, Status::Active);
        assert!(g.pause_reason.is_none());
        assert!(g.resume(4_000).is_err(), "resuming an active goal is a no-op error");
        g.abandon("not worth it", 5_000);
        assert_eq!(g.status, Status::Abandoned);
        assert!(g.resume(6_000).is_err(), "an abandoned goal is not resumable");
    }

    #[test]
    fn blocking_is_resumable_and_clears_the_streak() {
        let mut g = goal();
        g.progress.stalled_streak = 2;
        g.block("session is gone", 2_000);
        assert_eq!(g.status, Status::Blocked);
        g.resume(3_000).unwrap();
        assert_eq!(g.status, Status::Active);
        assert_eq!(g.progress.stalled_streak, 0);
    }

    #[test]
    fn the_file_pointer_follows_the_open_goal() {
        let mut file = GoalFile::new("s_1");
        assert!(!file.has_open_goal());
        file.goals.push(goal());
        file.refresh_active();
        assert_eq!(file.active_goal.as_deref(), Some("g1"));
        assert!(file.has_open_goal());
        file.goals[0].abandon("dropped", 5_000);
        file.refresh_active();
        assert!(file.active_goal.is_none());
        assert!(!file.has_open_goal());
    }

    #[test]
    fn goal_ids_do_not_reuse_a_number() {
        let mut file = GoalFile::new("s_1");
        file.goals.push(goal());
        assert_eq!(file.next_goal_id(), "g2");
        file.goals.push(goal());
        assert_eq!(file.next_goal_id(), "g3");
    }

    #[test]
    fn budget_merging_ignores_junk_and_keeps_the_rest() {
        let b = Budget::default();
        let merged = b.merged_with(&serde_json::json!({
            "max_continuations": 3,
            "max_minutes": "soon",
            "max_no_progress": null,
            "unknown": 9
        }));
        assert_eq!(merged.max_continuations, 3);
        assert_eq!(merged.max_minutes, 0, "untouched keys stay unlimited");
        assert_eq!(merged.max_stalled_resumes, 5);
    }

    #[test]
    fn a_zero_budget_is_clamped_to_something_that_runs() {
        let b = Budget {
            max_continuations: 1_000_000,
            max_minutes: u64::MAX,
            max_stalled_resumes: 9_999,
            max_failed_retries: 900_000,
        }
        .clamped();
        assert_eq!(b.max_continuations, 100_000);
        assert_eq!(b.max_minutes, 10 * 365 * 24 * 60);
        assert_eq!(b.max_stalled_resumes, 1_000);
        assert_eq!(b.max_failed_retries, 10_000);
    }

    #[test]
    fn elapsed_and_remaining_time_are_computed_from_the_goal_not_the_clock() {
        let mut g = goal();
        g.progress.started_at = 1_000;
        assert_eq!(g.elapsed_minutes(1_000 + 60_000 * 10), 10);
        // Unlimited by default, so no room arithmetic to get wrong.
        assert_eq!(g.minutes_left(1_000 + 60_000 * 10), None);
        g.budget.max_minutes = 180;
        assert_eq!(g.minutes_left(1_000 + 60_000 * 10), Some(170));
        assert_eq!(g.minutes_left(1_000 + 60_000 * 10_000), Some(0));
    }

    #[test]
    fn truncation_keeps_the_head_and_marks_the_cut() {
        let long = "word ".repeat(100);
        let cut = truncate(&long, 20);
        assert!(cut.ends_with('…'));
        assert!(cut.chars().count() <= 21, "{}", cut.chars().count());
        assert_eq!(truncate("short", 20), "short");
    }

    #[test]
    fn the_document_round_trips_through_json() {
        let mut file = GoalFile::new("s_1");
        let mut g = goal();
        g.record_progress("wrote the model", Some("wire the worker"), 2_000)
            .unwrap();
        file.goals.push(g);
        file.refresh_active();
        let text = serde_json::to_string_pretty(&file).unwrap();
        let back: GoalFile = serde_json::from_str(&text).unwrap();
        assert_eq!(back, file);
    }
}
