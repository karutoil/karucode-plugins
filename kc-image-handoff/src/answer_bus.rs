//! A process-wide event-stream reader: one connection to the karucode server,
//! one background thread, answers buffered per session.
//!
//! Why a single long-lived reader: a karucode vision run can outlive the
//! host's 60-second tool-call budget. `see` first waits inline; if the run is
//! still going it returns `pending` plus the session id, and the *caller*
//! re-asks with that id. The event frames for the run keep arriving here even
//! after the temporary session retires itself - they were captured live - so
//! the second call gets the answer that a fresh connection could no longer
//! see (the session row, transcript and replay are gone by then).

use crate::server::{Endpoint, HttpError, SseFrame};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// What one session did, as far as the reader saw.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionOutcome {
    pub answer: String,
    pub replaced: Option<String>,
    pub status: Option<String>,
    pub error: Option<String>,
    pub finished_at: Option<Instant>,
}

impl SessionOutcome {
    /// The final answer: the persisted replacement when one arrived, else the
    /// assembled deltas.
    pub fn answer(&self) -> String {
        match &self.replaced {
            Some(replaced) => replaced.clone(),
            None => self.answer.clone(),
        }
    }
}

struct Inner {
    outcomes: Mutex<HashMap<String, SessionOutcome>>,
    signal: Condvar,
}

struct Bus {
    inner: Inner,
    endpoint: Endpoint,
    token: String,
    /// Slightly stale view for lookups that must not block the writer.
    snapshot: RwLock<HashMap<String, SessionOutcome>>,
}

/// The bus is keyed by the server it reads from; the token is whatever the
/// first caller had. (One plugin process talks to exactly one server.)
fn bus(endpoint: Endpoint, token: &str) -> &'static Arc<Bus> {
    static BUS: OnceLock<Arc<Bus>> = OnceLock::new();
    BUS.get_or_init(|| {
        let bus = Arc::new(Bus {
            inner: Inner {
                outcomes: Mutex::new(HashMap::new()),
                signal: Condvar::new(),
            },
            endpoint,
            token: token.to_string(),
            snapshot: RwLock::new(HashMap::new()),
        });
        spawn_reader(bus.clone());
        bus
    })
}

/// Start the reading thread. Reconnects forever with a small backoff; a
/// dropped connection costs one retry, not an answer. An idle stream is fine
/// (a quiet server is normal between calls) and is treated as a reconnect.
fn spawn_reader(bus: Arc<Bus>) {
    std::thread::Builder::new()
        .name("image-handoff-events".into())
        .spawn(move || loop {
            if let Ok(mut stream) = crate::server::open_event_stream(&bus.endpoint, &bus.token) {
                while let Ok(Some(frame)) = read_quietly(&mut stream) {
                    record(&bus, &frame);
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        })
        .expect("spawn the event reader thread");
}

/// One frame with the idle timeout flattened to `None`: an idle stream is not
/// an error for the reader, it just means nothing happened yet.
fn read_quietly(stream: &mut crate::server::EventStream) -> Result<Option<SseFrame>, HttpError> {
    match stream.read_frame() {
        Ok(frame) => Ok(frame),
        Err(HttpError::Io(message)) if message.contains("idle") => Ok(None),
        other => other,
    }
}

fn record(bus: &Bus, frame: &SseFrame) {
    let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
        return;
    };
    let Some(session_id) = value.get("session_id").and_then(Value::as_str) else {
        return;
    };
    if session_id.is_empty() {
        return;
    }
    let mut outcomes = bus.inner.outcomes.lock().unwrap_or_else(|p| p.into_inner());
    let entry = outcomes.entry(session_id.to_string()).or_default();
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text_delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                entry.answer.push_str(delta);
            }
        }
        "text_replaced" => {
            if let Some(content) = value.get("content").and_then(Value::as_str) {
                entry.replaced = Some(content.to_string());
            }
        }
        "run_finished" => {
            entry.status = value
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string);
            entry.error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string);
            entry.finished_at = Some(Instant::now());
        }
        _ => {}
    }
    // Mirror into the lock-free-ish snapshot copy for non-blocking lookups.
    let copy = outcomes.clone();
    bus.inner.signal.notify_all();
    drop(outcomes);
    *bus.snapshot.write().unwrap_or_else(|p| p.into_inner()) = copy;
}

/// Wait (bounded) for `session_id` to reach a finished state.
pub fn wait_for_outcome(
    endpoint: &Endpoint,
    token: &str,
    session_id: &str,
    deadline: Instant,
) -> Option<SessionOutcome> {
    let bus = bus(endpoint.clone(), token);
    let mut outcomes = bus.inner.outcomes.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        if let Some(outcome) = outcomes.get(session_id) {
            if outcome.status.is_some() {
                return Some(outcome.clone());
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let (guard, _timeout) = bus
            .inner
            .signal
            .wait_timeout(outcomes, deadline - now)
            .unwrap_or_else(|p| p.into_inner());
        outcomes = guard;
    }
}

/// A finished outcome, if the reader already saw it. Never blocks.
pub fn finished_outcome(
    endpoint: &Endpoint,
    token: &str,
    session_id: &str,
) -> Option<SessionOutcome> {
    let bus = bus(endpoint.clone(), token);
    let outcomes = bus.snapshot.read().unwrap_or_else(|p| p.into_inner());
    outcomes
        .get(session_id)
        .filter(|o| o.status.is_some())
        .cloned()
}

/// Whether the reader has ever seen frames for this session.
pub fn known_session(endpoint: &Endpoint, token: &str, session_id: &str) -> bool {
    let bus = bus(endpoint.clone(), token);
    let outcomes = bus.snapshot.read().unwrap_or_else(|p| p.into_inner());
    outcomes.contains_key(session_id)
}

/// Forget a session's buffered outcome (called when it is collected).
pub fn forget(endpoint: &Endpoint, token: &str, session_id: &str) {
    let bus = bus(endpoint.clone(), token);
    bus.inner
        .outcomes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(session_id);
    bus.snapshot
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .remove(session_id);
}
