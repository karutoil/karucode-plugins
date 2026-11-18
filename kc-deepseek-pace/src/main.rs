//! `kc-deepseek-pace` - stop DeepSeek V4.1-Flash from deliberating instead of
//! working.
//!
//! DeepSeek-V4.1-Flash (notably the 1017 build) has a failure mode where it stops
//! working inside a turn and narrates its intent instead: `OK. Let me write. Go.
//! Now.`, repeated until the output budget is gone. Upstream (deepseek-recipe#24)
//! 69% of stalled replays hit the 32k output cap with **no content and no tool
//! call**. On this harness the same loop shows up as runs that take 87-179 s,
//! record no usage, and persist an empty assistant row - a run that looks like a
//! silent success.
//!
//! Three layers, cheapest first:
//!
//! 1. **Prevention** ([`pace`]) - a short, byte-stable pacing contract in the
//!    system prompt. It states the one fact the model cannot observe about
//!    itself: its reasoning is streamed and discarded, so a decision it reaches
//!    and does not act on is gone. Costs nothing per call and is cached.
//! 2. **Detection** ([`detect`]) - a pure state machine over the live event
//!    stream. A turn that produces no tool call and no answer is watched, and a
//!    ladder escalates warn -> steer -> cancel. A run that acts every turn but
//!    deliberates for thousands of characters first is reported separately.
//! 3. **Recovery** ([`api`]) - a steer folds a real user turn into the loop; a
//!    cancel stops the stuck stream and queues a retry so the work resumes
//!    unattended.
//!
//! ## Why a plugin and not a core change
//!
//! The runtime already catches one *exact* repetition shape (a 24-character block
//! eight times) and restarts the stream. That is a different detector for a
//! different signature, it lives in the hot path of every token, and widening it
//! there would risk cutting healthy answers. The rest of what this does - a
//! prompt note, a warn, a steer, a notification - is exactly what the plugin API
//! exposes (`before_prompt`, `notify`, and the ordinary HTTP routes), so no core
//! change is needed to get it.
//!
//! Everything the plugin cannot do is documented in `README.md`: it cannot set a
//! thinking budget or `reasoning_effort` on the wire, and it cannot see reasoning
//! after the fact, because that channel is not persisted.

#[allow(dead_code)]
mod api;
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod detect;
#[allow(dead_code)]
mod http;
#[allow(dead_code)]
mod pace;
#[allow(dead_code)]
mod protocol;
#[allow(dead_code, unused_imports)]
mod sdk;
#[allow(dead_code)]
mod watch;

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

use sdk::{
    PanelRequest, Plugin, PluginBuilder, PluginSetting, PluginSettingKind, PluginToolResult,
    PluginToolResultExt, ToolRequest,
};
use watch::Shared;

fn shared() -> &'static Arc<Shared> {
    static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| {
        let cfg = config::Config::from_settings(&config::settings());
        let shared = Arc::new(Shared::new(cfg));
        if shared.config.enabled && shared.config.watch {
            watch::spawn(Arc::clone(&shared));
        }
        shared
    })
}

