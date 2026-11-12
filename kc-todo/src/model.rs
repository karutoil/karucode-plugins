//! The task list itself: statuses, items, subtasks and the rules that stop a
//! list from lying.
//!
//! # Why the rules live here
//!
//! An agent's failure mode with TODO lists is not "cannot write a list". It is
//! that the list stops matching reality: items stay `in_progress` after the work
//! landed, items are marked `done` that were never verified, and a run ends with
//! a tidy-looking list nobody updated. Every rule below exists to make that
//! state either impossible or **visible in the next prompt**:
//!
//! * `done` needs `evidence` - a claim of completion carries the proof with it,
//!   so the human reading the UI can check the claim instead of trusting it.
//! * `partial` needs `limitation` - "implemented but limited" is a first-class
//!   outcome, so an honest half-finish never has to be dressed up as `done`.
//! * `blocked`/`deferred`/`cancelled` need `reason` - a status that stops work
//!   explains itself once, instead of being re-litigated next run.
//! * a parent cannot be `done` while a child is open.
//! * at most one item is `in_progress` by default - the agent must close what it
//!   started before opening the next thing.
//! * an item marked `done` without ever being `in_progress` is recorded as an
//!   *unverified* completion, and [`List::audit`] reports it.
//!
//! None of these can force an agent to be honest. What they can do - and do -
//! is make dishonesty require an explicit, recorded lie rather than a silent
//! omission, because the next `before_prompt` renders exactly what was left
//! dangling.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Lifecycle of one task.
///
/// The wire values are the conventional engineering names; the UI labels are the
/// vocabulary a person actually uses, and the mapping is deliberate:
///
/// | wire | UI label | means |
/// | --- | --- | --- |
/// | `pending` | Not implemented | no work started |
/// | `in_progress` | In progress | actively being worked |
/// | `blocked` | Blocked | cannot proceed; `reason` says on what |
/// | `deferred` | Deferred | deliberately postponed; `reason` says until when |
/// | `partial` | Implemented (limited) | works, with a known gap; `limitation` says what |
/// | `done` | Implemented | finished and verified; `evidence` says how |
/// | `cancelled` | Cancelled | will not be done; `reason` says why |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Pending,
    InProgress,
    Blocked,
    Deferred,
    Partial,
    Done,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Deferred => "deferred",
            Self::Partial => "partial",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a status the agent typed. Accepts the word the model reaches for
    /// most often as an alias, so a sensible call is not rejected over
    /// vocabulary: `not_implemented`/`todo`/`open` are all `pending`,
    /// `wip`/`doing` are `in_progress`, and `complete`/`completed`/`finished`
    /// are `done`.
    ///
    /// `implemented` maps to `done` and `implemented_limited` to `partial`,
    /// because that is how a model naturally describes the two.
    pub fn parse(raw: &str) -> Option<Self> {
        let key = raw.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Some(match key.as_str() {
            "pending" | "not_implemented" | "unimplemented" | "todo" | "open" | "not_started" => {
                Self::Pending
            }
            "in_progress" | "inprogress" | "wip" | "doing" | "started" | "active" => {
                Self::InProgress
            }
            "blocked" | "waiting" => Self::Blocked,
            "deferred" | "postponed" | "later" => Self::Deferred,
            "partial" | "implemented_limited" | "implemented_but_limited" | "limited" => {
                Self::Partial
            }
            "done" | "complete" | "completed" | "finished" | "implemented" => Self::Done,
            "cancelled" | "canceled" | "dropped" | "wontfix" | "abandoned" => Self::Cancelled,
            _ => return None,
        })
    }

    /// Statuses that count as "still needs the agent's attention". This is what
    /// the carry-over audit and the prompt block count.
    pub fn is_open(self) -> bool {
        !matches!(self, Self::Done | Self::Cancelled)
    }

    /// The argument a transition to this status cannot be made without.
    pub fn required_detail(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Done => Some((
                "evidence",
                "what proves it is done (the command that passed, the file and \
                 behaviour that now work, the test name)",
            )),
            Self::Partial => Some((
                "limitation",
                "what is still missing or imperfect about the working part",
            )),
            Self::Blocked | Self::Deferred | Self::Cancelled => {
                Some(("reason", "why work on this stops where it does"))
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One task. A subtask is an item with a `parent`, at most two levels deep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Proof of completion. Required before `done`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence: String,
    /// The known gap in a `partial` item. Required before `partial`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limitation: String,
    /// Why work stopped. Required before `blocked`/`deferred`/`cancelled`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Set once the item has been `in_progress`, so `audit` can tell a
    /// verified completion from a drive-by one.
    #[serde(default)]
    pub started: bool,
}

impl Item {
    fn new(id: String, title: String, at: u64) -> Self {
        Self {
            id,
            title,
            status: Status::Pending,
            parent: None,
            detail: String::new(),
            evidence: String::new(),
            limitation: String::new(),
            reason: String::new(),
            created_ms: at,
            updated_ms: at,
            started: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.status.is_open()
    }
}

/// A note about a run that ended while work was still open.
///
/// This is the mechanism that makes "the agent forgot to mark things off"
/// visible instead of silent: `on_run_end` records it, and the next
/// `before_prompt` puts it in front of the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CarryOver {
    pub run_id: String,
    pub at_ms: u64,
    /// Items that were `in_progress` when the run ended: work the agent had
    /// explicitly started and never closed.
    pub in_progress: Vec<String>,
    /// Items that were still `pending` when the run ended.
    pub pending: Vec<String>,
}

/// The on-disk list. One per project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct List {
    #[serde(default = "schema_version")]
    pub schema: u32,
    /// Bumped on every mutation. The UI uses it to show "changed on disk", and
    /// the optimistically-checked save path uses it to detect a lost update.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub next_id: u32,
    #[serde(default)]
    pub items: Vec<Item>,
    /// Recent runs that ended with open work, newest last, bounded by
    /// [`MAX_CARRY_OVER`].
    #[serde(default)]
    pub carry_over: Vec<CarryOver>,
    #[serde(default)]
    pub updated_ms: u64,
}

