//! The engine's plumbing: the intent queue, the reconcile tick, the POST, and the
//! log the panel shows.
//!
//! ## Why a thread and not a hook
//!
//! `on_run_end` is the trigger, but it fires inside the run's teardown and the
//! host gives a hook a few seconds. Doing HTTP there would put the plugin's
//! network latency on the critical path of every run that ends, and a hung
//! connection would look like a hung harness. So the hook only records an intent
//! and wakes this thread; everything that can fail or block happens here.
//!
//! ## Two triggers, one rulebook
//!
//! `on_run_end` is the fast path and the tick is the safety net: the tick is what
//! makes a goal survive a dropped hook, a plugin restart, a server restart, or a
//! POST that failed. Both call [`process`], which re-reads the goal file and asks
//! [`crate::engine::plan`] before doing anything, and both key their work on the
//! run id - so the safety net cannot double-continue what the fast path already
//! handled.
//!
//! ## State lives on disk
//!
//! Nothing here is authoritative. `App` holds a queue, a log and a stop flag; the
//! goal document and its counters are the state. Kill the process at any point and
//! the next one picks up where this one stopped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Settings;
use crate::engine::{self, Plan};
use crate::http::{Client, HttpError};
use crate::model::{now_ms, GoalFile};
use crate::sdk::{notify_with, NotifyFrame, NotifyKind};
use crate::store::Store;

/// How many log lines the panel keeps.
pub const MAX_LOG: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub at_ms: i64,
    pub session_id: String,
    pub run_id: String,
    pub kind: String,
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub session_id: String,
    pub run_id: String,
    pub status: String,
}

/// What the engine can see about a session. Exactly the fields the policy needs,
/// so a fake transport in a test has nothing to pretend about.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionView {
    pub exists: bool,
    pub running: bool,
    pub workspace: String,
    pub last_run_id: Option<String>,
    pub last_run_status: Option<String>,
    /// Tool calls made in this session so far, from the harness. This is how the
    /// engine tells "the run worked for an hour" from "the run did nothing", and
    /// it is the harness's count rather than the agent's own account.
    pub tool_calls: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostMode {
    Send,
    FollowUp,
}

