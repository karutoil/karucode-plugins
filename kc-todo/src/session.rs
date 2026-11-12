//! Per-session state, shared by the tools and the hooks.
//!
//! A plugin process is long-lived: it serves every session the server opens, so
//! this is the one place that knows *which* workspace a call belongs to and what
//! the last prompt block looked like.
//!
//! # Why the workspace is learned, not passed
//!
//! `tools/call` carries only `{name, arguments}`, so a tool cannot see the
//! session or the workspace. `before_prompt` carries both. The plugin therefore
//! learns the workspace there and remembers the most recent one. A run is
//! sequential, so the most recent workspace is the one the next tool call
//! belongs to.
//!
//! Two sessions in two different projects running **at the same time** can
//! cross-talk: the second `before_prompt` wins and the first session's tool
//! calls then act on the wrong project. Every tool accepts an explicit
//! `workspace` argument for exactly that case, and [`Store::workspace`] reports
//! when it is guessing.
//!
//! # Why the prompt block is memoised
//!
//! `before_prompt` runs on every model call and its text is part of the cached
//! prefix. Re-rendering an unchanged list would still produce the same bytes
//! (the renderer is deterministic), but it would cost a file read and a
//! sub-millisecond of CPU on every iteration of every session. Memoising on
//! `(session, list version)` makes the common case free and, more importantly,
//! makes it structurally impossible for the block to change when the list has
//! not.

use crate::model::{CarryOver, List};
use std::collections::HashMap;
use std::sync::Mutex;

/// How many sessions to remember before evicting the oldest. A long-lived plugin
/// serves every session the server ever opens, so this map cannot be unbounded.
const MAX_SESSIONS: usize = 64;

/// How many prompt strings to cache. One per session is enough; a couple of
/// versions of slack covers a mutation between two calls.
const MAX_PROMPT_CACHE: usize = 256;

#[derive(Debug, Clone)]
struct SessionState {
    workspace: String,
    /// The list version the cached block was rendered from.
    prompt_version: Option<u32>,
    /// The rendered block. Empty string means "render `None`".
    prompt: Option<String>,
    /// The carry-over line to prepend once, consumed by the next render.
    pending_carry_over: Option<String>,
    last_seen_ms: u64,
}

/// Why a workspace could not be determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoWorkspace {
    /// Nothing has called `before_prompt` yet and no workspace is pinned, so the
    /// plugin has no idea which project a tool call belongs to.
    Unknown,
}

impl std::fmt::Display for NoWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this plugin does not know which workspace the call belongs to yet \
             (no prompt has run for this session). Pass `workspace` explicitly \
             (the absolute project path from the project rail), or set \
             `settings.workspace` in the plugin's config entry.",
        )
    }
}

/// Every session the process has seen, plus the prompt cache.
#[derive(Default)]
pub struct Store {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, SessionState>,
    /// `(session, version)` -> rendered block.
    prompt_cache: HashMap<(String, u32), Option<String>>,
    clock: u64,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means another thread panicked mid-update. The data it
        // guards is a cache: recovering it is safe, and refusing every future
        // TODO call because one render panicked would be worse than reading a
        // possibly stale string.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record the workspace for one session, from `before_prompt`.
    pub fn note_workspace(&self, session_id: &str, workspace: &str) {
        let mut inner = self.lock();
        inner.clock += 1;
        let clock = inner.clock;

        if !inner.sessions.contains_key(session_id) && inner.sessions.len() >= MAX_SESSIONS {
            // Evict the least recently seen session. Keeping the newest is what
            // matters: the active session is the one about to call a tool.
            if let Some(oldest) = inner
                .sessions
                .iter()
                .min_by_key(|(_, state)| state.last_seen_ms)
                .map(|(id, _)| id.clone())
            {
                inner.sessions.remove(&oldest);
            }
        }

        let entry = inner
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionState {
                workspace: workspace.to_string(),
                prompt_version: None,
                prompt: None,
                pending_carry_over: None,
                last_seen_ms: 0,
            });