fn schema_version() -> u32 {
    1
}

/// How many levels of nesting the list supports.
pub const MAX_DEPTH: usize = 2;

impl Default for List {
    fn default() -> Self {
        Self {
            schema: schema_version(),
            version: 0,
            next_id: 1,
            items: Vec::new(),
            carry_over: Vec::new(),
            updated_ms: 0,
        }
    }
}

/// Why a mutation was refused. The text is handed straight to the model, so it
/// names the argument to pass and the call to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

pub type Outcome<T> = Result<T, Refused>;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refuse<T>(message: impl Into<String>) -> Outcome<T> {
    Err(Refused(message.into()))
}

/// What a caller asked to change on one item.
///
/// `None` means "leave this field alone"; `Some("")` means "clear it". The
/// distinction matters: `Some("")` on `reason` is how a caller removes a stale
/// explanation, and the rules still refuse a status that then has none.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Patch {
    pub title: Option<String>,
    pub detail: Option<String>,
    pub status: Option<Status>,
    pub evidence: Option<String>,
    pub limitation: Option<String>,
    pub reason: Option<String>,
    /// Permits a second `in_progress` item. Deliberate, so that parallel work is
    /// a decision rather than an accident.
    pub allow_parallel: bool,
}

impl List {
    // ---- lookup -----------------------------------------------------------

