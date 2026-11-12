//! The pacing contract that goes into the system prompt.
//!
//! This is the cheapest layer and the one that runs on every model call, so it is
//! also the one with the strictest constraints. Two facts shape it:
//!
//! ## It must be byte-stable
//!
//! `before_prompt` output becomes part of message 0. The host re-requests it on
//! **every** model call, and providers cache on an exact prefix: one changed byte
//! at position 1 re-bills the whole conversation. So this function returns the
//! same string for every session, at every iteration, forever - no counters, no
//! timestamps, no per-session state. A test pins that.
//!
//! That is also why the block is emitted unconditionally rather than only when
//! the active model is a DeepSeek one. The model can be re-pinned mid-session
//! (`PATCH /api/sessions/:id/model`), and a block that appears or disappears when
//! it does would invalidate the cache precisely when a long run can least afford
//! it. The advice is generic enough to be harmless elsewhere.
//!
//! ## It must not be a trailing note
//!
//! Measured on this harness, at the point where the model decides to write a
//! file: a two-line note appended *after* the last turn made it deliberate
//! instead of acting - 8k tokens of reasoning, no tool call - while the same
//! text at the *front* committed normally. `before_prompt` is the front, which
//! is exactly the placement that works.
//!
//! ## What it says, and why
//!
//! The core fact is one the model cannot observe about itself: karucode **streams
//! and discards** the reasoning channel. It is never persisted and never replayed,
//! so a decision reached in reasoning and not acted on is gone - the next turn
//! re-derives it from scratch, pays for it again, and often reaches a slightly
//! different answer. That is the machine-level explanation for the second-guessing
//! loop, and stating it is more useful than telling the model to "be decisive",
//! which is advice about a symptom.
//!
//! The rest counters the specific habit the failure reports describe: announcing
//! an action ("Let me write the file") and then continuing to deliberate. Naming
//! the phrase pattern helps a model that has been trained into it.

/// The built-in contract.
///
/// Kept short on purpose - it shares the system prompt with the harness's own
/// instructions and every other plugin's notes, and the host caps contributions
/// at 4 KiB each. This is ~1.2 KiB.
pub const DEFAULT_PROMPT: &str = "\
Pacing: your reasoning is streamed and then discarded. It is not saved and never \
replayed to you, so a decision you reach while thinking and do not act on is gone: the next \
turn re-derives it from the same context, costs another full pass, and may land somewhere \
slightly different. That is how a model ends up second-guessing work it had already settled.

So decide once and act in the same turn:
- When you conclude what to do, do it now: call the tool, or write the answer. Do not announce \
it and keep deliberating. \"Let me write the file\" (or \"time to write\", \"OK, writing now\") \
followed by more reasoning is the one pattern that reliably burns a whole turn: the file is \
never written and the budget is spent.
- One action per turn is enough to make progress. Reading, measuring and checking are cheap; \
re-deciding something you already decided is not.
- If you catch yourself re-opening a decision you already made, stop and take the action you \
already chose. If the action turns out wrong, the next turn has real information to correct it \
with - a tool result, not a memory.
- Say what you concluded in the answer text, not only in reasoning. The answer channel is the \
only one that survives.";

/// The block this plugin contributes, or `None` when it is turned off.
pub fn prompt_block(configured: &str) -> Option<String> {
    let text = if configured.trim().is_empty() {
        DEFAULT_PROMPT
    } else {
        configured
    };
    if text.trim().is_empty() {
        return None;
    }
    // One trailing newline, and nothing that varies: the host joins this with
    // other plugins' notes into the system prompt, which is cached by prefix.
    Some(text.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_block_is_stable_across_calls() {
        // This is the whole prompt-cache contract in one assertion: a byte that
        // changes between calls costs a full-prefix re-bill.
        let a = prompt_block("").unwrap();
        let b = prompt_block("");
        assert_eq!(b.as_deref(), Some(a.as_str()));
    }

    #[test]
    fn the_default_block_names_the_discarded_reasoning_channel() {
        let text = prompt_block("").unwrap();
        assert!(text.contains("discarded"), "{text}");
        assert!(text.contains("not saved"), "{text}");
    }

    #[test]
    fn the_default_block_names_the_announcement_pattern() {
        let text = prompt_block("").unwrap();
        // The exact habit the failure reports describe, quoted so a model that has
        // been trained into it recognises itself.
        assert!(text.contains("Let me write the file"), "{text}");
        assert!(text.contains("act"), "{text}");
    }

    #[test]
    fn the_block_is_well_under_the_host_cap() {
        // The host truncates a contribution at 4 KiB with no error, so a block
        // that grew past it would be cut mid-sentence and silently lose its point.
        let text = prompt_block("").unwrap();
        assert!(text.len() < 3500, "{} bytes", text.len());
    }

    #[test]
    fn the_block_has_no_trailing_whitespace_or_blank_lines() {
        let text = prompt_block("").unwrap();
        assert!(!text.ends_with('\n'));
        assert!(!text.ends_with(' '));
        assert!(!text.contains("\n\n\n"), "no ragged blank-line runs");
    }

    #[test]
    fn a_configured_prompt_replaces_the_default() {
        // Trailing whitespace is trimmed (it would otherwise be part of the
        // cached prefix for no reason), but leading indentation is the
        // operator's formatting and is kept.
        let text = prompt_block("  Be brief.  \n\n").unwrap();
        assert_eq!(text, "  Be brief.");
        assert!(!text.contains("streamed and then discarded"));
    }

    #[test]
    fn a_whitespace_only_prompt_falls_back_to_the_default_not_to_nothing() {
        // A blank `prompt_text` means "unset", which is what an operator gets by
        // leaving the key out. Turning the block off is `prompt = false`, and
        // that is decided by the caller before this function is reached, so a
        // stray space can never silently disarm the whole prevention layer.
        assert_eq!(prompt_block("   ").as_deref(), prompt_block("").as_deref());
        assert!(prompt_block("   ")
            .unwrap()
            .contains("streamed and then discarded"));
    }
}
