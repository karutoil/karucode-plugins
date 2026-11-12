//! How the list is rendered: to the model (the prompt block and tool results)
//! and to the UI (the tab's JSON view).
//!
//! # The prompt block, and why it is built this way
//!
//! `before_prompt` runs on **every model call** and its output becomes part of
//! the system prompt — the prefix providers cache. A block that changed every
//! iteration would miss the cache on every turn and re-bill the whole context.
//! So the block is:
//!
//! * **deterministic** — no timestamps, no counters, no randomness. The same
//!   list renders byte-for-byte the same text;
//! * **bounded** — past `compact_from` items it collapses to per-status counts
//!   plus only the items that need attention, so a 200-item list does not eat
//!   the context;
//! * **memoised per session and per list version** — recomputed only when the
//!   list actually changed (see [`crate::session`]).
//!
//! # Why the block nags
//!
//! The failure this plugin exists to prevent is a run that ends with the list
//! still saying `in_progress`. A prompt block that merely *shows* the list is
//! easy to ignore. So the block always leads with what is unfinished, names the
//! carry-over from the previous run explicitly, and states the rule that closes
//! an item (which field the status needs). The model reads that before every
//! single tool call it makes.

use crate::model::{Audit, Item, List, Status};

/// UI label for a status. The UI and the prompt use the same words so a user
/// reading the tab and a model reading the prompt cannot disagree.
pub fn status_label(status: Status) -> &'static str {
    match status {
        Status::Pending => "Not implemented",
        Status::InProgress => "In progress",
        Status::Blocked => "Blocked",
        Status::Deferred => "Deferred",
        Status::Partial => "Implemented (limited)",
        Status::Done => "Implemented",
        Status::Cancelled => "Cancelled",
    }
}

/// One-line reminder of what a status needs, used in tool results and the prompt.
pub fn requirement(status: Status) -> &'static str {
    match status {
        Status::Done => "`evidence` (what proves it)",
        Status::Partial => "`limitation` (what is still missing)",
        Status::Blocked | Status::Deferred | Status::Cancelled => "`reason` (why work stops)",
        Status::Pending | Status::InProgress => "nothing extra",
    }
}

/// A tree of items in display order: each task followed by its subtasks.
///
/// Items whose `parent` no longer exists are promoted to the top level rather
/// than dropped, so a hand-edited file cannot hide work.
pub fn tree(list: &List) -> Vec<(usize, &Item)> {
    let mut out = Vec::new();
    for item in list.items.iter().filter(|item| {
        item.parent
            .as_deref()
            .map(|parent| list.get(parent).is_none())
            .unwrap_or(true)
    }) {
        out.push((0, item));
        for child in list.children(&item.id) {
            out.push((1, child));
        }
    }
    out
}

/// `t3 Write the parser` plus the marker that makes the status scannable.
fn item_line(item: &Item) -> String {
    let marker = match item.status {
        Status::Pending => "[ ]",
        Status::InProgress => "[~]",
        Status::Blocked => "[!]",
        Status::Deferred => "[>]",
        Status::Partial => "[=]",
        Status::Done => "[x]",
        Status::Cancelled => "[-]",
    };
    format!("{} {} {}", marker, item.id, item.title)
}

/// The explanation attached to an item, if it has one worth showing.
fn item_note(item: &Item) -> Option<String> {
    match item.status {
        Status::Done if !item.evidence.is_empty() => Some(format!("done: {}", item.evidence)),
        Status::Partial if !item.limitation.is_empty() => {
            Some(format!("limited: {}", item.limitation))
        }
        Status::Blocked | Status::Deferred | Status::Cancelled if !item.reason.is_empty() => {
            Some(format!("reason: {}", item.reason))
        }
        _ => None,
    }
}

/// One line per item, indented by depth, with its explanation.
fn render_full(rows: &[(usize, &Item)]) -> Vec<String> {
    let mut out = Vec::with_capacity(rows.len());
    for (depth, item) in rows {
        let indent = "  ".repeat(*depth);
        let mut line = format!("{indent}{}", item_line(item));
        if let Some(note) = item_note(item) {
            line.push_str(" — ");
            line.push_str(&note);
        }
        if item.status == Status::InProgress {
            line.push_str("  <-- active");
        }
        out.push(line);
    }
    out
}