    pub fn get(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.id == id)
    }

    pub fn children(&self, id: &str) -> Vec<&Item> {
        self.items
            .iter()
            .filter(|item| item.parent.as_deref() == Some(id))
            .collect()
    }

    fn open_children(&self, id: &str) -> Vec<&Item> {
        self.children(id)
            .into_iter()
            .filter(|child| child.is_open())
            .collect()
    }

    /// Open items in list order.
    pub fn open_items(&self) -> Vec<&Item> {
        self.items.iter().filter(|item| item.is_open()).collect()
    }

    /// Items currently `in_progress`.
    pub fn in_progress(&self) -> Vec<&Item> {
        self.items
            .iter()
            .filter(|item| item.status == Status::InProgress)
            .collect()
    }

    pub fn depth_of(&self, id: &str) -> usize {
        // Bounded: a malformed parent cycle must not hang the process.
        let mut depth = 0;
        let mut cursor = self.get(id).and_then(|item| item.parent.as_deref());
        while let Some(parent) = cursor {
            depth += 1;
            if depth > MAX_DEPTH + 1 {
                break;
            }
            cursor = self.get(parent).and_then(|item| item.parent.as_deref());
        }
        depth
    }

    // ---- mutation ---------------------------------------------------------

    fn mint_id(&mut self) -> String {
        let id = format!("t{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn touch(&mut self, at: u64) {
        self.version += 1;
        self.updated_ms = at;
    }

    /// Add a task. `parent` must exist, and a parent may not itself be a
    /// subtask (two levels: task, then subtask).
    pub fn add(
        &mut self,
        title: &str,
        detail: &str,
        parent: Option<&str>,
        at: u64,
    ) -> Outcome<String> {
        let title = title.trim();
        if title.is_empty() {
            return refuse("`title` is required: say what the task actually is");
        }
        if let Some(parent_id) = parent {
            let Some(parent_item) = self.get(parent_id) else {
                return refuse(format!(
                    "no task with id `{parent_id}`. Call the `list` tool to see the ids that exist."
                ));
            };
            if self.depth_of(parent_id) >= MAX_DEPTH - 1 {
                return refuse(format!(
                    "`{parent_id}` is already a subtask; tasks nest {MAX_DEPTH} levels deep, \
                     so a subtask of a subtask is not allowed"
                ));
            }
            if !parent_item.is_open() {
                return refuse(format!(
                    "`{parent_id}` is {}; a subtask cannot be added to a task that is no longer open",
                    parent_item.status
                ));
            }
        }
        let id = self.mint_id();
        let mut item = Item::new(id.clone(), title.to_string(), at);
        item.detail = detail.trim().to_string();
        item.parent = parent.map(|p| p.to_string());
        self.items.push(item);
        self.touch(at);
        Ok(id)
    }

    /// Apply a patch, enforcing every rule in the module docs.
    ///
    /// Returns the id, plus a short note when the transition was legal but
    /// suspicious (a `done` with no `in_progress` history), so the tool result
    /// can say so while still succeeding.
    pub fn update(&mut self, id: &str, patch: Patch, at: u64) -> Outcome<(String, Option<String>)> {
        let max_in_progress = self.max_in_progress();
        let mut note = None;

        let Some(index) = self.position(id) else {
            return refuse(format!(
                "no task with id `{id}`. Call the `list` tool to see the ids that exist."
            ));
        };

        // --- validate everything before writing anything --------------------
        if let Some(title) = patch.title.as_deref() {
            if title.trim().is_empty() {
                return refuse("`title` cannot be emptied; pass the new text or omit it");
            }
        }

        if let Some(status) = patch.status {
            // 1. The status has to earn itself.
            if let Some((field, why)) = status.required_detail() {
                let supplied = match field {
                    "evidence" => patch.evidence.as_deref(),
                    "limitation" => patch.limitation.as_deref(),
                    _ => patch.reason.as_deref(),
                };
                let existing = match field {
                    "evidence" => self.items[index].evidence.as_str(),
                    "limitation" => self.items[index].limitation.as_str(),
                    _ => self.items[index].reason.as_str(),
                };
                let value = supplied.unwrap_or(existing).trim();
                if value.is_empty() {
                    return refuse(format!(
                        "marking `{id}` as `{}` requires `{field}`: {why}. \
                         If that is not true yet, use `partial` (implemented but limited) \
                         or leave it `in_progress`.",
                        status.as_str()
                    ));
                }
            }

            // 2. A parent with open children is not done.
            if matches!(status, Status::Done | Status::Cancelled) {
                let open = self.open_children(id);
                if !open.is_empty() {
                    let names = open
                        .iter()
                        .map(|child| format!("{} ({})", child.id, child.status))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return refuse(format!(
                        "`{id}` still has open subtasks: {names}. Close those first \
                         (each needs its own status and evidence), or mark `{id}` `partial` \
                         and say what the gap is."
                    ));
                }
            }

            // 3. One thing at a time, unless the caller says otherwise.
            if status == Status::InProgress && !patch.allow_parallel {
                let others: Vec<&Item> = self
                    .items
                    .iter()
                    .filter(|item| item.id != id && item.status == Status::InProgress)
                    .collect();
                if others.len() >= max_in_progress {
                    let names = others
                        .iter()
                        .map(|item| format!("{} ({})", item.id, item.title))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return refuse(format!(
                        "`{id}` cannot start while {names} is still `in_progress`. \
                         Close it first - `done` needs `evidence`, `partial` needs \
                         `limitation`, `blocked`/`deferred` need `reason` - or pass \
                         `allow_parallel: true` if these genuinely run at the same time."
                    ));
                }
            }

            // 4. A drive-by completion is legal but recorded.
            let item = &self.items[index];
            if status == Status::Done && !item.started && item.status == Status::Pending {
                note = Some(format!(
                    "`{id}` went straight from pending to done. That is allowed, but it was \
                     never in progress - if this was not verified just now, say how it was \
                     verified in `evidence`."
                ));
            }
        }

        // --- write -----------------------------------------------------------
        let item = &mut self.items[index];
        if let Some(title) = patch.title {
            item.title = title.trim().to_string();
        }
        if let Some(detail) = patch.detail {
            item.detail = detail.trim().to_string();
        }
        if let Some(evidence) = patch.evidence {
            item.evidence = evidence.trim().to_string();
        }
        if let Some(limitation) = patch.limitation {
            item.limitation = limitation.trim().to_string();
        }
        if let Some(reason) = patch.reason {
            item.reason = reason.trim().to_string();
        }
        if let Some(status) = patch.status {
            item.status = status;
            if status == Status::InProgress {
                item.started = true;
            }
            // A status that stops work keeps only the field that explains it, so
            // the rendered list never shows "blocked" with a stale evidence
            // string that no longer applies.
            if matches!(
                status,
                Status::Blocked | Status::Deferred | Status::Cancelled
            ) {
                item.limitation.clear();
            }
        }
        item.updated_ms = at;
        self.touch(at);
        Ok((id.to_string(), note))
    }

    /// Remove a task and its subtasks. Returns the removed ids.
    pub fn remove(&mut self, id: &str, at: u64) -> Outcome<Vec<String>> {
        if self.get(id).is_none() {
            return refuse(format!(
                "no task with id `{id}`. Call the `list` tool to see the ids that exist."
            ));
        }
        let mut doomed = vec![id.to_string()];
        let mut cursor = 0;
        while cursor < doomed.len() {
            let parent = doomed[cursor].clone();
            for child in self.children(&parent) {
                doomed.push(child.id.clone());
            }
            cursor += 1;
        }
        self.items.retain(|item| !doomed.contains(&item.id));
        self.touch(at);
        Ok(doomed)
    }

    /// Replace the whole list: what a planning call does when it plans again.
    pub fn clear(&mut self, at: u64) {
        self.items.clear();
        self.next_id = 1;
        self.touch(at);
    }

    // ---- limits -----------------------------------------------------------

    /// Concurrent `in_progress` ceiling. One by default; the settings table can
    /// raise it for genuinely parallel work.
    fn max_in_progress(&self) -> usize {
        crate::config::max_in_progress()
    }

    // ---- audit ------------------------------------------------------------

    /// Everything about this list that a careful reviewer would flag.
    ///
    /// This is the honest counterpart to "the agent said it was done": it is
    /// computed from the stored history, not from what anyone claimed.
    pub fn audit(&self) -> Audit {
        let in_progress: Vec<String> = self.in_progress().iter().map(|i| i.id.clone()).collect();

        let unverified: Vec<String> = self
            .items
            .iter()
            .filter(|item| item.status == Status::Done && !item.started)
            .map(|item| item.id.clone())
            .collect();

        let missing_evidence: Vec<String> = self
            .items
            .iter()
            .filter(|item| item.status == Status::Done && item.evidence.trim().is_empty())
            .map(|item| item.id.clone())
            .collect();

        let missing_limitation: Vec<String> = self
            .items
            .iter()
            .filter(|item| item.status == Status::Partial && item.limitation.trim().is_empty())
            .map(|item| item.id.clone())
            .collect();

        let open: Vec<String> = self.open_items().iter().map(|i| i.id.clone()).collect();

        let mut counts: BTreeMap<&'static str, u32> = BTreeMap::new();
        for item in &self.items {
            *counts.entry(item.status.as_str()).or_insert(0) += 1;
        }

        Audit {
            total: self.items.len(),
            open,
            in_progress,
            unverified,
            missing_evidence,
            missing_limitation,
            counts,
        }
    }
}