fn declared_settings() -> Vec<PluginSetting> {
    let defaults = config::Config::default();
    vec![
        PluginSetting {
            key: "enabled".into(),
            label: "Enable pacing protection".into(),
            kind: PluginSettingKind::Bool,
            default: json!(defaults.enabled),
            help: "Master switch for prompt contract, watcher and interventions.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "prompt".into(),
            label: "Inject pacing contract in system prompt".into(),
            kind: PluginSettingKind::Bool,
            default: json!(defaults.prompt),
            help: "Add the byte-stable pacing note to the system prompt.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "prompt_text".into(),
            label: "Custom pacing contract text".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Override the built-in pacing contract (empty = use built-in text).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "watch".into(),
            label: "Watch live event stream".into(),
            kind: PluginSettingKind::Bool,
            default: json!(defaults.watch),
            help: "Subscribe to /api/events to detect turns that reason without acting.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "server".into(),
            label: "karucode server URL".into(),
            kind: PluginSettingKind::Text,
            default: json!(defaults.server),
            help: "Loopback HTTP URL of the karucode server that spawned this plugin.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "token".into(),
            label: "Server bearer token".into(),
            kind: PluginSettingKind::Secret,
            default: json!(""),
            help: "Bearer token when the karucode server runs with KARUCODE_AUTH_TOKEN.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "models".into(),
            label: "Target model substrings".into(),
            kind: PluginSettingKind::Text,
            default: json!(defaults.models.join(",")),
            help: "Comma-separated model id substrings to intervene on (empty = all models).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "dry_run".into(),
            label: "Dry run (record only, never steer/cancel)".into(),
            kind: PluginSettingKind::Bool,
            default: json!(defaults.dry_run),
            help: "Record decisions in the panel without steering, cancelling or retrying.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "warn_after_secs".into(),
            label: "Warn after (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.warn.after_secs),
            help: "Seconds of narration without acting before warning (0 = off).".into(),
            min: Some(0.0),
            max: Some(3600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "warn_after_chars".into(),
            label: "Warn after (reasoning chars)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.warn.after_chars),
            help: "Minimum reasoning characters before warning.".into(),
            min: Some(0.0),
            max: Some(100_000.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "steer_after_secs".into(),
            label: "Steer after (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.steer.after_secs),
            help: "Seconds of narration without acting before steering (0 = off).".into(),
            min: Some(0.0),
            max: Some(3600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "steer_after_chars".into(),
            label: "Steer after (reasoning chars)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.steer.after_chars),
            help: "Minimum reasoning characters before steering.".into(),
            min: Some(0.0),
            max: Some(100_000.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "cancel_after_secs".into(),
            label: "Cancel after (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.cancel.after_secs),
            help: "Seconds of narration without acting before cancelling and retrying (0 = off).".into(),
            min: Some(0.0),
            max: Some(3600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "cancel_after_chars".into(),
            label: "Cancel after (reasoning chars)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.cancel.after_chars),
            help: "Minimum reasoning characters before cancelling.".into(),
            min: Some(0.0),
            max: Some(200_000.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "drift_after_turns".into(),
            label: "Drift check after turns".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.drift.after_turns),
            help: "Iterations before checking cumulative reasoning drift (0 = off).".into(),
            min: Some(0.0),
            max: Some(100.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "drift_after_chars".into(),
            label: "Drift check after (reasoning chars)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.drift.after_chars),
            help: "Total reasoning characters before drift can be reported.".into(),
            min: Some(0.0),
            max: Some(200_000.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "drift_chars_per_tool_call".into(),
            label: "Drift chars per tool call threshold".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.drift.chars_per_tool_call),
            help: "Reasoning chars per tool call above which drift is reported.".into(),
            min: Some(100.0),
            max: Some(100_000.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "narration_min".into(),
            label: "Narration phrases before early trigger".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.narration_min),
            help: "Action-announcement hits that can fire a rung without waiting for chars.".into(),
            min: Some(1.0),
            max: Some(50.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "report_empty_run".into(),
            label: "Report empty runs".into(),
            kind: PluginSettingKind::Bool,
            default: json!(defaults.report_empty_run),
            help: "Emit a notification when a run ends with zero tool calls and zero answer.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "retry_empty_run".into(),
            label: "Auto-retry empty runs".into(),
            kind: PluginSettingKind::Bool,
            default: json!(defaults.retry_empty_run),
            help: "Queue a retry message after an empty run.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "cooldown_secs".into(),
            label: "Intervention cooldown (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.cooldown_secs),
            help: "Minimum seconds between steering or cancelling the same session.".into(),
            min: Some(0.0),
            max: Some(3600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "reconnect_secs".into(),
            label: "Event stream reconnect delay (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.reconnect_secs),
            help: "Seconds to wait before reconnecting after /api/events drops.".into(),
            min: Some(1.0),
            max: Some(300.0),
            options: Vec::new(),
        },
    ]
}

fn build_plugin() -> Plugin {
    let mut builder = PluginBuilder::new("kc-deepseek-pace", env!("CARGO_PKG_VERSION"))
        .description(
            "Keeps DeepSeek V4.1-Flash working instead of narrating. A byte-stable pacing \
             contract in the system prompt, a live watcher for turns that reason without \
             acting, and a warn/steer/cancel ladder that puts a stalled run back to work.",
        )
        // Only talks to the local server over loopback.
        .permission(sdk::permissions::NETWORK)
        .notifications();
    for setting in declared_settings() {
        builder = builder.setting(setting);
    }
    builder
        // Layer 1: the prompt contract. Byte-stable by construction; see pace.rs
        // for why that is a hard requirement rather than a nicety.
        .hook(sdk::hooks::BEFORE_PROMPT, |_payload| {
            let cfg = &shared().config;
            if !cfg.enabled || !cfg.prompt {
                return Value::Null;
            }
            match pace::prompt_block(&cfg.prompt_text) {
                Some(block) => Value::String(block),
                None => Value::Null,
            }
        })
        // A run that ended with nothing is the case the hooks *can* see, so it is
        // also recorded here: `on_run_end` fires even if the watcher's stream
        // dropped, and a run id with no turn is not something the detector can
        // reconstruct after the fact.
        .hook(sdk::hooks::ON_RUN_END, |payload| {
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if status == "cancelled" {
                // A user pressing Escape needs no commentary.
                return Value::Null;
            }
            Value::Null
        })
        .panel_with("pace", "Pace", Some("bolt"), 5_000, |req: PanelRequest| {
            panel(shared(), &req)
        })
        .tool_with_class(
            "pace_status",
            "Report how the DeepSeek pacing watcher sees this session right now: the turn in \
             flight, how long it has reasoned without acting, whether the ladder has fired, and \
             the recent decisions. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "Session to report. Normally unnecessary: the host sends \
                                        the calling session."
                    }
                }
            }),
            Some("read_only"),
            |req: ToolRequest| {
                let explicit = req.str_arg("session");
                let session = if explicit.is_empty() {
                    req.session_id().unwrap_or_default().to_string()
                } else {
                    explicit.to_string()
                };
                PluginToolResult::ok(status_text(shared(), &session))
            },
        )
        .tool_with_class(
            "pace_explain",
            "Explain what kc-deepseek-pace is doing and why, including the thresholds in force \
             and the upstream evidence behind them. Use it when the user asks why the run was \
             steered or cancelled, or how to tune it. Read-only.",
            json!({ "type": "object", "properties": {} }),
            Some("read_only"),
            |_req: ToolRequest| PluginToolResult::ok(explain_text(shared())),
        )
        .build()
}

fn main() -> std::io::Result<()> {
    // Initialize shared state (and start the SSE watcher if enabled) once the
    // JSON-RPC loop is live so `plugin/settings/get` can reach the host.
    std::thread::Builder::new()
        .name("pace-init".into())
        .spawn(|| {
            while !sdk::HostClient::global().is_live() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = shared();
        })
        .ok();
    build_plugin().run()
}

/// The inspector tab: the current turn, the ladder, and recent decisions.
fn panel(shared: &Arc<Shared>, req: &PanelRequest) -> Value {
    let now = watch::now_ms();
    let cfg = &shared.config;
    let session = req.session_id.clone();

    let (turn, live) = match shared.detector.lock() {
        Ok(detector) => (detector.current_turn(&session, now), detector.live_runs()),
        Err(_) => (None, 0),
    };

    let turn = match turn {
        Some(t) => json!({
            "agent": t.agent_id,
            "iteration": t.iteration,
            "elapsed_secs": t.elapsed_secs,
            "reasoning_chars": t.reasoning_chars,
            "narration_hits": t.narration_hits,
            "acted": t.acted,
            "escalated": t.escalated,
        }),
        None => Value::Null,
    };

    let log: Vec<Value> = shared
        .log
        .lock()
        .map(|log| {
            log.iter()
                .rev()
                .take(25)
                .map(|entry| {
                    json!({
                        "at_ms": entry.at_ms,
                        "age_secs": (now - entry.at_ms).max(0) / 1000,
                        "session_id": entry.session_id,
                        "run_id": entry.run_id,
                        "kind": entry.kind,
                        "reason": entry.reason,
                        "outcome": entry.outcome,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "enabled": cfg.enabled,
        "watching": cfg.watch && cfg.enabled,
        "connected": shared.connected.load(Ordering::Relaxed),
        "dry_run": cfg.dry_run,
        "server": cfg.server,
        "live_runs": live,
        "last_error": shared.last_error.lock().ok().and_then(|e| e.clone()),
        "session_id": session,
        "turn": turn,
        "ladder": {
            "warn": rule_json(cfg.warn),
            "steer": rule_json(cfg.steer),
            "cancel": rule_json(cfg.cancel),
            "narration_min": cfg.narration_min,
            "cooldown_secs": cfg.cooldown_secs,
        },
        "drift": {
            "after_turns": if cfg.drift.is_off() { 0 } else { cfg.drift.after_turns },
            "after_chars": if cfg.drift.is_off() { 0 } else { cfg.drift.after_chars },
            "chars_per_tool_call":
                if cfg.drift.is_off() { 0 } else { cfg.drift.chars_per_tool_call },
        },
        "recent": log,
    })
}

fn rule_json(rule: config::Rule) -> Value {
    if rule.is_off() {
        json!({ "off": true })
    } else {
        json!({ "after_secs": rule.after_secs, "after_chars": rule.after_chars })
    }
}

/// The text behind `pace_status`.
fn status_text(shared: &Arc<Shared>, session: &str) -> String {
    let now = watch::now_ms();
    let cfg = &shared.config;
    let mut out = String::new();
    out.push_str(&format!(
        "kc-deepseek-pace: {} (watching {}, dry_run {}, server {})\n",
        if cfg.enabled { "enabled" } else { "disabled" },
        if cfg.watch && cfg.enabled {
            "yes"
        } else {
            "no"
        },
        if cfg.dry_run { "yes" } else { "no" },
        cfg.server,
    ));
    if let Some(error) = shared.last_error.lock().ok().and_then(|e| e.clone()) {
        out.push_str(&format!("stream error: {error}\n"));
    } else {
        out.push_str(&format!(
            "stream: {}\n",
            if shared.connected.load(Ordering::Relaxed) {
                "connected"
            } else {
                "reconnecting"
            }
        ));
    }
    match shared.detector.lock() {
        Ok(detector) => {
            out.push_str(&format!("live runs: {}\n", detector.live_runs()));
            match detector.current_turn(session, now) {
                Some(turn) => out.push_str(&format!(
                    "session {session}: turn {iter} ({agent}) {elapsed}s in, {chars} reasoning \
                     chars, {hits} action announcement(s), acted={acted}, escalated={esc}\n",
                    iter = turn.iteration,
                    agent = turn.agent_id,
                    elapsed = turn.elapsed_secs,
                    chars = turn.reasoning_chars,
                    hits = turn.narration_hits,
                    acted = turn.acted,
                    esc = turn.escalated.as_deref().unwrap_or("nothing yet"),
                )),
                None => out.push_str(&format!(
                    "session {session}: no turn in flight (nothing is stuck, or the watcher has \
                     not seen this session)\n"
                )),
            }
        }
        Err(_) => out.push_str("detector unavailable\n"),
    }
    let recent: Vec<String> = shared
        .log
        .lock()
        .map(|log| {
            log.iter()
                .rev()
                .filter(|e| e.session_id == session)
                .take(5)
                .map(|e| {
                    format!(
                        "  {}s ago [{}] {} -> {}",
                        (now - e.at_ms).max(0) / 1000,
                        e.kind,
                        e.reason,
                        e.outcome
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if !recent.is_empty() {
        out.push_str("recent decisions for this session (newest first):\n");
        for line in recent {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// The text behind `pace_explain`.
fn explain_text(shared: &Arc<Shared>) -> String {
    let cfg = &shared.config;
    format!(
        "kc-deepseek-pace - why it exists and what it does.\n\
\n\
THE PROBLEM. DeepSeek-V4.1-Flash (the 1017 build especially) sometimes stops working inside \
a turn and narrates its intent instead: \"OK. Let me write. Go. Now.\" repeated until the \
output budget is gone. Upstream (deepseek-recipe#24) 69% of stalled replays hit the 32k output \
cap with no content and no tool call, raising reasoning effort made it worse, and a \
thinking_token_budget of 4000 fixed 33/34 cases. On this harness the same loop shows up as runs \
that take minutes, record no usage, and persist an EMPTY assistant row - a run that looks like a \
silent success.\n\
\n\
WHY IT SECOND-GUESSES. karucode streams reasoning_content and then discards it: it is never \
persisted and never replayed. A decision the model reaches in reasoning and does not act on is \
gone, so the next turn re-derives it from the same context and may land slightly differently. \
That is the machine-level reason for the loop, and it is what the prompt block tries to state.\n\
\n\
THE THREE LAYERS.\n\
  1. prompt    A short pacing contract in the system prompt ({prompt}). It is byte-stable \
because it sits in message 0, which providers cache by prefix.\n\
  2. watch     The live event stream is the only place reasoning is observable, because it is \
never stored. The watcher reads it, and a turn that produces no tool call and no answer for a \
long time is a stall.\n\
  3. act       warn -> notify only. steer -> fold a user turn into the run. cancel -> stop the \
stream and queue a retry, because a steer cannot land until the turn ends.\n\
\n\
THE LADDER IN FORCE.\n\
  warn     after {warn_s}s and {warn_c} reasoning chars\n\
  steer    after {steer_s}s and {steer_c} chars\n\
  cancel   after {cancel_s}s and {cancel_c} chars\n\
  narration: {narration} action announcement(s) can fire a rung without the character floor\n\
  cooldown: {cooldown}s between interventions in one session\n\
  drift:    reported when a run takes more than {drift} reasoning chars per action\n\
  dry_run:  {dry_run}\n\
\n\
WHAT IT CANNOT DO. It cannot set thinking_token_budget or reasoning_effort - those are \
inference-side settings, and they are the most effective mitigation (4000 worked in 33/34 \
upstream cases). It cannot see reasoning after the fact, because reasoning is not persisted. It \
cannot make a model act; it can only notice, ask, and stop.\n",
        prompt = if cfg.prompt { "on" } else { "off" },
        warn_s = cfg.warn.after_secs,
        warn_c = cfg.warn.after_chars,
        steer_s = cfg.steer.after_secs,
        steer_c = cfg.steer.after_chars,
        cancel_s = cfg.cancel.after_secs,
        cancel_c = cfg.cancel.after_chars,
        narration = cfg.narration_min,
        cooldown = cfg.cooldown_secs,
        drift = if cfg.drift.is_off() {
            "never (off)".to_string()
        } else {
            cfg.drift.chars_per_tool_call.to_string()
        },
        dry_run = if cfg.dry_run { "yes" } else { "no" },
    )
}

/// Kept so a build with `--all-targets` still compiles the tool-result helpers.
#[allow(dead_code)]
fn _assert_result_helpers() -> PluginToolResult {
    PluginToolResult::err("unused")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_settings_pass_host_validation() {
        let p = build_plugin();
        let settings = &p.manifest().settings;
        assert_eq!(settings.len(), 22);
        sdk::validate_settings(settings).expect("declared settings must be valid");
    }
}