impl PostMode {
    pub fn query(&self) -> &'static str {
        match self {
            PostMode::Send => "",
            PostMode::FollowUp => "?mode=follow_up",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    pub status: u16,
    pub body: Value,
}

impl Post {
    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The two things the engine does over the network. A trait so the policy and
/// the flow are testable against a fake server, without a server.
pub trait Transport: Send + Sync {
    fn session(&self, session_id: &str) -> Result<SessionView, String>;
    fn post(&self, session_id: &str, text: &str, mode: PostMode) -> Result<Post, String>;
}

pub struct HttpTransport {
    client: Client,
}

/// Stands in when the configured server URL cannot be parsed. Every call fails
/// with the reason, which reaches `GET /api/plugins` and the panel - far better
/// than a plugin that refuses to start because one setting is misspelled.
pub struct DeadTransport {
    reason: String,
}

impl Transport for DeadTransport {
    fn session(&self, _session_id: &str) -> Result<SessionView, String> {
        Err(self.reason.clone())
    }
    fn post(&self, _session_id: &str, _text: &str, _mode: PostMode) -> Result<Post, String> {
        Err(self.reason.clone())
    }
}

impl HttpTransport {
    pub fn new(base: &str, token: &str) -> Result<Self, String> {
        Ok(Self {
            client: Client::new(base, token).map_err(|e| e.to_string())?,
        })
    }
}

impl Transport for HttpTransport {
    fn session(&self, session_id: &str) -> Result<SessionView, String> {
        let path = format!("/api/sessions/{session_id}/info");
        let response = self.client.get(&path).map_err(|e| e.to_string())?;
        if response.status == 404 {
            return Ok(SessionView {
                exists: false,
                ..SessionView::default()
            });
        }
        if !response.is_ok() {
            return Err(format!(
                "GET {path} answered HTTP {}: {}",
                response.status,
                crate::model::truncate(response.body.trim(), 200)
            ));
        }
        let value = response.json().map_err(|e: HttpError| e.to_string())?;
        let session = value.get("session").cloned().unwrap_or(Value::Null);
        let runs = value
            .get("runs")
            .and_then(|r| r.get("recent"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // `recent` is newest first, so the first entry is the run that matters.
        let newest = runs.first();
        Ok(SessionView {
            exists: true,
            running: value
                .get("running")
                .and_then(Value::as_bool)
                .or_else(|| session.get("running").and_then(Value::as_bool))
                .unwrap_or(false),
            workspace: session
                .get("workspace")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            last_run_id: newest
                .and_then(|r| r.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            last_run_status: newest
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_calls: value
                .get("tools")
                .and_then(|t| t.get("total"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }

    fn post(&self, session_id: &str, text: &str, mode: PostMode) -> Result<Post, String> {
        let path = format!("/api/sessions/{session_id}/messages{}", mode.query());
        let response = self
            .client
            .post(&path, &json!({ "content": text }))
            .map_err(|e| e.to_string())?;
        let body = response
            .json()
            .unwrap_or_else(|_| json!({ "raw": crate::model::truncate(&response.body, 400) }));
        Ok(Post {
            status: response.status,
            body,
        })
    }
}

/// Why a continuation did not land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliverError {
    /// The session does not exist any more (404). The goal is orphaned.
    Gone(String),
    /// Anything else worth retrying on the next tick.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Continued { via: &'static str },
    Paused { reason: String },
    Skipped { reason: String },
    Failed(String),
}

impl Outcome {
    pub fn kind(&self) -> &'static str {
        match self {
            Outcome::Continued { .. } => "continue",
            Outcome::Paused { .. } => "pause",
            Outcome::Skipped { .. } => "skip",
            Outcome::Failed(_) => "error",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Outcome::Continued { via } => format!("posted ({via})"),
            Outcome::Paused { reason } => format!("paused: {reason}"),
            Outcome::Skipped { reason } => format!("skipped: {reason}"),
            Outcome::Failed(reason) => format!("failed: {reason}"),
        }
    }
}

/// What a goal transition does to a human: `(session, kind, title, body)`.
///
/// A named alias because the same boxed closure appears in the production
/// constructor and in the test one, and `type_complexity` is right to complain.
pub type Notifier = Box<dyn Fn(Option<&str>, NotifyKind, &str, &str) + Send + Sync>;

pub struct App {
    pub cfg: Settings,
    pub store: Store,
    pub transport: Box<dyn Transport>,
    pub log: Mutex<Vec<LogEntry>>,
    pub last_error: Mutex<Option<String>>,
    pending: Mutex<Vec<Intent>>,
    signal: Condvar,
    pub stopping: AtomicBool,
    /// `(session, kind, title, body)`. A field rather than a direct SDK call so a
    /// test can observe notifications without writing a protocol frame to stdout.
    pub notify: Notifier,
}

impl App {
    pub fn new(cfg: Settings) -> Arc<App> {
        let (transport, error): (Box<dyn Transport>, Option<String>) =
            match HttpTransport::new(&cfg.server, &cfg.token) {
                Ok(transport) => (Box::new(transport), None),
                Err(error) => (
                    Box::new(DeadTransport {
                        reason: error.clone(),
                    }),
                    Some(error),
                ),
            };
        let app = Arc::new(App {
            store: Store::new(cfg.state_dir.clone(), cfg.server.clone()),
            cfg,
            transport,
            log: Mutex::new(Vec::new()),
            last_error: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            signal: Condvar::new(),
            stopping: AtomicBool::new(false),
            notify: Box::new(|session, kind, title, body| {
                let _ = notify_with(NotifyFrame {
                    kind,
                    title: title.to_string(),
                    body: body.to_string(),
                    session_id: session.map(str::to_string),
                    sound: None,
                    sticky: false,
                });
            }),
        });
        if let Some(error) = error {
            app.set_error(error);
        }
        app
    }

    #[cfg(test)]
    pub fn with_transport(cfg: Settings, transport: Box<dyn Transport>) -> Arc<App> {
        Self::with_transport_and_notify(cfg, transport, Box::new(|_, _, _, _| {}))
    }

    /// Same, with a notifier a test can read back. Production installs the SDK
    /// frame in [`App::new`].
    #[cfg(test)]
    pub fn with_transport_and_notify(
        cfg: Settings,
        transport: Box<dyn Transport>,
        notify: Notifier,
    ) -> Arc<App> {
        Arc::new(App {
            store: Store::new(cfg.state_dir.clone(), cfg.server.clone()),
            cfg,
            transport,
            log: Mutex::new(Vec::new()),
            last_error: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            signal: Condvar::new(),
            stopping: AtomicBool::new(false),
            notify,
        })
    }

    pub fn record(&self, session_id: &str, run_id: &str, kind: &str, outcome: &str) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        // The tick re-examines every open goal every few seconds, so the routine
        // "still paused" / "already continued" answer repeats. Keep the first and
        // drop the echo: the log is for a reader, not a heartbeat.
        if let Some(last) = log.last() {
            if last.session_id == session_id
                && last.run_id == run_id
                && last.kind == kind
                && last.outcome == outcome
            {
                return;
            }
        }
        log.push(LogEntry {
            at_ms: now_ms(),
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            kind: kind.to_string(),
            outcome: outcome.to_string(),
        });
        let excess = log.len().saturating_sub(MAX_LOG);
        if excess > 0 {
            log.drain(..excess);
        }
    }

    pub fn log_snapshot(&self) -> Vec<LogEntry> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_error(&self, message: impl Into<String>) {
        let message = message.into();
        if !message.is_empty() {
            eprintln!("kc-goals: {message}");
        }
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = if message.is_empty() {
            None
        } else {
            Some(message)
        };
    }

    pub fn error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Queue a run-end intent and wake the worker. Called from the hook, so it
    /// must be fast and must never block on the network.
    pub fn wake_with(&self, intent: Intent) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(intent);
        self.signal.notify_one();
    }

    /// Wake the worker without an intent (the reconciler decides what to do).
    pub fn wake(&self) {
        self.signal.notify_one();
    }

    fn take_intents(&self) -> Vec<Intent> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *pending)
    }
}

/// Start the engine thread. Returns immediately; the thread lives until
/// `App::stopping` is set.
pub fn spawn(app: Arc<App>) {
    let _ = std::thread::Builder::new()
        .name("kc-goals-engine".into())
        .spawn(move || run(app));
}

fn run(app: Arc<App>) {
    let tick = Duration::from_secs(app.cfg.tick_secs.max(2));
    let mut next_tick = Instant::now();
    loop {
        if app.stopping.load(Ordering::Relaxed) {
            return;
        }
        for intent in app.take_intents() {
            process(&app, &intent.session_id, &intent.run_id, &intent.status);
        }
        if Instant::now() >= next_tick {
            reconcile(&app);
            next_tick = Instant::now() + tick;
        }
        // Sleep until the next tick, or until a hook wakes us - whichever is
        // first. Parking on the condvar rather than polling keeps an idle plugin
        // at zero CPU.
        let pending = app.pending.lock().unwrap_or_else(|e| e.into_inner());
        if !pending.is_empty() || app.stopping.load(Ordering::Relaxed) {
            continue;
        }
        let timeout = next_tick.saturating_duration_since(Instant::now());
        let (guard, _) = app
            .signal
            .wait_timeout_while(pending, timeout, |_| {
                !app.stopping.load(Ordering::Relaxed)
            })
            .unwrap_or_else(|e| e.into_inner());
        drop(guard);
    }
}

/// Handle one run that ended. Public so the hook path, the tick and tests all go
/// through the same code.
pub fn process(app: &App, session_id: &str, run_id: &str, status: &str) -> Outcome {
    process_with(app, session_id, run_id, status, None)
}

/// The same, when the caller already has the session view (the reconciler does,
/// and fetching twice for one decision would be silly).
pub fn process_with(
    app: &App,
    session_id: &str,
    run_id: &str,
    status: &str,
    view: Option<&SessionView>,
) -> Outcome {
    let fetched;
    let view = match view {
        Some(view) => Some(view),
        None => {
            fetched = app.transport.session(session_id).ok();
            fetched.as_ref()
        }
    };
    let workspace = match resolve_workspace(app, session_id, view) {
        Ok(workspace) => workspace,
        Err(error) => {
            app.record(session_id, run_id, "error", &error);
            return Outcome::Failed(error);
        }
    };
    // The harness's own tool-call counter: `None` when it could not be read, in
    // which case the stall detector abstains instead of guessing.
    let tool_calls = match view {
        Some(view) if view.exists => Some(view.tool_calls),
        _ => None,
    };
    let file = match app.store.load(&workspace, session_id) {
        Ok(file) => file,
        Err(error) => {
            app.record(session_id, run_id, "error", &error);
            app.set_error(&error);
            return Outcome::Failed(error);
        }
    };
    let Some(goal) = file.active().cloned() else {
        let outcome = Outcome::Skipped {
            reason: "this session has no active goal".into(),
        };
        app.record(session_id, run_id, outcome.kind(), &outcome.describe());
        return outcome;
    };

    let now = now_ms();
    let plan = engine::plan(&goal, run_id, status, tool_calls, now, &app.cfg);
    let outcome = match plan {
        Plan::Skip { reason } => Outcome::Skipped { reason },
        Plan::Pause { reason } => {
            settle(
                app,
                &workspace,
                session_id,
                |document| {
                    if let Some(goal) = document.active_mut() {
                        engine::note_paused(goal, &reason, now);
                    }
                    Ok(())
                },
                run_id,
            );
            (app.notify)(
                Some(session_id),
                NotifyKind::NeedsInput,
                &format!("Goal paused: {}", crate::model::truncate(&goal.title, 60)),
                &format!("{reason}. The goal is kept - resume it with goal_resume."),
            );
            Outcome::Paused { reason }
        }
        Plan::Continue { prompt } => match deliver(app, session_id, &prompt) {
            Ok(via) => {
                settle(
                    app,
                    &workspace,
                    session_id,
                    |document| {
                        if let Some(goal) = document.active_mut() {
                            engine::note_continued(goal, run_id, status, tool_calls, now);
                        }
                        Ok(())
                    },
                    run_id,
                );
                Outcome::Continued { via }
            }
            Err(DeliverError::Gone(error)) => {
                settle(
                    app,
                    &workspace,
                    session_id,
                    |document| {
                        if let Some(goal) = document.active_mut() {
                            engine::note_blocked(
                                goal,
                                "the session no longer exists, so this goal has nowhere to run",
                                now,
                            );
                        }
                        Ok(())
                    },
                    run_id,
                );
                app.store.forget(session_id);
                (app.notify)(
                    Some(session_id),
                    NotifyKind::Error,
                    &format!("Goal orphaned: {}", crate::model::truncate(&goal.title, 60)),
                    "The session was deleted, so the goal cannot be resumed. Set a new one.",
                );
                Outcome::Failed(error)
            }
            Err(DeliverError::Other(error)) => {
                // Nothing is written: leaving `last_run_id` untouched is what
                // makes the next tick retry this run instead of losing it.
                app.set_error(&error);
                Outcome::Failed(error)
            }
        },
    };
    app.record(session_id, run_id, outcome.kind(), &outcome.describe());
    outcome
}

/// Apply one change to a session's goal document, ignoring a document that
/// changed under us into a state where the change no longer applies.
fn settle(
    app: &App,
    workspace: &str,
    session_id: &str,
    change: impl FnOnce(&mut GoalFile) -> Result<(), String>,
    run_id: &str,
) {
    if let Err(error) = app.store.update(workspace, session_id, change) {
        app.set_error(format!("could not record run {run_id}: {error}"));
    }
}

/// `send`, and on the boundary race fall back to `follow_up`, which is exactly
/// what the composer's two modes are for:
///
/// * `409` - a run is still registered, so queue the message behind it;
/// * `400` after that - the run ended between the two calls, so `send` is right
///   after all. One retry, then give up and let the tick try again.
fn deliver(app: &App, session_id: &str, text: &str) -> Result<&'static str, DeliverError> {
    let first = app
        .transport
        .post(session_id, text, PostMode::Send)
        .map_err(DeliverError::Other)?;
    if first.is_ok() {
        return Ok("send");
    }
    if first.status == 404 {
        return Err(DeliverError::Gone(format!(
            "POST /api/sessions/{session_id}/messages answered 404"
        )));
    }
    if first.status != 409 {
        return Err(DeliverError::Other(describe_post(&first)));
    }
    let queued = app
        .transport
        .post(session_id, text, PostMode::FollowUp)
        .map_err(DeliverError::Other)?;
    if queued.is_ok() {
        return Ok("follow_up");
    }
    if queued.status == 404 {
        return Err(DeliverError::Gone(format!(
            "POST /api/sessions/{session_id}/messages?mode=follow_up answered 404"
        )));
    }
    if queued.status != 400 {
        return Err(DeliverError::Other(describe_post(&queued)));
    }
    // `follow_up` with nothing running. The run is gone: send again, once.
    std::thread::sleep(Duration::from_millis(300));
    let retry = app
        .transport
        .post(session_id, text, PostMode::Send)
        .map_err(DeliverError::Other)?;
    if retry.is_ok() {
        return Ok("send_retry");
    }
    Err(DeliverError::Other(describe_post(&retry)))
}

fn describe_post(post: &Post) -> String {
    let detail = post
        .body
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| crate::model::truncate(&post.body.to_string(), 200));
    format!("HTTP {}: {detail}", post.status)
}

/// The session -> workspace hint, refreshed from the server when it is missing
/// or stale. The goal file is still the truth; this only says where to look.
fn resolve_workspace(
    app: &App,
    session_id: &str,
    view: Option<&SessionView>,
) -> Result<String, String> {
    if let Some(workspace) = app.store.sessions().get(session_id) {
        if !workspace.is_empty() {
            return Ok(workspace.clone());
        }
    }
    let fetched;
    let view = match view {
        Some(view) => view,
        None => {
            fetched = app.transport.session(session_id)?;
            &fetched
        }
    };
    if !view.exists {
        return Err(format!("session `{session_id}` does not exist"));
    }
    if view.workspace.is_empty() {
        return Err(format!(
            "the server did not report a workspace for session `{session_id}`"
        ));
    }
    app.store.remember(session_id, &view.workspace);
    Ok(view.workspace.clone())
}

/// The workspace a session actually lives in, best effort.
///
/// The index answers without a round trip; when it has no entry (a fresh plugin,
/// or a panel read for a session this process has never served a tool call for)
/// the server is asked. `None` means "could not find out", which callers treat as
/// "keep what you were given".
pub fn session_workspace(app: &App, session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    if let Some(workspace) = app.store.sessions().get(session_id) {
        if !workspace.is_empty() {
            return Some(workspace.clone());
        }
    }
    match app.transport.session(session_id) {
        Ok(view) if view.exists && !view.workspace.is_empty() => {
            app.store.remember(session_id, &view.workspace);
            Some(view.workspace)
        }
        _ => None,
    }
}

/// One reconciliation pass: for every session the plugin has ever seen, if it has
/// an open goal and its newest run has not been handled, handle it.
///
/// This is the whole recovery story. A dropped hook, a plugin restart, a server
/// restart, a POST that failed - all of them look like "a goal is open and a run
/// ended that nobody has continued", which is exactly what this finds.
pub fn reconcile(app: &App) {
    for (session_id, workspace) in app.store.sessions() {
        if app.stopping.load(Ordering::Relaxed) {
            return;
        }
        let file = match app.store.load(&workspace, &session_id) {
            Ok(file) => file,
            Err(error) => {
                app.set_error(&error);
                continue;
            }
        };
        let Some(goal) = file.active() else {
            continue;
        };
        let view = match app.transport.session(&session_id) {
            Ok(view) => view,
            Err(error) => {
                app.set_error(format!("{session_id}: {error}"));
                continue;
            }
        };
        if !view.exists {
            if let Some(goal) = file.active() {
                let reason = "the session no longer exists, so this goal has nowhere to run";
                let now = now_ms();
                let _ = app.store.update(&workspace, &session_id, |document| {
                    if let Some(goal) = document.active_mut() {
                        engine::note_blocked(goal, reason, now);
                    }
                    Ok(())
                });
                (app.notify)(
                    Some(&session_id),
                    NotifyKind::Error,
                    &format!("Goal orphaned: {}", crate::model::truncate(&goal.title, 60)),
                    "The session was deleted, so the goal cannot be resumed.",
                );
                app.record(&session_id, "", "pause", reason);
                app.store.forget(&session_id);
            }
            continue;
        }
        // A session that moved workspace would otherwise be read from a file
        // nobody updates any more.
        if !view.workspace.is_empty() && view.workspace != workspace {
            app.store.remember(&session_id, &view.workspace);
            continue;
        }
        if view.running {
            continue;
        }
        let Some(run_id) = view.last_run_id.clone() else {
            continue;
        };
        if goal.progress.last_run_id.as_deref() == Some(run_id.as_str()) {
            continue;
        }
        let status = view.last_run_status.clone().unwrap_or_else(|| "completed".into());
        process_with(app, &session_id, &run_id, &status, Some(&view));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Budget, Criterion, Source};
    use std::sync::Mutex as StdMutex;

    /// A scripted server. Enough to prove the flow, and nothing else.
    #[derive(Default)]
    struct Fake {
        sessions: StdMutex<std::collections::BTreeMap<String, SessionView>>,
        /// When set, `send` answers 409 and `follow_up` answers 400 - the
        /// boundary race, in the order it actually happens.
        busy_then_idle: StdMutex<bool>,
        /// Every message the engine posted: (session, text, mode).
        posted: StdMutex<Vec<(String, String, PostMode)>>,
        /// Force a status code on the next `send`.
        fail_send_with: StdMutex<Option<u16>>,
        fail_follow_up_with: StdMutex<Option<u16>>,
    }

    impl Fake {
        fn set(&self, session: &str, view: SessionView) {
            self.sessions
                .lock()
                .unwrap()
                .insert(session.to_string(), view);
        }

        fn posted(&self) -> Vec<(String, String, PostMode)> {
            self.posted.lock().unwrap().clone()
        }
    }

    impl Transport for Fake {
        fn session(&self, session_id: &str) -> Result<SessionView, String> {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .unwrap_or_default())
        }

        fn post(&self, session_id: &str, text: &str, mode: PostMode) -> Result<Post, String> {
            let forced = match mode {
                PostMode::Send => *self.fail_send_with.lock().unwrap(),
                PostMode::FollowUp => *self.fail_follow_up_with.lock().unwrap(),
            };
            if let Some(status) = forced {
                // A forced 400 on `follow_up` is the real server's "nothing is
                // running any more": the run ended between our two calls.
                if mode == PostMode::FollowUp && status == 400 {
                    *self.busy_then_idle.lock().unwrap() = false;
                }
                return Ok(Post {
                    status,
                    body: json!({"error": format!("forced {status}")}),
                });
            }
            if mode == PostMode::Send && *self.busy_then_idle.lock().unwrap() {
                return Ok(Post {
                    status: 409,
                    body: json!({"error": "a run is already active for this session"}),
                });
            }
            self.posted
                .lock()
                .unwrap()
                .push((session_id.to_string(), text.to_string(), mode));
            Ok(Post {
                status: 202,
                body: json!({"mode": "send", "run_id": "r_next"}),
            })
        }
    }

