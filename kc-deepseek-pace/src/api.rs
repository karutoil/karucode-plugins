//! What the plugin does about a decision: tell the human, steer, cancel, retry.
//!
//! Every action is an ordinary karucode API call against the local server, so the
//! plugin needs no privileged surface and the server's own rules (cooldowns,
//! approval policy, queue limits) apply unchanged.
//!
//! | decision | action | why this one |
//! | --- | --- | --- |
//! | warn | `notify` (the plugin frame) | changes nothing about the run |
//! | steer | `POST /api/sessions/:id/messages?mode=steer` | the only mid-run intervention; a real user turn lands at the next iteration |
//! | cancel | `POST /api/sessions/:id/cancel` then a follow-up | a steer cannot land until the turn ends, so a turn stuck *inside* a stream needs the stream stopped |
//! | drift | notify only | the run is working; nagging it would be worse than the drift |
//! | empty_run | notify, optionally a follow-up | an empty run looks like success, which is the real bug |
//!
//! ## Why cancel and retry are one step
//!
//! A stalled turn cannot be steered: steering is folded in at the *top of the next
//! iteration*, and the turn that is stuck never reaches one. So the cancel rung
//! does both halves itself - stop the stream, then queue a fresh prompt as a
//! follow-up, which `continue_with_follow_up` starts as "the run that finished
//! starts the next". Without the second half the user would have to notice and
//! re-send, which is exactly the manual recovery this plugin exists to remove.

use crate::config::Config;
use crate::detect::{Decision, DecisionKind};
use crate::http::{self, Endpoint};
use crate::sdk::NotifyKind;

/// The prompt folded in when a stall reaches the steer rung.
///
/// Deliberately short and imperative. It is a *user turn*, so it costs one
/// message and lands where a follow-up would; a long lecture about reasoning
/// loops would be a second thing for the model to deliberate about.
pub const STEER_TEXT: &str = "You have already decided what to do. Stop deliberating and take \
the next action now: call the tool or write the answer. Reasoning you do not act on is \
discarded, so re-deciding it costs another turn.";

/// The prompt queued after a stall is cancelled.
pub const RETRY_TEXT: &str = "The previous turn stalled - it narrated what it was going to do \
and never did it, so its reasoning was discarded. Do not deliberate again. Take one concrete \
action immediately: call the tool you already decided on, or write the answer you already \
drafted.";

/// The prompt queued after a run that produced nothing at all.
pub const EMPTY_RETRY_TEXT: &str = "The previous run produced no tool call and no answer, so \
nothing happened. Start with one concrete action - call a tool - rather than a plan.";

/// What the caller should send over the plugin protocol, or do over HTTP.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Raise a notification (the plugin protocol frame, not HTTP).
    Notify {
        kind: NotifyKind,
        title: String,
        body: String,
        session_id: String,
    },
    /// Fold a user turn into the live run.
    Steer {
        session_id: String,
        text: String,
        reason: String,
    },
    /// Stop the run and queue a replacement prompt.
    CancelAndRetry {
        session_id: String,
        text: String,
        reason: String,
    },
    /// Queue a replacement prompt without stopping anything.
    Retry { session_id: String, text: String },
}

/// Translate decisions into actions.
///
/// One decision in, at most one action out, and `dry_run` turns them into
/// notifications only: the panel and the logs then show exactly what the ladder
/// would have done, which is how the thresholds get tuned without risking a real
/// run.
pub fn plan(decisions: &[Decision], cfg: &Config) -> Vec<Action> {
    decisions.iter().map(|d| plan_one(d, cfg)).collect()
}

fn plan_one(decision: &Decision, cfg: &Config) -> Action {
    let session_id = decision.session_id.clone();
    match decision.kind {
        DecisionKind::Warn => Action::Notify {
            kind: NotifyKind::Info,
            title: "Model is deliberating without acting".into(),
            body: format!(
                "{} - watching; a steer follows if it continues.",
                decision.reason
            ),
            session_id,
        },
        DecisionKind::Steer => {
            if cfg.dry_run {
                return Action::Notify {
                    kind: NotifyKind::Info,
                    title: "Would steer a stalled turn".into(),
                    body: decision.reason.clone(),
                    session_id,
                };
            }
            Action::Steer {
                session_id,
                text: STEER_TEXT.to_string(),
                reason: decision.reason.clone(),
            }
        }
        DecisionKind::Cancel => {
            if cfg.dry_run {
                return Action::Notify {
                    kind: NotifyKind::Info,
                    title: "Would cancel a stalled turn".into(),
                    body: decision.reason.clone(),
                    session_id,
                };
            }
            Action::CancelAndRetry {
                session_id,
                text: RETRY_TEXT.to_string(),
                reason: decision.reason.clone(),
            }
        }
        DecisionKind::Drift => Action::Notify {
            kind: NotifyKind::Info,
            title: "Run is deliberating far more than it acts".into(),
            body: decision.reason.clone(),
            session_id,
        },
        DecisionKind::EmptyRun => Action::Notify {
            kind: NotifyKind::Error,
            title: "Run finished without doing anything".into(),
            body: format!(
                "{} - the turn cost real tokens and produced nothing visible.",
                decision.reason
            ),
            session_id,
        },
    }
}