/// The result of [`List::audit`].
#[derive(Debug, Clone)]
pub struct Audit {
    pub total: usize,
    pub open: Vec<String>,
    pub in_progress: Vec<String>,
    pub unverified: Vec<String>,
    pub missing_evidence: Vec<String>,
    pub missing_limitation: Vec<String>,
    pub counts: BTreeMap<&'static str, u32>,
}

impl Audit {
    /// True when there is nothing a reviewer would flag.
    pub fn is_clean(&self) -> bool {
        self.open.is_empty()
            && self.unverified.is_empty()
            && self.missing_evidence.is_empty()
            && self.missing_limitation.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> List {
        List::default()
    }

    fn patch_to(status: Status) -> Patch {
        Patch {
            status: Some(status),
            ..Patch::default()
        }
    }

    #[test]
    fn status_parsing_accepts_the_words_models_use() {
        assert_eq!(Status::parse("not implemented"), Some(Status::Pending));
        assert_eq!(Status::parse("in-progress"), Some(Status::InProgress));
        assert_eq!(Status::parse("implemented"), Some(Status::Done));
        assert_eq!(
            Status::parse("implemented but limited"),
            Some(Status::Partial)
        );
        assert_eq!(Status::parse("WIP"), Some(Status::InProgress));
        assert_eq!(Status::parse("postponed"), Some(Status::Deferred));
        assert_eq!(Status::parse("nonsense"), None);
    }

    #[test]
    fn subtasks_hang_off_a_task_and_nest_twice() {
        let mut l = list();
        let task = l.add("Ship it", "", None, 1).unwrap();
        let sub = l.add("Write the test", "", Some(&task), 2).unwrap();
        assert_eq!(l.get(&sub).unwrap().parent.as_deref(), Some(task.as_str()));
        let too_deep = l.add("Nested again", "", Some(&sub), 3);
        let refused = too_deep.expect_err("three levels must be refused");
        assert!(refused.0.contains("2 levels"), "{}", refused.0);
    }

    #[test]
    fn a_parent_cannot_close_while_a_child_is_open() {
        let mut l = list();
        let task = l.add("Ship it", "", None, 1).unwrap();
        let sub = l.add("Write the test", "", Some(&task), 2).unwrap();
        let refused = l
            .update(
                &task,
                Patch {
                    status: Some(Status::Done),
                    evidence: Some("merged".into()),
                    ..Patch::default()
                },
                3,
            )
            .unwrap_err();
        assert!(refused.0.contains(&sub), "{}", refused.0);
        assert!(refused.0.contains("open subtasks"));
    }

    #[test]
    fn done_requires_evidence() {
        let mut l = list();
        let id = l.add("Task", "", None, 1).unwrap();
        let refused = l
            .update(
                &id,
                Patch {
                    status: Some(Status::InProgress),
                    ..Patch::default()
                },
                2,
            )
            .unwrap();
        assert_eq!(refused.1, None);
        let refused = l.update(&id, patch_to(Status::Done), 3).unwrap_err();
        assert!(refused.0.contains("requires `evidence`"), "{}", refused.0);
        l.update(
            &id,
            Patch {
                status: Some(Status::Done),
                evidence: Some("cargo test passed".into()),
                ..Patch::default()
            },
            4,
        )
        .unwrap();
        assert_eq!(l.get(&id).unwrap().status, Status::Done);
    }

    #[test]
    fn partial_requires_a_limitation_and_makes_it_visible() {
        let mut l = list();
        let id = l.add("Task", "", None, 1).unwrap();
        let refused = l.update(&id, patch_to(Status::Partial), 2).unwrap_err();
        assert!(refused.0.contains("`limitation`"));
        l.update(
            &id,
            Patch {
                status: Some(Status::Partial),
                limitation: Some("no retry on 429".into()),
                ..Patch::default()
            },
            3,
        )
        .unwrap();
        assert_eq!(l.get(&id).unwrap().limitation, "no retry on 429");
        assert!(l.get(&id).unwrap().is_open(), "partial is still open work");
    }

    #[test]
    fn only_one_item_is_in_progress_by_default() {
        let mut l = list();
        let a = l.add("A", "", None, 1).unwrap();
        let b = l.add("B", "", None, 2).unwrap();
        l.update(&a, patch_to(Status::InProgress), 3).unwrap();
        let refused = l.update(&b, patch_to(Status::InProgress), 4).unwrap_err();
        assert!(refused.0.contains(&a), "{}", refused.0);
        assert!(refused.0.contains("allow_parallel"));
        l.update(
            &b,
            Patch {
                status: Some(Status::InProgress),
                allow_parallel: true,
                ..Patch::default()
            },
            5,
        )
        .unwrap();
        assert_eq!(l.in_progress().len(), 2);
    }

    #[test]
    fn reopening_the_same_item_is_not_blocked_by_the_wip_limit() {
        let mut l = list();
        let a = l.add("A", "", None, 1).unwrap();
        l.update(&a, patch_to(Status::InProgress), 2).unwrap();
        l.update(&a, patch_to(Status::InProgress), 3).unwrap();
        assert_eq!(l.in_progress().len(), 1);
    }

    #[test]
    fn drive_by_completion_succeeds_but_is_noted_and_audited() {
        let mut l = list();
        let id = l.add("Task", "", None, 1).unwrap();
        let (_, note) = l
            .update(
                &id,
                Patch {
                    status: Some(Status::Done),
                    evidence: Some("trust me".into()),
                    ..Patch::default()
                },
                2,
            )
            .unwrap();
        assert!(note.unwrap().contains("never in progress"));
        assert_eq!(l.audit().unverified, vec![id]);
    }

    #[test]
    fn removing_a_task_removes_its_subtasks() {
        let mut l = list();
        let task = l.add("Ship it", "", None, 1).unwrap();
        let sub = l.add("Test", "", Some(&task), 2).unwrap();
        let gone = l.remove(&task, 3).unwrap();
        assert_eq!(gone, vec![task, sub]);
        assert!(l.items.is_empty());
    }

    #[test]
    fn unknown_ids_are_refused_with_the_fix() {
        let mut l = list();
        let refused = l.update("t99", patch_to(Status::Done), 1).unwrap_err();
        assert!(refused.0.contains("no task with id `t99`"));
        assert!(refused.0.contains("`list` tool"));
    }

    #[test]
    fn clearing_an_explanation_field_is_allowed_but_a_missing_one_is_not() {
        let mut l = list();
        let id = l.add("Task", "", None, 1).unwrap();
        l.update(
            &id,
            Patch {
                status: Some(Status::Blocked),
                reason: Some("waiting on review".into()),
                ..Patch::default()
            },
            2,
        )
        .unwrap();
        // Clearing the reason while staying blocked must fail, because the
        // cleared value would be an unexplained blocked item.
        let refused = l
            .update(
                &id,
                Patch {
                    status: Some(Status::Blocked),
                    reason: Some(String::new()),
                    ..Patch::default()
                },
                3,
            )
            .unwrap_err();
        assert!(refused.0.contains("`reason`"));
    }

    #[test]
    fn audit_reports_a_clean_list_as_clean() {
        let mut l = list();
        let id = l.add("Task", "", None, 1).unwrap();
        l.update(&id, patch_to(Status::InProgress), 2).unwrap();
        l.update(
            &id,
            Patch {
                status: Some(Status::Done),
                evidence: Some("cargo test -p kc-todo".into()),
                ..Patch::default()
            },
            3,
        )
        .unwrap();
        assert!(l.audit().is_clean());
    }
}