    struct Harness {
        app: Arc<App>,
        fake: Arc<Fake>,
        root: std::path::PathBuf,
        workspace: String,
        notices: Arc<StdMutex<Vec<(String, String)>>>,
    }

    fn harness(workspace_override: Option<&str>) -> Harness {
        let root = crate::model::temp_dir("kc-goals-worker");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let cfg = Settings {
            state_dir: root.join("state").to_string_lossy().to_string(),
            min_gap_secs: 0,
            ..Settings::default()
        };
        let fake = Arc::new(Fake::default());
        let transport: Box<dyn Transport> = Box::new(FakeProxy(Arc::clone(&fake)));
        // The tests assert on what a human would have been told, so the notifier
        // records instead of writing a protocol frame to stdout.
        let notices = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&notices);
        let app = App::with_transport_and_notify(
            cfg,
            transport,
            Box::new(move |_, _kind, title, body| {
                sink.lock().unwrap().push((title.to_string(), body.to_string()));
            }),
        );
        let workspace = workspace_override
            .map(str::to_string)
            .unwrap_or_else(|| workspace.to_string_lossy().to_string());
        let h = Harness {
            app,
            fake,
            root,
            workspace,
            notices,
        };
        h.app.store.remember("s_1", &h.workspace);
        h
    }

    /// `Arc<Fake>` needs to be a `Transport`; this is the `Arc` adaptation.
    struct FakeProxy(Arc<Fake>);
    impl Transport for FakeProxy {
        fn session(&self, id: &str) -> Result<SessionView, String> {
            self.0.session(id)
        }
        fn post(&self, id: &str, text: &str, mode: PostMode) -> Result<Post, String> {
            self.0.post(id, text, mode)
        }
    }

    impl Harness {
        fn session_of(&self) -> &str {
            "s_1"
        }

        fn seed_goal(&self) {
            let workspace = self.workspace.clone();
            self.app
                .store
                .update(&workspace, self.session_of(), |file| {
                    file.goals.push(crate::model::Goal::new(
                        "g1",
                        "Ship the thing",
                        "Ship the thing end to end",
                        vec![Criterion::new("c1", "builds"), Criterion::new("c2", "tested")],
                        Budget::default(),
                        Source::User,
                        now_ms(),
                    ));
                    file.refresh_active();
                    Ok(())
                })
                .unwrap();
        }

        /// The session's newest goal, whatever its status - the tests assert on
        /// closed goals too.
        fn goal(&self) -> crate::model::Goal {
            self.app
                .store
                .load(&self.workspace, self.session_of())
                .unwrap()
                .newest_first()
                .first()
                .map(|goal| (*goal).clone())
                .expect("a goal")
        }

        fn notices(&self) -> Vec<(String, String)> {
            self.notices.lock().unwrap().clone()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_finished_run_with_an_open_goal_posts_a_continuation() {
        let h = harness(None);
        h.seed_goal();
        let outcome = process(&h.app, h.session_of(), "r1", "completed");
        assert_eq!(outcome, Outcome::Continued { via: "send" });
        let posted = h.fake.posted();
        assert_eq!(posted.len(), 1);
        assert!(posted[0].1.contains("[kc-goals] Continuing goal g1"));
        assert!(posted[0].1.contains("c1 — builds"));
        let goal = h.goal();
        assert_eq!(goal.progress.continuations, 1);
        assert_eq!(goal.progress.last_run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn the_same_run_is_not_continued_twice() {
        let h = harness(None);
        h.seed_goal();
        process(&h.app, h.session_of(), "r1", "completed");
        let second = process(&h.app, h.session_of(), "r1", "completed");
        assert!(matches!(second, Outcome::Skipped { .. }), "{second:?}");
        assert_eq!(h.fake.posted().len(), 1);
        assert_eq!(h.goal().progress.continuations, 1);
    }

    #[test]
    fn a_cancelled_run_pauses_the_goal_and_posts_nothing() {
        let h = harness(None);
        h.seed_goal();
        let outcome = process(&h.app, h.session_of(), "r1", "cancelled");
        assert!(matches!(outcome, Outcome::Paused { .. }), "{outcome:?}");
        assert!(h.fake.posted().is_empty());
        assert_eq!(h.goal().status, crate::model::Status::Paused);
        assert_eq!(h.notices().len(), 1);
        assert!(h.notices()[0].1.contains("cancelled"));
    }

    #[test]
    fn a_failed_post_does_not_mark_the_run_handled_so_it_is_retried() {
        let h = harness(None);
        h.seed_goal();
        *h.fake.fail_send_with.lock().unwrap() = Some(500);
        let outcome = process(&h.app, h.session_of(), "r1", "completed");
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
        assert!(h.goal().progress.last_run_id.is_none());
        assert_eq!(h.goal().progress.continuations, 0);
        // The server recovers: the retry goes through.
        *h.fake.fail_send_with.lock().unwrap() = None;
        let outcome = process(&h.app, h.session_of(), "r1", "completed");
        assert_eq!(outcome, Outcome::Continued { via: "send" });
        assert_eq!(h.fake.posted().len(), 1);
    }

    #[test]
    fn a_busy_session_falls_back_to_a_follow_up() {
        let h = harness(None);
        h.seed_goal();
        *h.fake.busy_then_idle.lock().unwrap() = true;
        let outcome = process(&h.app, h.session_of(), "r1", "completed");
        assert_eq!(outcome, Outcome::Continued { via: "follow_up" });
        let posted = h.fake.posted();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].2, PostMode::FollowUp);
    }

    #[test]
    fn a_409_then_a_400_retries_with_a_plain_send() {
        let h = harness(None);
        h.seed_goal();
        // 409 on send, then 400 on follow_up: the run ended in between.
        *h.fake.busy_then_idle.lock().unwrap() = true;
        *h.fake.fail_follow_up_with.lock().unwrap() = Some(400);
        let outcome = process(&h.app, h.session_of(), "r1", "completed");
        assert_eq!(outcome, Outcome::Continued { via: "send_retry" });
        let posted = h.fake.posted();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].2, PostMode::Send);
    }

    #[test]
    fn a_deleted_session_blocks_the_goal_instead_of_retrying_forever() {
        let h = harness(None);
        h.seed_goal();
        *h.fake.fail_send_with.lock().unwrap() = Some(404);
        let outcome = process(&h.app, h.session_of(), "r1", "completed");
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
        assert_eq!(h.goal().status, crate::model::Status::Blocked);
        assert!(!h
            .app
            .store
            .sessions()
            .contains_key(h.session_of()));
        assert_eq!(h.notices().len(), 1);
    }

    #[test]
    fn a_session_without_a_goal_is_left_alone() {
        let h = harness(None);
        let outcome = process(&h.app, h.session_of(), "r1", "completed");
        assert!(matches!(outcome, Outcome::Skipped { .. }), "{outcome:?}");
        assert!(h.fake.posted().is_empty());
    }

    #[test]
    fn reconcile_continues_a_goal_whose_run_ended_while_the_plugin_was_down() {
        let h = harness(None);
        h.seed_goal();
        h.fake.set(
            h.session_of(),
            SessionView {
                exists: true,
                running: false,
                workspace: h.workspace.clone(),
                last_run_id: Some("r7".into()),
                last_run_status: Some("completed".into()),
                tool_calls: 7,
            },
        );
        reconcile(&h.app);
        assert_eq!(h.fake.posted().len(), 1);
        assert_eq!(h.goal().progress.last_run_id.as_deref(), Some("r7"));
    }

    #[test]
    fn reconcile_ignores_a_running_session() {
        let h = harness(None);
        h.seed_goal();
        h.fake.set(
            h.session_of(),
            SessionView {
                exists: true,
                running: true,
                workspace: h.workspace.clone(),
                last_run_id: Some("r7".into()),
                last_run_status: Some("running".into()),
                tool_calls: 7,
            },
        );
        reconcile(&h.app);
        assert!(h.fake.posted().is_empty());
    }

    #[test]
    fn reconcile_does_not_repost_a_run_it_already_handled() {
        let h = harness(None);
        h.seed_goal();
        h.fake.set(
            h.session_of(),
            SessionView {
                exists: true,
                running: false,
                workspace: h.workspace.clone(),
                last_run_id: Some("r7".into()),
                last_run_status: Some("completed".into()),
                tool_calls: 7,
            },
        );
        reconcile(&h.app);
        reconcile(&h.app);
        assert_eq!(h.fake.posted().len(), 1);
    }

    #[test]
    fn reconcile_blocks_a_goal_whose_session_is_gone() {
        let h = harness(None);
        h.seed_goal();
        h.fake.set(
            h.session_of(),
            SessionView {
                exists: false,
                ..SessionView::default()
            },
        );
        reconcile(&h.app);
        assert_eq!(h.goal().status, crate::model::Status::Blocked);
        assert_eq!(h.notices().len(), 1);
        assert!(!h.app.store.sessions().contains_key(h.session_of()));
    }

    #[test]
    fn reconcile_follows_a_session_that_moved_workspace() {
        let h = harness(None);
        h.seed_goal();
        h.fake.set(
            h.session_of(),
            SessionView {
                exists: true,
                running: false,
                workspace: "/somewhere/else".into(),
                last_run_id: Some("r7".into()),
                last_run_status: Some("completed".into()),
                tool_calls: 7,
            },
        );
        reconcile(&h.app);
        assert_eq!(
            h.app
                .store
                .sessions()
                .get(h.session_of())
                .map(String::as_str),
            Some("/somewhere/else")
        );
        assert!(h.fake.posted().is_empty(), "a moved session is re-read, not continued blind");
    }

    #[test]
    fn a_409_conflict_is_queued_and_counted_once() {
        let h = harness(None);
        h.seed_goal();
        *h.fake.busy_then_idle.lock().unwrap() = true;
        process(&h.app, h.session_of(), "r1", "completed");
        let goal = h.goal();
        assert_eq!(goal.progress.continuations, 1);
        assert_eq!(goal.progress.last_run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn a_stalled_run_is_paused_once_the_stall_budget_is_reached() {
        let h = harness(None);
        h.seed_goal();
        let workspace = h.workspace.clone();
        h.app
            .store
            .update(&workspace, h.session_of(), |file| {
                let goal = file.active_mut().unwrap();
                goal.budget.max_stalled_resumes = 2;
                Ok(())
            })
            .unwrap();
        // The harness reports the same tool-call total every time: the runs make
        // no call, and nothing is recorded with goal_progress.
        h.fake.set(
            h.session_of(),
            SessionView {
                exists: true,
                running: false,
                workspace: workspace.clone(),
                last_run_id: Some("r1".into()),
                last_run_status: Some("completed".into()),
                tool_calls: 5,
            },
        );
        // r1 is the run that created the goal, so it is progress by definition.
        assert_eq!(
            process(&h.app, h.session_of(), "r1", "completed"),
            Outcome::Continued { via: "send" }
        );
        // r2 did nothing: one strike, still under the budget of 2.
        assert_eq!(
            process(&h.app, h.session_of(), "r2", "completed"),
            Outcome::Continued { via: "send" }
        );
        // r3 did nothing again: 2 strikes, so the goal pauses instead of looping.
        match process(&h.app, h.session_of(), "r3", "completed") {
            Outcome::Paused { reason } => assert!(reason.contains("did nothing"), "{reason}"),
            other => panic!("expected a pause, got {other:?}"),
        }
        assert_eq!(h.goal().status, crate::model::Status::Paused);
        assert_eq!(h.fake.posted().len(), 2, "two continuations, then a pause");
    }

    #[test]
    fn a_run_that_used_tools_resets_the_stall_counter() {
        let h = harness(None);
        h.seed_goal();
        let workspace = h.workspace.clone();
        h.app
            .store
            .update(&workspace, h.session_of(), |file| {
                let goal = file.active_mut().unwrap();
                goal.budget.max_stalled_resumes = 2;
                goal.progress.pending_progress = 0;
                goal.progress.stalled_streak = 1;
                Ok(())
            })
            .unwrap();
        h.fake.set(
            h.session_of(),
            SessionView {
                exists: true,
                running: false,
                workspace: workspace.clone(),
                last_run_id: Some("r1".into()),
                last_run_status: Some("completed".into()),
                // More calls than the baseline: this run did something.
                tool_calls: 9,
            },
        );
        let outcome = process(&h.app, h.session_of(), "r1", "completed");
        assert_eq!(outcome, Outcome::Continued { via: "send" });
        assert_eq!(h.goal().progress.stalled_streak, 0);
    }

    #[test]
    fn the_log_records_what_happened_for_the_panel() {
        let h = harness(None);
        h.seed_goal();
        process(&h.app, h.session_of(), "r1", "completed");
        let log = h.app.log_snapshot();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].kind, "continue");
        assert_eq!(log[0].run_id, "r1");
        assert_eq!(log[0].outcome, "posted (send)");
    }

    #[test]
    fn a_new_continuation_after_a_recorded_progress_keeps_the_streak_clean() {
        let h = harness(None);
        h.seed_goal();
        process(&h.app, h.session_of(), "r1", "completed");
        let workspace = h.workspace.clone();
        h.app
            .store
            .update(&workspace, h.session_of(), |file| {
                file.active_mut().unwrap().record_progress("wrote the worker", Some("test it"), now_ms())
            })
            .unwrap();
        process(&h.app, h.session_of(), "r2", "completed");
        let goal = h.goal();
        assert_eq!(goal.progress.stalled_streak, 0);
        assert_eq!(goal.progress.continuations, 2);
    }
}