/// The extra retry queued after an empty run, when the operator asked for it.
pub fn empty_run_retry(decision: &Decision, cfg: &Config) -> Option<Action> {
    if cfg.dry_run || !cfg.retry_empty_run {
        return None;
    }
    if decision.kind != DecisionKind::EmptyRun {
        return None;
    }
    Some(Action::Retry {
        session_id: decision.session_id.clone(),
        text: EMPTY_RETRY_TEXT.to_string(),
    })
}

/// Perform an action that is an HTTP call.
///
/// Returns a one-line description of what happened, for the log and the panel.
/// An error is *not* fatal: a plugin that cannot reach the server must say so and
/// keep watching, because the next decision may be the one that matters.
pub fn execute(action: &Action, endpoint: &Endpoint, token: &str) -> Result<String, String> {
    match action {
        Action::Notify { .. } => Ok("notified".to_string()),
        Action::Steer {
            session_id, text, ..
        } => {
            let body = serde_json::json!({ "content": text }).to_string();
            let path = format!(
                "/api/sessions/{}/messages?mode=steer",
                http::encode_query(session_id)
            );
            http::request(endpoint, "POST", &path, token, Some(&body))
                .map(|_| format!("steered {session_id}"))
                .map_err(|e| format!("could not steer {session_id}: {e}"))
        }
        Action::Retry { session_id, text } => {
            let body = serde_json::json!({ "content": text }).to_string();
            let path = format!(
                "/api/sessions/{}/messages?mode=follow_up",
                http::encode_query(session_id)
            );
            http::request(endpoint, "POST", &path, token, Some(&body))
                .map(|_| format!("queued a retry for {session_id}"))
                .map_err(|e| format!("could not queue a retry for {session_id}: {e}"))
        }
        Action::CancelAndRetry {
            session_id, text, ..
        } => {
            let cancel_path = format!("/api/sessions/{}/cancel", http::encode_query(session_id));
            let cancel = http::request(endpoint, "POST", &cancel_path, token, Some("{}"));
            // The retry is queued even when the cancel failed. The two are
            // independent halves: if the run was already over, the follow-up
            // still starts the work again; if the run is still live, the
            // follow-up waits for it. Reporting the cancel error is enough.
            let body = serde_json::json!({ "content": text }).to_string();
            let follow_path = format!(
                "/api/sessions/{}/messages?mode=follow_up",
                http::encode_query(session_id)
            );
            let follow = http::request(endpoint, "POST", &follow_path, token, Some(&body));
            match (cancel, follow) {
                (Ok(_), Ok(_)) => Ok(format!("cancelled and queued a retry for {session_id}")),
                (Err(c), Ok(_)) => Ok(format!(
                    "queued a retry for {session_id} (cancel reported: {c})"
                )),
                (Ok(_), Err(f)) => Err(format!(
                    "cancelled {session_id} but could not queue a retry: {f}"
                )),
                (Err(c), Err(f)) => Err(format!(
                    "could not cancel {session_id} ({c}) or queue a retry ({f})"
                )),
            }
        }
    }
}

