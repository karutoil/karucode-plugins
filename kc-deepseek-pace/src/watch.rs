//! The watcher: one background thread that reads `/api/events`, feeds the
//! detector, and performs what it decides.
//!
//! ## Why a thread and not a hook
//!
//! The hooks a plugin can implement are `before_prompt`, `on_run_end`,
//! `on_tool_result` and `on_notify`, and none of them carries reasoning. That is
//! not an oversight in the host - `reasoning_content` is streamed to clients and
//! discarded, never persisted, so there is no row and no hook payload to read it
//! from. The *only* place reasoning is observable is the live event stream, so the
//! watcher reads the same SSE stream the web UI reads. It is a normal client of a
//! normal route, needs no privilege, and sees exactly what a browser sees.
//!
//! Being a separate thread also keeps the plugin's stdin loop responsive: a
//! `hooks/run` request must be answered inside the host's 5 s budget even while
//! the watcher is blocked on a socket read.
//!
//! ## Idle ticking
//!
//! The detector is told the time on every event, but a stalled turn that stops
//! emitting deltas is the interesting case: the stream can go quiet for a minute
//! while the model sits at the output cap. So the read timeout doubles as the tick
//! interval - [`EventStream::next_event`] returns `None` when it idles out, and the
//! watcher then ticks the detector for every session it knows about before
//! reconnecting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::api::{self, Action};
use crate::config::Config;
use crate::detect::{Decision, DecisionKind, Detector};
use crate::http::{Endpoint, EventStream};
use crate::sdk::{notify_with, NotifyFrame, NotifyKind};

/// Everything the panel and the tools read.
#[derive(Debug, Default)]
pub struct Shared {
    pub config: Config,
    /// The live detector, behind a lock because the watcher writes and the panel
    /// reads.
    pub detector: Mutex<Detector>,
    /// Recent decisions, newest last, bounded.
    pub log: Mutex<Vec<LogEntry>>,
    /// Whether the event stream is currently connected.
    pub connected: AtomicBool,
    /// The last connection error, if any, for the panel.
    pub last_error: Mutex<Option<String>>,
    /// Set by the plugin's `shutdown` to stop the thread.
    pub stopping: AtomicBool,
}

impl Shared {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            ..Default::default()
        }
    }

    fn record(&self, decision: &Decision, outcome: &str) {
        let mut log = self.log.lock().expect("log lock");
        log.push(LogEntry {
            at_ms: now_ms(),
            session_id: decision.session_id.clone(),
            run_id: decision.run_id.clone(),
            kind: decision.kind.as_str().to_string(),
            reason: decision.reason.clone(),
            outcome: outcome.to_string(),
        });
        // Bounded: a dashboard keeps recent history, not a session's whole life.
        let excess = log.len().saturating_sub(MAX_LOG_ENTRIES);
        if excess > 0 {
            log.drain(..excess);
        }
    }
}

/// One line of history for the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub at_ms: i64,
    pub session_id: String,
    pub run_id: String,
    pub kind: String,
    pub reason: String,
    pub outcome: String,
}

pub const MAX_LOG_ENTRIES: usize = 100;

/// Milliseconds since the epoch, matching the server's `ts_ms`.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Start the watcher. Returns immediately; the thread lives until shutdown.
pub fn spawn(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("kc-deepseek-pace-watch".into())
        .spawn(move || run(shared))
        .ok();
}

fn run(shared: Arc<Shared>) {
    let cfg = shared.config.clone();
    let endpoint = match Endpoint::parse(&cfg.server) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            set_error(&shared, e.to_string());
            return;
        }
    };

    // The idle read timeout is also the tick interval. 15 s is under the server's
    // own 45 s "treat the connection as dead" guidance and coarse enough that the
    // ladder's minutes are resolved accurately.
    let idle = Duration::from_secs(15);

    while !shared.stopping.load(Ordering::Relaxed) {
        match EventStream::open(&endpoint, &cfg.token, idle) {
            Ok(mut stream) => {
                shared.connected.store(true, Ordering::Relaxed);
                set_error(&shared, String::new());
                while !shared.stopping.load(Ordering::Relaxed) {
                    match stream.next_event() {
                        Some(payload) => {
                            if let Some(value) = parse(&payload) {
                                handle(&shared, &endpoint, &value);
                            }
                        }
                        None => break,
                    }
                }
                shared.connected.store(false, Ordering::Relaxed);
                // Idle out: tick every session we are watching, so a turn that
                // stopped sending deltas is still judged.
                tick_all(&shared, idle);
            }
            Err(e) => {
                shared.connected.store(false, Ordering::Relaxed);
                set_error(&shared, e.to_string());
            }
        }
        if shared.stopping.load(Ordering::Relaxed) {
            return;
        }
        sleep(cfg.reconnect_secs, &shared);
    }
}