        // A different workspace for the same session means the session moved
        // (or the id was reused): drop the cached block so it cannot describe
        // the wrong project.
        if entry.workspace != workspace {
            entry.workspace = workspace.to_string();
            entry.prompt_version = None;
            entry.prompt = None;
        }
        entry.last_seen_ms = clock;
    }

    /// The workspace to use for a tool call.
    ///
    /// Order, first hit wins: an explicit workspace from the caller, the
    /// workspace the host forwarded in `context`, an operator pin in settings,
    /// then the most recent `before_prompt`.
    ///
    /// `context_workspace` comes from `ToolRequest::workspace()`, which the host
    /// sends with every `tools/call` since panels/context support landed. It is
    /// the authoritative answer and sits above the operator pin because it names
    /// the project *this* call belongs to; the pin exists for hosts too old to
    /// send it.
    pub fn workspace_from(
        &self,
        explicit: Option<&str>,
        context_workspace: Option<&str>,
    ) -> Result<String, NoWorkspace> {
        if let Some(workspace) = explicit.map(str::trim).filter(|w| !w.is_empty()) {
            return Ok(workspace.to_string());
        }
        if let Some(workspace) = context_workspace.map(str::trim).filter(|w| !w.is_empty()) {
            return Ok(workspace.to_string());
        }
        if let Some(pinned) = crate::config::pinned_workspace() {
            return Ok(pinned);
        }
        let inner = self.lock();
        inner
            .sessions
            .values()
            .max_by_key(|state| state.last_seen_ms)
            .map(|state| state.workspace.clone())
            .ok_or(NoWorkspace::Unknown)
    }

    /// The workspace of a specific session, for the hooks (which do have an id).
    pub fn session_workspace(&self, session_id: &str) -> Option<String> {
        let inner = self.lock();
        inner
            .sessions
            .get(session_id)
            .map(|state| state.workspace.clone())
            .or_else(crate::config::pinned_workspace)
    }

    /// The session a tool call belongs to.
    ///
    /// Order, first hit wins: an explicit `session` argument, the session the
    /// host forwarded in `context`, an operator pin in settings, then the most
    /// recently seen session. Lists are per session, so getting this wrong would
    /// read another conversation's work - there is no safe default, and
    /// [`NoWorkspace`] is returned rather than guessing a shared file.
    pub fn session_from(
        &self,
        explicit: Option<&str>,
        context_session: Option<&str>,
    ) -> Result<String, NoWorkspace> {
        if let Some(session) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(session.to_string());
        }
        if let Some(session) = context_session.map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(session.to_string());
        }
        if let Some(pinned) = crate::config::pinned_session() {
            return Ok(pinned);
        }
        let inner = self.lock();
        inner
            .sessions
            .iter()
            .max_by_key(|(_, state)| state.last_seen_ms)
            .map(|(id, _)| id.clone())
            .ok_or(NoWorkspace::Unknown)
    }

    /// Queue a carry-over line for one specific session.
    ///
    /// This must also drop that session's cached prompt block. `on_run_end` does
    /// not change the list's `version` (it appends to `carry_over` and saves, but
    /// the items are untouched), so a cache keyed only by version would keep
    /// serving the pre-run block and the reminder - the whole mechanism that
    /// closes the loop on unfinished work - would never reach the model.
    pub fn queue_carry_over_for(&self, session_id: &str, line: String) {
        let mut inner = self.lock();
        inner.clock += 1;
        let clock = inner.clock;
        inner
            .prompt_cache
            .retain(|(session, _), _| session != session_id);
        if let Some(state) = inner.sessions.get_mut(session_id) {
            state.pending_carry_over = Some(line);
            state.last_seen_ms = clock;
            state.prompt_version = None;
            state.prompt = None;
        }
    }

    /// Render (or reuse) the prompt block for one session.
    ///
    /// `list` is only read when the cached block is stale, which is what keeps
    /// the steady state free.
    pub fn prompt_block(&self, session_id: &str, list: &List) -> Option<String> {
        let version = list.version;
        {
            let inner = self.lock();
            if let Some(cached) = inner.prompt_cache.get(&(session_id.to_string(), version)) {
                return cached.clone();
            }
        }

        let mut inner = self.lock();
        inner.clock += 1;
        let clock = inner.clock;

        // Take a queued carry-over exactly once, so it leads one prompt block
        // and then lives in `list.carry_over` for the UI.
        let carry_over = inner
            .sessions
            .get_mut(session_id)
            .and_then(|state| state.pending_carry_over.take());
        let carry: Vec<String> = carry_over.into_iter().collect();

        let block = crate::render::prompt_block(list, &carry);

        if inner.prompt_cache.len() >= MAX_PROMPT_CACHE {
            // Cheap bound: drop everything for sessions other than this one.
            // The cache is per (session, version), so this cannot serve stale
            // text - it only costs a re-render.
            inner
                .prompt_cache
                .retain(|(other, _), _| other == session_id);
            if inner.prompt_cache.len() >= MAX_PROMPT_CACHE {
                inner.prompt_cache.clear();
            }
        }
        inner
            .prompt_cache
            .insert((session_id.to_string(), version), block.clone());

        if let Some(state) = inner.sessions.get_mut(session_id) {
            state.prompt_version = Some(version);
            state.prompt = block.clone();
            state.last_seen_ms = clock;
        }

        block
    }

    /// Forget the memoised block for a session, after this plugin mutated the
    /// list itself. The version bump already covers it; this exists so a caller
    /// that wrote through the HTTP API (without bumping `version`) can force a
    /// re-render.
    pub fn invalidate(&self, session_id: &str) {
        let mut inner = self.lock();
        inner
            .prompt_cache
            .retain(|(session, _), _| session != session_id);
        if let Some(state) = inner.sessions.get_mut(session_id) {
            state.prompt_version = None;
            state.prompt = None;
        }
    }

    /// Record a run-end audit on the list and queue its reminder.
    ///
    /// Called from `on_run_end`, which carries the session id and run id. The
    /// list is passed in because the caller already loaded and will save it.
    pub fn note_run_end(
        &self,
        session_id: &str,
        run_id: &str,
        list: &mut List,
        at_ms: u64,
    ) -> Option<String> {
        let in_progress: Vec<String> = list
            .items
            .iter()
            .filter(|item| item.status == crate::model::Status::InProgress)
            .map(|item| item.id.clone())
            .collect();
        let pending: Vec<String> = list
            .items
            .iter()
            .filter(|item| item.status == crate::model::Status::Pending)
            .map(|item| item.id.clone())
            .collect();

        if in_progress.is_empty() && pending.is_empty() {
            return None;
        }

        let line = crate::render::carry_over_line(list, run_id)?;

        list.carry_over.push(CarryOver {
            run_id: run_id.to_string(),
            at_ms,
            in_progress,
            pending,
        });
        // Bounded: the newest runs are the ones that matter.
        const MAX_CARRY_OVER: usize = 20;
        if list.carry_over.len() > MAX_CARRY_OVER {
            let cut = list.carry_over.len() - MAX_CARRY_OVER;
            list.carry_over.drain(0..cut);
        }

        self.queue_carry_over_for(session_id, line.clone());
        Some(line)
    }

    /// Number of sessions currently remembered. Test-only introspection.
    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.lock().sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{List, Patch, Status};

    #[test]
    fn the_workspace_is_learned_from_before_prompt() {
        let store = Store::new();
        assert_eq!(store.workspace_from(None, None), Err(NoWorkspace::Unknown));
        store.note_workspace("s1", "/w/one");
        assert_eq!(store.workspace_from(None, None), Ok("/w/one".to_string()));
    }

    #[test]
    fn an_explicit_workspace_always_wins() {
        let store = Store::new();
        store.note_workspace("s1", "/w/one");
        assert_eq!(
            store.workspace_from(Some("/w/two"), None),
            Ok("/w/two".to_string())
        );
        // Blank is not an override.
        assert_eq!(
            store.workspace_from(Some("   "), None),
            Ok("/w/one".to_string())
        );
    }

    #[test]
    fn the_most_recent_session_supplies_the_workspace() {
        let store = Store::new();
        store.note_workspace("s1", "/w/one");
        store.note_workspace("s2", "/w/two");
        assert_eq!(store.workspace_from(None, None), Ok("/w/two".to_string()));
        assert_eq!(store.session_workspace("s1"), Some("/w/one".to_string()));
    }

    /// The host now forwards `context.workspace` with every `tools/call`, which
    /// is what makes two concurrent sessions in different projects safe: the
    /// call carries its own project instead of inheriting the most recent
    /// `before_prompt`.
    #[test]
    fn the_forwarded_context_wins_over_the_last_prompt() {
        let store = Store::new();
        store.note_workspace("s1", "/w/last-prompt");
        assert_eq!(
            store.workspace_from(None, Some("/w/this-call")),
            Ok("/w/this-call".to_string()),
            "the context must beat the remembered workspace"
        );
        // With no context (an older host) the learned workspace still works.
        assert_eq!(
            store.workspace_from(None, None),
            Ok("/w/last-prompt".to_string())
        );
    }

    #[test]
    fn an_explicit_argument_beats_the_forwarded_context() {
        let store = Store::new();
        assert_eq!(
            store.workspace_from(Some("/w/explicit"), Some("/w/context")),
            Ok("/w/explicit".to_string())
        );
        // A blank explicit value is not an override, and a blank context is not
        // an answer either.
        store.note_workspace("s1", "/w/learned");
        assert_eq!(
            store.workspace_from(Some("  "), Some("   ")),
            Ok("/w/learned".to_string())
        );
    }

    #[test]
    fn the_prompt_block_is_memoised_per_version() {
        let store = Store::new();
        store.note_workspace("s1", "/w");
        let mut list = List::default();
        list.add("Task", "", None, 1).unwrap();

        let first = store.prompt_block("s1", &list);
        // Mutating the passed-in list without bumping the version must not
        // change the cached block: that is the cache being honest.
        let second = store.prompt_block("s1", &list);
        assert_eq!(first, second);

        list.items[0].title = "Changed".into();
        let stale = store.prompt_block("s1", &list);
        assert_eq!(first, stale, "same version must reuse the cached block");

        // A real mutation bumps the version and re-renders.
        list.update(
            "t1",
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            2,
        )
        .unwrap();
        let fresh = store.prompt_block("s1", &list);
        assert_ne!(first, fresh);
        assert!(fresh.unwrap().contains("[~] t1 Changed"));
    }

    #[test]
    fn a_carry_over_leads_exactly_one_prompt_block() {
        let store = Store::new();
        store.note_workspace("s1", "/w");
        let mut list = List::default();
        let id = list.add("Task", "", None, 1).unwrap();
        list.update(
            &id,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            2,
        )
        .unwrap();

        store.queue_carry_over_for("s1", "Carried over: t1 was still in_progress.".into());
        let first = store.prompt_block("s1", &list).unwrap();
        assert!(first.contains("Carried over: t1"), "{first}");

        // The same version renders again (cache hit) and still has it.
        let again = store.prompt_block("s1", &list).unwrap();
        assert_eq!(first, again);

        // After a real change the reminder has been consumed.
        list.update(
            &id,
            Patch {
                status: Some(Status::Pending),
                ..Patch::default()
            },
            3,
        )
        .unwrap();
        let next = store.prompt_block("s1", &list).unwrap();
        assert!(!next.contains("Carried over"), "{next}");
    }

    #[test]
    fn a_moved_session_drops_its_cached_block() {
        let store = Store::new();
        store.note_workspace("s1", "/w/one");
        let mut list = List::default();
        list.add("Task", "", None, 1).unwrap();
        let first = store.prompt_block("s1", &list);

        store.note_workspace("s1", "/w/two");
        let after = store.prompt_block("s1", &list);
        // Same list, but the cached entry was dropped on the move, so it is
        // re-rendered (and identical, because the renderer is deterministic).
        assert_eq!(first, after);
        assert_eq!(store.session_workspace("s1"), Some("/w/two".to_string()));
    }

    #[test]
    fn run_end_records_carry_over_and_queues_the_reminder() {
        let store = Store::new();
        store.note_workspace("s1", "/w");
        let mut list = List::default();
        let id = list.add("Half done", "", None, 1).unwrap();
        list.update(
            &id,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            2,
        )
        .unwrap();

        let line = store.note_run_end("s1", "r1", &mut list, 3).unwrap();
        assert!(line.contains("in_progress"), "{line}");
        assert_eq!(list.carry_over.len(), 1);
        assert_eq!(list.carry_over[0].run_id, "r1");
        assert_eq!(list.carry_over[0].in_progress, vec![id]);
    }

    /// Regression: `on_run_end` does not bump the list version, so a prompt
    /// block cached for `(session, version)` would be served again and the
    /// carry-over reminder would never reach the model - defeating the entire
    /// mechanism that nags an agent about unfinished work.
    #[test]
    fn the_carry_over_reminder_reaches_the_prompt_even_though_the_version_is_unchanged() {
        let store = Store::new();
        store.note_workspace("s1", "/w");
        let mut list = List::default();
        let id = list.add("Half done", "", None, 1).unwrap();
        list.update(
            &id,
            Patch {
                status: Some(Status::InProgress),
                ..Patch::default()
            },
            2,
        )
        .unwrap();

        // A prompt runs mid-session and caches the block.
        let before = store.prompt_block("s1", &list).unwrap();
        assert!(!before.contains("Carried over"), "{before}");

        // The run ends with the item still in progress.
        let version_before = list.version;
        store.note_run_end("s1", "r1", &mut list, 3).unwrap();
        assert_eq!(
            list.version, version_before,
            "on_run_end must not change the version, or this test proves nothing"
        );

        // The next prompt must carry the reminder despite the same version.
        let after = store.prompt_block("s1", &list).unwrap();
        assert!(
            after.contains("Carried over"),
            "the reminder was swallowed by the prompt cache:\n{after}"
        );
        assert!(after.contains(&id), "{after}");
    }

    #[test]
    fn run_end_with_nothing_open_records_nothing() {
        let store = Store::new();
        store.note_workspace("s1", "/w");
        let mut list = List::default();
        let id = list.add("Done", "", None, 1).unwrap();
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
        assert!(store.note_run_end("s1", "r1", &mut list, 4).is_none());
        assert!(list.carry_over.is_empty());
    }

    #[test]
    fn the_session_map_is_bounded() {
        let store = Store::new();
        for i in 0..MAX_SESSIONS + 10 {
            store.note_workspace(&format!("s{i}"), "/w");
        }
        assert!(store.session_count() <= MAX_SESSIONS);
        // The newest is still known.
        assert_eq!(
            store.session_workspace(&format!("s{}", MAX_SESSIONS + 9)),
            Some("/w".to_string())
        );
    }
}