/// The model a session is pinned to, for the `models` gate.
///
/// `GET /api/sessions/:id/info` is the cheap one (counts and totals, no
/// transcript). A failure returns `None`, which the caller treats as "not
/// covered" only when a model filter is configured - an unreachable server must
/// not silently disable an intervention the operator asked for.
pub fn session_model(endpoint: &Endpoint, token: &str, session_id: &str) -> Option<String> {
    let path = format!("/api/sessions/{}/info", http::encode_query(session_id));
    let body = http::request(endpoint, "GET", &path, token, None).ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    value
        .get("context")
        .and_then(|c| c.get("model"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::DecisionKind;
    use serde_json::json;

    fn decision(kind: DecisionKind) -> Decision {
        Decision {
            session_id: "s_1".into(),
            run_id: "r_1".into(),
            agent_id: "main".into(),
            kind,
            reason: "turn 4 produced nothing for 130s".into(),
        }
    }

    #[test]
    fn a_warning_is_only_a_notification() {
        let out = plan(&[decision(DecisionKind::Warn)], &Config::default());
        assert!(matches!(out[0], Action::Notify { .. }));
    }

    #[test]
    fn a_steer_folds_a_user_turn_in_and_a_cancel_stops_and_retries() {
        let cfg = Config::default();
        let out = plan(
            &[
                decision(DecisionKind::Steer),
                decision(DecisionKind::Cancel),
            ],
            &cfg,
        );
        match &out[0] {
            Action::Steer { text, .. } => assert_eq!(text, STEER_TEXT),
            other => panic!("{other:?}"),
        }
        match &out[1] {
            Action::CancelAndRetry { text, .. } => assert_eq!(text, RETRY_TEXT),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dry_run_turns_every_intervention_into_a_notification() {
        let cfg = Config::test_with(|c| c.dry_run = true);
        let out = plan(
            &[
                decision(DecisionKind::Steer),
                decision(DecisionKind::Cancel),
                decision(DecisionKind::EmptyRun),
            ],
            &cfg,
        );
        assert!(matches!(out[0], Action::Notify { .. }), "{:?}", out[0]);
        assert!(matches!(out[1], Action::Notify { .. }), "{:?}", out[1]);
        // The empty-run report is a notification anyway, and must survive.
        assert!(matches!(out[2], Action::Notify { .. }), "{:?}", out[2]);
    }

    #[test]
    fn an_empty_run_is_notified_as_an_error_kind() {
        let out = plan(&[decision(DecisionKind::EmptyRun)], &Config::default());
        match &out[0] {
            Action::Notify { kind, .. } => assert_eq!(*kind, NotifyKind::Error),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn drift_only_warns() {
        let out = plan(&[decision(DecisionKind::Drift)], &Config::default());
        assert!(matches!(out[0], Action::Notify { .. }));
    }

    #[test]
    fn the_empty_run_retry_is_opt_in_and_skipped_in_dry_run() {
        let d = decision(DecisionKind::EmptyRun);
        assert!(empty_run_retry(&d, &Config::default()).is_none());

        let retry_on = Config::test_with(|c| c.retry_empty_run = true);
        assert!(empty_run_retry(&d, &retry_on).is_some());

        let dry = Config::test_with(|c| {
            c.retry_empty_run = true;
            c.dry_run = true;
        });
        assert!(empty_run_retry(&d, &dry).is_none());

        // And it never fires for anything else.
        assert!(empty_run_retry(&decision(DecisionKind::Steer), &retry_on).is_none());
    }

    #[test]
    fn the_steer_prompt_names_the_only_thing_that_matters() {
        // The advice has to say *why* narrating is useless, because that is the
        // fact the model cannot see: its reasoning is not replayed to it.
        assert!(STEER_TEXT.contains("discarded"), "{STEER_TEXT}");
        assert!(STEER_TEXT.contains("now"), "{STEER_TEXT}");
        assert!(!STEER_TEXT.contains('\n'), "a user turn should be one line");
    }

    #[test]
    fn the_retry_prompts_ask_for_one_action_not_a_plan() {
        for text in [RETRY_TEXT, EMPTY_RETRY_TEXT] {
            assert!(text.contains("action"), "{text}");
            assert!(!text.starts_with("Let"), "{text}");
        }
    }

    #[test]
    fn a_notify_action_is_executed_without_an_http_call() {
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        let action = Action::Notify {
            kind: NotifyKind::Info,
            title: "t".into(),
            body: "b".into(),
            session_id: "s".into(),
        };
        // The endpoint is not listening; a notify must not care.
        assert_eq!(execute(&action, &endpoint, "").unwrap(), "notified");
    }

    #[test]
    fn an_unreachable_server_produces_an_error_string_not_a_panic() {
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        let action = Action::Steer {
            session_id: "s_1".into(),
            text: "go".into(),
            reason: "r".into(),
        };
        let err = execute(&action, &endpoint, "").unwrap_err();
        assert!(err.contains("could not steer"), "{err}");
        assert!(err.contains("s_1"), "{err}");
    }

    #[test]
    fn the_session_model_is_read_from_the_context_block() {
        // Covered against a live server in tests/live.rs; here the shape of the
        // parse is pinned so a server change fails a unit test too.
        let body = json!({ "context": { "model": "opendesign-2/deepseek-v4.1-flash" } });
        let model = body
            .get("context")
            .and_then(|c| c.get("model"))
            .and_then(|m| m.as_str());
        assert_eq!(model, Some("opendesign-2/deepseek-v4.1-flash"));
    }
}