/// The prompt block for a list.
///
/// Returns `None` when the list is empty: a project with no list must not have
/// a TODO paragraph in every prompt, because an empty list is not a rule the
/// agent needs to read on every turn.
pub fn prompt_block(list: &List, carry_over: &[String]) -> Option<String> {
    if list.items.is_empty() && carry_over.is_empty() {
        return None;
    }

    let audit = list.audit();
    let rows = tree(list);
    let mut out = Vec::new();

    out.push("[TODO list]".to_string());

    // Lead with the unfinished work. The agent's attention is best spent there.
    if !audit.open.is_empty() {
        // Name a real open item in the example, so the call is copy-pasteable
        // and cannot point at something already closed.
        let example = audit
            .open
            .iter()
            .filter_map(|id| list.get(id))
            .next()
            .map(|item| item.id.as_str())
            .unwrap_or("t1");
        out.push(format!(
            "{} of {} item(s) are still open. Close each as you finish it, in the same turn \
             as the work: `todo_update {{\"id\":\"{example}\",\"status\":\"done\",\
             \"evidence\":\"...\"}}`.",
            audit.open.len(),
            audit.total
        ));
    } else if list.items.is_empty() {
        out.push("The list is empty of open work.".to_string());
    } else {
        out.push(format!(
            "All {} item(s) are closed. If you find new work, add it with `todo_add`.",
            audit.total
        ));
    }

    // The carry-over from the previous run is the single most valuable line
    // here: it is the literal answer to "the agent forgot to mark things off".
    for line in carry_over {
        out.push(line.clone());
    }

    // Bounded rendering: past the threshold, counts plus attention items only.
    if rows.len() > crate::config::compact_from() {
        let mut counts: Vec<String> = Vec::new();
        for status in [
            Status::InProgress,
            Status::Pending,
            Status::Blocked,
            Status::Deferred,
            Status::Partial,
            Status::Done,
            Status::Cancelled,
        ] {
            if let Some(count) = audit.counts.get(status.as_str()) {
                counts.push(format!("{} {}", count, status_label(status).to_lowercase()));
            }
        }
        out.push(format!(
            "{} items: {}. The full list is in the Todo tab; call `todo_list` to read it.",
            audit.total,
            counts.join(", ")
        ));
        let attention: Vec<(usize, &Item)> = rows
            .iter()
            .copied()
            .filter(|(_, item)| item.status != Status::Done && item.status != Status::Cancelled)
            .take(12)
            .collect();
        if !attention.is_empty() {
            out.push("Open items:".to_string());
            out.extend(render_full(&attention));
            if audit.open.len() > attention.len() {
                out.push(format!(
                    "… and {} more open item(s); `todo_list` shows all of them.",
                    audit.open.len() - attention.len()
                ));
            }
        }
    } else {
        out.extend(render_full(&rows));
    }

    // The audit: what a reviewer would flag. Stated plainly so it is fixable in
    // the same turn rather than discovered by the human later.
    if !audit.unverified.is_empty() {
        out.push(format!(
            "Unverified: {} marked done without ever being in progress ({}). If that was \
             not actually verified, add the evidence or set the honest status.",
            audit.unverified.join(", "),
            audit
                .unverified
                .iter()
                .filter_map(|id| list.get(id).map(|item| item.title.as_str()))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if !audit.missing_evidence.is_empty() {
        out.push(format!(
            "Missing evidence: {}. `partial` with a stated limitation is the honest \
             status for work that is not finished.",
            audit.missing_evidence.join(", ")
        ));
    }
    if !audit.missing_limitation.is_empty() {
        out.push(format!(
            "Missing limitation: {}.",
            audit.missing_limitation.join(", ")
        ));
    }

    out.push(
        "Statuses: pending (not implemented), in_progress, blocked (needs `reason`), \
         deferred (needs `reason`), partial (implemented but limited, needs `limitation`), \
         done (needs `evidence`), cancelled (needs `reason`). A parent cannot be `done` \
         while a subtask is open."
            .to_string(),
    );

    Some(out.join("\n"))
}

/// The `on_run_end` reminder: what was left open when the run stopped.
///
/// Returned as one line so the next prompt block can lead with it. `None` when
/// nothing was left open, or when the reminder is switched off.
pub fn carry_over_line(list: &List, run_id: &str) -> Option<String> {
    if !crate::config::carry_over_reminder() {
        return None;
    }
    let in_progress: Vec<String> = list
        .items
        .iter()
        .filter(|item| item.status == Status::InProgress)
        .map(|item| format!("{} ({})", item.id, item.title))
        .collect();
    let pending: Vec<String> = list
        .items
        .iter()
        .filter(|item| item.status == Status::Pending)
        .map(|item| format!("{} ({})", item.id, item.title))
        .collect();
    if in_progress.is_empty() && pending.is_empty() {
        return None;
    }

    let _ = run_id;
    let mut parts = Vec::new();
    if !in_progress.is_empty() {
        parts.push(format!("still `in_progress` — {}", in_progress.join(", ")));
    }
    if !pending.is_empty() {
        parts.push(format!("never started — {}", pending.join(", ")));
    }
    Some(format!(
        "Carried over from the last run: {}. Finish these, or record why they stopped.",
        parts.join("; ")
    ))
}

/// The tool result for `todo_list` and for a successful mutation.
///
/// The model sees this text, so it carries the list and one actionable next step
/// rather than a bare "ok".
pub fn render_tool_list(list: &List, heading: &str) -> String {
    if list.items.is_empty() {
        return format!(
            "{heading}\nThe TODO list is empty. Plan the work with `todo_add` before starting."
        );
    }
    let rows = tree(list);
    let audit = list.audit();
    let mut out = vec![format!("{heading} ({} item(s))", audit.total)];
    out.extend(render_full(&rows));
    if !audit.open.is_empty() {
        out.push(format!(
            "{} open. Next: work the top open item and close it in the same turn as the work.",
            audit.open.len()
        ));
    }
    if !audit.unverified.is_empty() {
        out.push(format!(
            "Unverified completions: {}. Add `evidence` or set an honest status.",
            audit.unverified.join(", ")
        ));
    }
    out.join("\n")
}

/// A compact description of one mutation, for the model's tool result.
pub fn render_change(action: &str, list: &List, ids: &[String]) -> String {
    let audit = list.audit();
    let mut out = vec![format!("{action}: {}", ids.join(", "))];
    if !audit.open.is_empty() {
        let open: Vec<String> = audit
            .open
            .iter()
            .filter_map(|id| list.get(id))
            .map(|item| format!("{} {}", item.id, status_label(item.status).to_lowercase()))
            .collect();
        out.push(format!("Still open: {}.", open.join(", ")));
    } else {
        out.push("Nothing is open now.".to_string());
    }
    out.join("\n")
}

/// The UI's view of the list: everything the tab needs, in one object.
pub fn to_view(list: &List) -> serde_json::Value {
    let audit = list.audit();
    let rows = tree(list);
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|(depth, item)| {
            serde_json::json!({
                "id": item.id,
                "title": item.title,
                "status": item.status.as_str(),
                "label": status_label(item.status),
                "parent": item.parent,
                "depth": depth,
                "detail": item.detail,
                "evidence": item.evidence,
                "limitation": item.limitation,
                "reason": item.reason,
                "started": item.started,
                "created_ms": item.created_ms,
                "updated_ms": item.updated_ms,
                "open": item.is_open(),
                "needs": requirement(item.status),
                "children": list
                    .children(&item.id)
                    .iter()
                    .map(|child| child.id.clone())
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    serde_json::json!({
        "schema": list.schema,
        "version": list.version,
        "updated_ms": list.updated_ms,
        "items": items,
        "audit": {
            "total": audit.total,
            "open": audit.open,
            "in_progress": audit.in_progress,
            "unverified": audit.unverified,
            "missing_evidence": audit.missing_evidence,
            "missing_limitation": audit.missing_limitation,
            "counts": audit.counts,
            "clean": audit_is_clean(&audit),
        },
        // A run-end audit records what was open *at that moment*. Items may have
        // been closed since, so each entry also carries the subset that is still
        // open now: the UI must not tell a user "left open by the last run" about
        // work that has since been finished.
        "carry_over": list.carry_over.iter().map(|run| {
            let still_open: Vec<&str> = run
                .in_progress
                .iter()
                .chain(run.pending.iter())
                .filter(|id| list.get(id).map(|item| item.is_open()).unwrap_or(false))
                .map(|id| id.as_str())
                .collect();
            serde_json::json!({
                "run_id": run.run_id,
                "at_ms": run.at_ms,
                "in_progress": run.in_progress,
                "pending": run.pending,
                "still_open": still_open,
            })
        }).collect::<Vec<_>>(),
    })
}

fn audit_is_clean(audit: &Audit) -> bool {
    audit.is_clean()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{List, Patch};

    fn sample() -> List {
        let mut list = List::default();
        let task = list.add("Ship the parser", "", None, 1).unwrap();
        list.add("Write the test", "", Some(&task), 2).unwrap();
        list.add("Handle CRLF", "", Some(&task), 3).unwrap();
        list.update(
            &task,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            4,
        )
        .unwrap();
        list
    }

    #[test]
    fn status_labels_use_the_requested_vocabulary() {
        assert_eq!(status_label(Status::Pending), "Not implemented");
        assert_eq!(status_label(Status::InProgress), "In progress");
        assert_eq!(status_label(Status::Deferred), "Deferred");
        assert_eq!(status_label(Status::Partial), "Implemented (limited)");
        assert_eq!(status_label(Status::Done), "Implemented");
    }

    #[test]
    fn an_empty_list_contributes_nothing_to_the_prompt() {
        assert!(prompt_block(&List::default(), &[]).is_none());
    }

    #[test]
    fn the_block_is_stable_across_calls() {
        let list = sample();
        let a = prompt_block(&list, &[]).unwrap();
        let b = prompt_block(&list, &[]).unwrap();
        assert_eq!(a, b, "the prompt block must be byte-stable for the cache");
        // No timestamps or counters leaked in.
        assert!(!a.contains(&list.updated_ms.to_string()));
    }

    #[test]
    fn subtasks_are_indented_under_their_task() {
        let list = sample();
        let block = prompt_block(&list, &[]).unwrap();
        assert!(block.contains("[~] t1 Ship the parser"), "{block}");
        assert!(block.contains("  [ ] t2 Write the test"), "{block}");
    }

    #[test]
    fn the_block_leads_with_open_work_and_the_carry_over() {
        let list = sample();
        let block = prompt_block(
            &list,
            &["Carried over: t1 was still in_progress.".to_string()],
        )
        .unwrap();
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines[0], "[TODO list]");
        assert!(lines[1].contains("still open"), "{block}");
        assert_eq!(lines[2], "Carried over: t1 was still in_progress.");
    }

    #[test]
    fn a_large_list_compacts_instead_of_flooding_the_prompt() {
        let mut list = List::default();
        for i in 0..60 {
            list.add(&format!("Task {i}"), "", None, i).unwrap();
        }
        let block = prompt_block(&list, &[]).unwrap();
        assert!(block.contains("call `todo_list` to read it"), "{block}");
        assert!(
            block.lines().count() < 30,
            "block was {} lines",
            block.lines().count()
        );
    }

    /// The host caps a plugin's `before_prompt` contribution at 4 KiB
    /// (`MAX_HOOK_NOTE_BYTES`) and truncates anything longer with
    /// `[… truncated]`. A truncated block would cut the status rules and the
    /// audit off the end, so the block must stay under that cap on its own.
    #[test]
    fn the_block_stays_under_the_host_hook_budget_even_for_a_huge_list() {
        let mut list = List::default();
        for i in 0..200 {
            list.add(
                &format!("Task number {i} with a fairly long descriptive title that takes space"),
                "",
                None,
                i,
            )
            .unwrap();
        }
        let block = prompt_block(&list, &[]).unwrap();
        assert!(
            block.len() <= 4096,
            "block was {} bytes, over the 4 KiB hook cap",
            block.len()
        );
        // The status rules are the last thing in the block: if they survive, the
        // host will not have to truncate anything the model needs.
        assert!(block.contains("A parent cannot be `done`"), "{block}");
    }

    #[test]
    fn unverified_completions_are_called_out_in_the_prompt() {
        let mut list = List::default();
        let id = list.add("Fix the bug", "", None, 1).unwrap();
        list.update(
            &id,
            Patch {
                status: Some(Status::Done),
                evidence: Some("trust me".into()),
                ..Patch::default()
            },
            2,
        )
        .unwrap();
        let block = prompt_block(&list, &[]).unwrap();
        assert!(block.contains("Unverified"), "{block}");
        assert!(block.contains(&id), "{block}");
    }

    #[test]
    fn carry_over_names_both_in_progress_and_unstarted_work() {
        let mut list = List::default();
        let a = list.add("Started", "", None, 1).unwrap();
        list.add("Ignored", "", None, 2).unwrap();
        list.update(
            &a,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            3,
        )
        .unwrap();

        let line = carry_over_line(&list, "r1").unwrap();
        assert!(line.contains("in_progress"), "{line}");
        assert!(line.contains("Started"), "{line}");
        assert!(line.contains("never started"), "{line}");
        assert!(line.contains("Ignored"), "{line}");
    }

    #[test]
    fn no_carry_over_when_everything_is_closed() {
        let mut list = List::default();
        let id = list.add("Done thing", "", None, 1).unwrap();
        list.update(
            &id,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            2,
        )
        .unwrap();
        list.update(
            &id,
            Patch {
                status: Some(Status::Done),
                evidence: Some("tests pass".into()),
                ..Patch::default()
            },
            3,
        )
        .unwrap();
        assert!(carry_over_line(&list, "r1").is_none());
    }

    #[test]
    fn the_view_exposes_every_field_the_tab_renders() {
        let list = sample();
        let view = to_view(&list);
        assert_eq!(view["items"][0]["id"], "t1");
        assert_eq!(view["items"][0]["label"], "In progress");
        assert_eq!(view["items"][1]["depth"], 1);
        assert_eq!(view["items"][1]["parent"], "t1");
        assert_eq!(view["audit"]["total"], 3);
        assert_eq!(view["audit"]["clean"], false);
        assert!(view["version"].as_u64().unwrap() > 0);
    }

    /// A run-end audit records what was open *then*. If those items have since
    /// been closed, the payload must say so, or the tab claims work is unfinished
    /// over a fully-closed list.
    #[test]
    fn carry_over_reports_which_ids_are_still_open_now() {
        let mut list = List::default();
        let a = list.add("Finished since", "", None, 1).unwrap();
        let b = list.add("Still open", "", None, 2).unwrap();
        // Close `a` so it is genuinely finished, which is the case that matters:
        // it was pending at run end and has since been completed.
        list.update(
            &a,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            2,
        )
        .unwrap();
        list.update(
            &a,
            Patch {
                status: Some(Status::Done),
                evidence: Some("done".into()),
                ..Patch::default()
            },
            3,
        )
        .unwrap();
        list.carry_over.push(crate::model::CarryOver {
            run_id: "r1".into(),
            at_ms: 3,
            in_progress: vec![],
            pending: vec![a.clone(), b.clone()],
        });

        let view = to_view(&list);
        let entry = &view["carry_over"][0];
        // Both were pending at run end…
        assert_eq!(entry["pending"], json_json(&[&a, &b]));
        // …but only one is still open now.
        assert_eq!(entry["still_open"], json_json(&[&b]));

        // Close the remaining one and the still-open set empties.
        list.update(
            &b,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            4,
        )
        .unwrap();
        list.update(
            &b,
            Patch {
                status: Some(Status::Done),
                evidence: Some("done".into()),
                ..Patch::default()
            },
            5,
        )
        .unwrap();
        let view = to_view(&list);
        assert_eq!(
            view["carry_over"][0]["still_open"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    fn json_json(ids: &[&String]) -> serde_json::Value {
        serde_json::json!(ids)
    }

    #[test]
    fn an_orphan_item_is_promoted_rather_than_hidden() {
        let mut list = List::default();
        let child = list.add("Orphan", "", None, 1).unwrap();
        // Simulate a hand-edited file pointing at an id that does not exist.
        list.items[0].parent = Some("t999".into());
        let rows = tree(&list);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.id, child);
        assert_eq!(rows[0].0, 0);
    }

    #[test]
    fn the_tool_result_always_offers_a_next_step() {
        let list = sample();
        let text = render_tool_list(&list, "TODO list");
        assert!(text.contains("TODO list (3 item(s))"), "{text}");
        assert!(text.contains("Next:"), "{text}");
    }
}