fn set_error(shared: &Arc<Shared>, message: String) {
    if let Ok(mut slot) = shared.last_error.lock() {
        *slot = if message.is_empty() {
            None
        } else {
            Some(message)
        };
    }
}

fn sleep(secs: u64, shared: &Arc<Shared>) {
    // Woken in short slices so a shutdown does not wait out the reconnect delay.
    let deadline = std::time::Instant::now() + Duration::from_secs(secs.max(1));
    while std::time::Instant::now() < deadline {
        if shared.stopping.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn parse(payload: &str) -> Option<Value> {
    serde_json::from_str(payload).ok()
}

/// Feed one event to the detector, then act on whatever it decided.
fn handle(shared: &Arc<Shared>, endpoint: &Endpoint, event: &Value) {
    let session_id = event
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let run_id = event.get("run_id").and_then(Value::as_str).unwrap_or("");
    let agent_id = event
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("main");
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    if session_id.is_empty() {
        return;
    }
    let at_ms = event
        .get("ts_ms")
        .and_then(Value::as_i64)
        .unwrap_or_else(now_ms);
    let cfg = &shared.config;

    let mut detector = match shared.detector.lock() {
        Ok(detector) => detector,
        Err(_) => return,
    };

    let mut decisions: Vec<Decision> = Vec::new();
    match kind {
        "run_started" => {
            detector.run_started(session_id, run_id, at_ms);
        }
        "iteration_started" => {
            let iteration = event.get("iteration").and_then(Value::as_u64).unwrap_or(1) as u32;
            decisions
                .extend(detector.iteration_started(session_id, run_id, agent_id, iteration, at_ms));
        }
        "reasoning_delta" => {
            let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
            detector.reasoning(session_id, run_id, agent_id, delta, at_ms);
            decisions.extend(detector.tick(session_id, at_ms, cfg));
        }
        "text_delta" => {
            let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
            detector.text(session_id, run_id, agent_id, delta, at_ms);
        }
        "tool_call_started" => {
            detector.tool_call(session_id, run_id, agent_id, at_ms);
        }
        "run_finished" => {
            let status = event.get("status").and_then(Value::as_str).unwrap_or("");
            decisions.extend(detector.run_finished(session_id, run_id, status, cfg));
        }
        _ => {}
    }
    drop(detector);

    for decision in decisions {
        act(shared, endpoint, decision);
    }
}

/// Tick every session the detector knows about, after the stream idled out.
fn tick_all(shared: &Arc<Shared>, idle: Duration) {
    let cfg = &shared.config;
    let at_ms = now_ms();
    let sessions: Vec<String> = {
        let detector = match shared.detector.lock() {
            Ok(detector) => detector,
            Err(_) => return,
        };
        detector.sessions()
    };
    let mut decisions = Vec::new();
    if let Ok(mut detector) = shared.detector.lock() {
        for session in sessions {
            decisions.extend(detector.tick(&session, at_ms, cfg));
        }
    }
    for decision in decisions {
        act_lazy(shared, decision);
    }
    let _ = idle;
}

/// Perform a decision, but only when a server endpoint is reachable.
///
/// Split from [`act`] because a tick after an idle-out has no endpoint in hand
/// (the stream is gone) and a notification needs none.
fn act_lazy(shared: &Arc<Shared>, decision: Decision) {
    let endpoint = Endpoint::parse(&shared.config.server);
    match endpoint {
        Ok(endpoint) => act(shared, &endpoint, decision),
        Err(e) => shared.record(&decision, &format!("skipped: {e}")),
    }
}

fn act(shared: &Arc<Shared>, endpoint: &Endpoint, decision: Decision) {
    let cfg = &shared.config;

    // A model gate on the interventions only: the prompt block is not gated (see
    // `pace.rs`), and a verdict about an empty run is worth reporting whatever
    // the model was.
    if is_intervention(&decision.kind) {
        let model = if cfg.models.is_empty() {
            None
        } else {
            api::session_model(endpoint, &cfg.token, &decision.session_id)
        };
        if !covered(cfg, model.as_deref()) {
            shared.record(
                &decision,
                &format!(
                    "skipped: `{}` is not covered by the `models` filter",
                    model.as_deref().unwrap_or("the session's model")
                ),
            );
            return;
        }
    }

    let mut actions = api::plan(std::slice::from_ref(&decision), cfg);
    if let Some(retry) = api::empty_run_retry(&decision, cfg) {
        actions.push(retry);
    }

    for action in actions {
        match &action {
            Action::Notify {
                kind,
                title,
                body,
                session_id,
            } => {
                let frame = NotifyFrame {
                    kind: *kind,
                    title: title.clone(),
                    body: body.clone(),
                    session_id: Some(session_id.clone()),
                    sound: None,
                    // A stall report is worth keeping on screen until dismissed;
                    // a drift note is not.
                    sticky: matches!(decision.kind, DecisionKind::EmptyRun),
                };
                match notify_with(frame) {
                    Ok(()) => shared.record(&decision, "notified"),
                    Err(e) => shared.record(&decision, &format!("notify failed: {e}")),
                }
            }
            other => match api::execute(other, endpoint, &cfg.token) {
                Ok(outcome) => shared.record(&decision, &outcome),
                Err(e) => {
                    shared.record(&decision, &format!("failed: {e}"));
                    // Say it out loud too: a silent failure of the layer whose
                    // whole job is not being silent is the worst outcome here.
                    let _ = notify_with(NotifyFrame {
                        kind: NotifyKind::Error,
                        title: "kc-deepseek-pace could not act".into(),
                        body: e,
                        session_id: Some(decision.session_id.clone()),
                        sound: None,
                        sticky: false,
                    });
                }
            },
        }
    }
}

fn is_intervention(kind: &DecisionKind) -> bool {
    matches!(kind, DecisionKind::Steer | DecisionKind::Cancel)
}

/// Whether an intervention may proceed, given what the session lookup returned.
///
/// Pure, so the fail-open rule is testable without a server. `None` (the lookup
/// failed, or the route is unavailable) counts as covered: not being able to read
/// the model is not evidence that the operator's filter excludes it, and a
/// transient 502 must not silently disarm the ladder.
fn covered(cfg: &Config, model: Option<&str>) -> bool {
    if cfg.models.is_empty() {
        return true;
    }
    match model {
        Some(model) => cfg.covers_model(model),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_are_handled_and_a_stall_becomes_a_logged_notification() {
        let cfg = Config::test_with(|c| {
            c.dry_run = true; // never touch the network in a unit test
            c.warn = crate::config::Rule {
                after_secs: 1,
                after_chars: 10,
            };
            c.steer = crate::config::Rule::OFF;
            c.cancel = crate::config::Rule::OFF;
        });
        let shared = Arc::new(Shared::new(cfg));
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();

        handle(
            &shared,
            &endpoint,
            &json!({"type":"run_started","session_id":"s","run_id":"r","ts_ms":0}),
        );
        handle(
            &shared,
            &endpoint,
            &json!({"type":"iteration_started","session_id":"s","run_id":"r",
                    "iteration":1,"ts_ms":0}),
        );
        for i in 0..3 {
            handle(
                &shared,
                &endpoint,
                &json!({"type":"reasoning_delta","session_id":"s","run_id":"r",
                        "delta":"let me write the file","ts_ms":1000 * (i + 1)}),
            );
        }
        let log = shared.log.lock().unwrap();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].kind, "warn");
        assert_eq!(log[0].outcome, "notified");
        assert!(
            log[0].reason.contains("action announcement"),
            "{:?}",
            log[0]
        );
    }

    #[test]
    fn an_event_with_no_session_is_ignored_rather_than_panicking() {
        let shared = Arc::new(Shared::new(Config::default()));
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        handle(&shared, &endpoint, &json!({"type":"reasoning_delta"}));
        handle(
            &shared,
            &endpoint,
            &json!({"type":"unknown_type","session_id":"s"}),
        );
        assert!(shared.log.lock().unwrap().is_empty());
    }

    #[test]
    fn the_log_is_bounded() {
        let shared = Arc::new(Shared::new(Config::default()));
        for i in 0..(MAX_LOG_ENTRIES + 20) {
            shared.record(
                &Decision {
                    session_id: "s".into(),
                    run_id: "r".into(),
                    agent_id: "main".into(),
                    kind: DecisionKind::Warn,
                    reason: format!("{i}"),
                },
                "notified",
            );
        }
        let log = shared.log.lock().unwrap();
        assert_eq!(log.len(), MAX_LOG_ENTRIES);
        // The newest survive, not the oldest.
        assert_eq!(
            log.last().unwrap().reason,
            (MAX_LOG_ENTRIES + 19).to_string()
        );
    }

    #[test]
    fn an_unreachable_server_is_recorded_as_an_error_not_a_crash() {
        let cfg = Config::test_with(|c| {
            c.steer = crate::config::Rule {
                after_secs: 1,
                after_chars: 1,
            };
            c.warn = crate::config::Rule::OFF;
            c.cancel = crate::config::Rule::OFF;
            c.models = Vec::new(); // no model gate: no session lookup
        });
        let shared = Arc::new(Shared::new(cfg));
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        handle(
            &shared,
            &endpoint,
            &json!({"type":"iteration_started","session_id":"s","run_id":"r",
                    "iteration":1,"ts_ms":0}),
        );
        handle(
            &shared,
            &endpoint,
            &json!({"type":"reasoning_delta","session_id":"s","run_id":"r",
                    "delta":"xxxxx","ts_ms":5000}),
        );
        let log = shared.log.lock().unwrap();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].kind, "steer");
        assert!(log[0].outcome.starts_with("failed:"), "{:?}", log[0]);
    }

    #[test]
    fn the_model_gate_lets_an_unreadable_model_through() {
        // The session lookup cannot succeed against a dead port, so `model` is
        // `None`. The ladder must still act: a transient failure to read the
        // model is not evidence the operator excluded it.
        let cfg = Config::test_with(|c| {
            c.dry_run = true;
            c.models = vec!["deepseek".into()];
            c.steer = crate::config::Rule {
                after_secs: 1,
                after_chars: 1,
            };
            c.warn = crate::config::Rule::OFF;
            c.cancel = crate::config::Rule::OFF;
        });
        let shared = Arc::new(Shared::new(cfg));
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        handle(
            &shared,
            &endpoint,
            &json!({"type":"iteration_started","session_id":"s","run_id":"r",
                    "iteration":1,"ts_ms":0}),
        );
        handle(
            &shared,
            &endpoint,
            &json!({"type":"reasoning_delta","session_id":"s","run_id":"r",
                    "delta":"xxxxx","ts_ms":5000}),
        );
        let log = shared.log.lock().unwrap();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].kind, "steer");
        assert_eq!(log[0].outcome, "notified", "dry-run must still notify");
    }

    #[test]
    fn the_model_gate_skips_a_model_it_can_read_and_does_not_cover() {
        let cfg = Config::test_with(|c| c.models = vec!["deepseek".into()]);
        assert!(!covered(&cfg, Some("codex/gpt-6.1-terra")));
        assert!(covered(&cfg, Some("surplus/deepseek-v4.1-flash-1017")));
        // Unreadable is covered; an empty filter covers everything.
        assert!(covered(&cfg, None));
        let open = Config::test_with(|c| c.models = Vec::new());
        assert!(covered(&open, Some("anything")));
    }

    #[test]
    fn a_run_finished_event_reports_an_empty_run() {
        let cfg = Config::test_with(|c| c.dry_run = true);
        let shared = Arc::new(Shared::new(cfg));
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        handle(
            &shared,
            &endpoint,
            &json!({"type":"run_started","session_id":"s","run_id":"r","ts_ms":0}),
        );
        handle(
            &shared,
            &endpoint,
            &json!({"type":"iteration_started","session_id":"s","run_id":"r",
                    "iteration":1,"ts_ms":0}),
        );
        handle(
            &shared,
            &endpoint,
            &json!({"type":"reasoning_delta","session_id":"s","run_id":"r",
                    "delta":"thinking","ts_ms":100}),
        );
        handle(
            &shared,
            &endpoint,
            &json!({"type":"run_finished","session_id":"s","run_id":"r",
                    "status":"completed","ts_ms":200}),
        );
        let log = shared.log.lock().unwrap();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].kind, "empty_run");
    }

    #[test]
    fn a_subagent_frame_is_attributed_to_the_subagent() {
        let cfg = Config::test_with(|c| {
            c.dry_run = true;
            c.warn = crate::config::Rule {
                after_secs: 1,
                after_chars: 10,
            };
            c.steer = crate::config::Rule::OFF;
            c.cancel = crate::config::Rule::OFF;
        });
        let shared = Arc::new(Shared::new(cfg));
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        handle(
            &shared,
            &endpoint,
            &json!({"type":"iteration_started","session_id":"s","run_id":"r",
                    "agent_id":"a_1","iteration":1,"ts_ms":0}),
        );
        handle(
            &shared,
            &endpoint,
            &json!({"type":"reasoning_delta","session_id":"s","run_id":"r",
                    "agent_id":"a_1","delta":"let me write it","ts_ms":3000}),
        );
        let log = shared.log.lock().unwrap();
        assert_eq!(log.len(), 1, "{log:?}");
        let turn = shared
            .detector
            .lock()
            .unwrap()
            .current_turn("s", 4000)
            .expect("a turn");
        assert_eq!(turn.agent_id, "a_1");
    }
}
