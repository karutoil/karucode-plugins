//! Settings for `kc-deepseek-pace`.
//!
//! Everything arrives as JSON in `KARUCODE_PLUGIN_SETTINGS` (the `settings` table
//! of the `[[plugins]]` entry). Every key is optional: with no settings at all
//! the plugin runs with the defaults below, which are tuned on the measurements
//! in `README.md`.
//!
//! Two of these deserve a word up front, because both are surprising:
//!
//! * `server` cannot be discovered. The host clears the plugin's environment
//!   (nothing named `*_TOKEN`/`*_KEY` is inherited either), and the plugin
//!   protocol carries no server address, so the operator states it. Loopback is
//!   the only supported shape: this plugin talks to the same server that spawned
//!   it, over plain HTTP.
//! * `token` must be set by hand for the same reason. If the server runs with
//!   `KARUCODE_AUTH_TOKEN`, the plugin cannot read it from the environment - by
//!   design - so it goes here, or the calls are refused with 401.

use serde_json::Value;

/// Where the karucode server listens. The plugin is spawned by it and runs on the
/// same machine, so loopback is right; a remote address would need TLS, which
/// this build does not speak (see `http.rs`).
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8787";

/// One rung of the stall ladder.
///
/// A rung fires when the turn has produced *no* tool call and *no* answer text
/// for at least `after_secs` **and** has narrated at least `after_chars`
/// characters of reasoning. Both conditions, not either: a fast turn that
/// reasons a lot is normal, and a slow turn that reasons a little is a network
/// problem, not a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// Wall clock inside the turn before this rung may fire.
    pub after_secs: u64,
    /// Reasoning characters in this turn before this rung may fire.
    pub after_chars: usize,
}

impl Rule {
    /// A rung that never fires. `after_secs = 0` would fire on the first delta of
    /// every turn, so "off" has to be spelled out rather than left at zero.
    pub const OFF: Rule = Rule {
        after_secs: u64::MAX,
        after_chars: usize::MAX,
    };

    pub fn is_off(&self) -> bool {
        self.after_secs == u64::MAX
    }
}

/// A run that keeps *acting* but re-derives every decision first.
///
/// This is the other half of the symptom, and the per-turn rule cannot see it: a
/// turn that ends in a tool call is not stuck, so the ladder stays silent, yet the
/// run as a whole spends most of its wall clock narrating. Measured on this
/// machine, healthy turns act 2-3 s after the iteration starts and stalled ones
/// spend 6-16 s; over a 300-iteration run that difference is an hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftRule {
    /// Iterations a run must have started before drift may be reported.
    pub after_turns: u32,
    /// Reasoning characters the run must have spent in total.
    pub after_chars: usize,
    /// Reasoning characters per tool call above which the run is deliberating
    /// more than it is working.
    pub chars_per_tool_call: usize,
}

impl DriftRule {
    pub const OFF: DriftRule = DriftRule {
        after_turns: u32::MAX,
        after_chars: usize::MAX,
        chars_per_tool_call: usize::MAX,
    };

    pub fn is_off(&self) -> bool {
        self.after_turns == u32::MAX
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Master switch. Off means no prompt block, no watcher, no tools' data.
    pub enabled: bool,
    /// Put the pacing contract in the system prompt.
    ///
    /// On by default and **not** gated on the model, deliberately: `before_prompt`
    /// output is part of message 0, and the model can be re-pinned mid-session
    /// (`PATCH /api/sessions/:id/model`). A block that appears or disappears when
    /// the pin changes would invalidate the provider's prefix cache from
    /// position 1 and re-bill the whole conversation. The advice is generic
    /// enough to be harmless on a model that never loops.
    pub prompt: bool,
    /// Replace the built-in pacing contract. Empty keeps the built-in text.
    pub prompt_text: String,

    // --- watcher ---------------------------------------------------------
    /// Read `/api/events` and watch for stalls.
    pub watch: bool,
    pub server: String,
    /// Bearer token, when the server has `KARUCODE_AUTH_TOKEN` set.
    pub token: String,
    /// Model-id substrings the *interventions* apply to. Empty means every model.
    ///
    /// The prompt block is not gated on this (see `prompt`); only the actions
    /// that change a run - a steer, a cancel, a retried turn - are.
    pub models: Vec<String>,
    /// Decide and record, but never steer, cancel or retry. The panel still shows
    /// what it would have done, which is how the thresholds get tuned safely.
    pub dry_run: bool,

    // --- ladder ----------------------------------------------------------
    /// Tell the human (once per turn) that the model is narrating without acting.
    pub warn: Rule,
    /// Fold a real user turn into the run: "you have already decided this, act".
    pub steer: Rule,
    /// Stop the stream and queue a retry. The last rung, because it is the only
    /// one that can lose work.
    pub cancel: Rule,
    pub drift: DriftRule,
    /// How many action announcements a turn needs before the narration signature
    /// can fire a rung on its own, without waiting for the character floor.
    ///
    /// Three is the measured shape: the loops in deepseek-recipe#24 say it four
    /// to eight times before the output cap, and ordinary prose about a plan
    /// rarely says "let me write" three times in one turn.
    pub narration_min: usize,
    /// A run that ended with zero tool calls and zero answer text is reported to
    /// the human. On by default: an empty run currently looks like a silent
    /// success, which is the worst possible thing for it to look like.
    pub report_empty_run: bool,
    /// Queue a retry after an empty run, so the work resumes unattended.
    ///
    /// Off by default, unlike the report: the user may want to read the empty run
    /// first, and a retry spends tokens.
    pub retry_empty_run: bool,
    /// Seconds before the same session can be steered or cancelled again. A loop
    /// that is going to be stopped is often stopped more than once.
    pub cooldown_secs: u64,
    /// Seconds between reconnect attempts after the event stream drops.
    pub reconnect_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            prompt: true,
            prompt_text: String::new(),

            watch: true,
            server: DEFAULT_SERVER.to_string(),
            token: String::new(),
            models: vec!["deepseek".to_string()],
            dry_run: false,

            // The ladder is deliberately generous. Every rung requires the turn
            // to have produced nothing at all, so the cost of waiting too long is
            // one wasted turn; the cost of firing too early is interrupting real
            // work, which is much worse on a harness the user is watching.
            warn: Rule {
                after_secs: 45,
                after_chars: 3_000,
            },
            steer: Rule {
                after_secs: 120,
                after_chars: 10_000,
            },
            cancel: Rule {
                after_secs: 240,
                after_chars: 24_000,
            },
            drift: DriftRule {
                after_turns: 8,
                after_chars: 24_000,
                chars_per_tool_call: 8_000,
            },
            narration_min: 3,
            report_empty_run: true,
            retry_empty_run: false,
            cooldown_secs: 90,
            reconnect_secs: 5,
        }
    }
}

impl Config {
    pub fn from_settings(settings: &Value) -> Self {
        let mut config = Config::default();
        let s = |k: &str| settings.get(k).and_then(Value::as_str);
        let b = |k: &str| settings.get(k).and_then(Value::as_bool);
        let u = |k: &str| settings.get(k).and_then(Value::as_u64);
        let ns = |k: &str| settings.get(k).and_then(Value::as_u64).map(|v| v as usize);

        if let Some(v) = b("enabled") {
            config.enabled = v;
        }
        if let Some(v) = b("prompt") {
            config.prompt = v;
        }
        if let Some(v) = s("prompt_text") {
            config.prompt_text = v.to_string();
        }

        if let Some(v) = b("watch") {
            config.watch = v;
        }
        if let Some(v) = s("server") {
            config.server = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = s("token") {
            config.token = v.to_string();
        }
        if let Some(list) = settings.get("models").and_then(Value::as_array) {
            config.models = list
                .iter()
                .filter_map(Value::as_str)
                .map(|m| m.trim().to_ascii_lowercase())
                .filter(|m| !m.is_empty())
                .collect();
        }
        if let Some(v) = b("dry_run") {
            config.dry_run = v;
        }

        // A rung is written either as a bare number (seconds, characters left at
        // the default) or as a table. `0` turns the rung off, which is the
        // spelling an operator reaches for; `Rule::OFF` is what it means.
        config.warn = rule(settings.get("warn"), config.warn);
        config.steer = rule(settings.get("steer"), config.steer);
        config.cancel = rule(settings.get("cancel"), config.cancel);
        if let Some(v) = settings.get("drift") {
            let base = config.drift;
            config.drift = DriftRule {
                after_turns: v
                    .get("after_turns")
                    .and_then(Value::as_u64)
                    .map(|n| n as u32)
                    .unwrap_or(base.after_turns),
                after_chars: v
                    .get("after_chars")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(base.after_chars),
                chars_per_tool_call: v
                    .get("chars_per_tool_call")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(base.chars_per_tool_call),
            };
            if v.as_bool() == Some(false) || config.drift.after_turns == 0 {
                config.drift = DriftRule::OFF;
            }
        }

        if let Some(v) = settings.get("narration_min").and_then(Value::as_u64) {
            config.narration_min = (v as usize).max(1);
        }
        if let Some(v) = b("report_empty_run") {
            config.report_empty_run = v;
        }
        if let Some(v) = b("retry_empty_run") {
            config.retry_empty_run = v;
        }
        if let Some(v) = u("cooldown_secs") {
            config.cooldown_secs = v;
        }
        if let Some(v) = u("reconnect_secs") {
            config.reconnect_secs = v.clamp(1, 300);
        }
        let _ = ns;
        config
    }

    /// Whether the interventions apply to this model id (the prompt is separate).
    pub fn covers_model(&self, model: &str) -> bool {
        if self.models.is_empty() {
            return true;
        }
        let model = model.to_ascii_lowercase();
        self.models.iter().any(|needle| model.contains(needle))
    }
}

fn rule(value: Option<&Value>, base: Rule) -> Rule {
    let Some(value) = value else { return base };
    if value.as_bool() == Some(false) || value.as_u64() == Some(0) {
        return Rule::OFF;
    }
    if let Some(secs) = value.as_u64() {
        return Rule {
            after_secs: secs,
            after_chars: base.after_chars,
        };
    }
    let after_secs = value
        .get("after_secs")
        .and_then(Value::as_u64)
        .unwrap_or(base.after_secs);
    let after_chars = value
        .get("after_chars")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(base.after_chars);
    if after_secs == 0 && after_chars == 0 {
        return Rule::OFF;
    }
    Rule {
        after_secs,
        after_chars,
    }
}

/// The settings handed to the plugin by the host, read once per process.
pub fn settings() -> Value {
    std::env::var("KARUCODE_PLUGIN_SETTINGS")
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
impl Config {
    /// `Config::default()` with a few fields varied, in one expression.
    ///
    /// A helper rather than `let mut c = Config::default(); c.x = …` at each test
    /// site: that shape trips `clippy::field_reassign_with_default`, and spelling
    /// out a whole struct literal for two differing fields buries the difference.
    pub fn test_with(f: impl FnOnce(&mut Config)) -> Config {
        let mut config = Config::default();
        f(&mut config);
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_are_the_documented_ladder() {
        let c = Config::default();
        assert!(c.enabled && c.prompt && c.watch);
        assert!(!c.dry_run);
        assert_eq!(c.server, DEFAULT_SERVER);
        // The ladder must increase: a lower rung that fires after a higher one
        // would make the escalation order meaningless.
        assert!(c.warn.after_secs < c.steer.after_secs);
        assert!(c.steer.after_secs < c.cancel.after_secs);
        assert!(c.warn.after_chars < c.steer.after_chars);
        assert!(c.steer.after_chars < c.cancel.after_chars);
    }

    #[test]
    fn an_empty_settings_object_keeps_every_default() {
        let c = Config::from_settings(&Value::Null);
        assert_eq!(c.warn, Config::default().warn);
        assert_eq!(c.server, DEFAULT_SERVER);
    }

    #[test]
    fn zero_turns_a_rung_off() {
        let c = Config::from_settings(&json!({"steer": 0}));
        assert!(c.steer.is_off(), "{}", c.steer.after_secs);
        // The others are untouched: turning off one rung must not disarm the
        // ladder.
        assert!(!c.warn.is_off());
        assert!(!c.cancel.is_off());
    }

    #[test]
    fn a_bare_number_sets_seconds_and_a_table_sets_both() {
        let c = Config::from_settings(&json!({"cancel": 300}));
        assert_eq!(c.cancel.after_secs, 300);
        assert_eq!(
            c.cancel.after_chars,
            Config::default().cancel.after_chars,
            "a bare number must not reset the character floor to zero"
        );

        let c = Config::from_settings(&json!({"cancel": {"after_secs": 60, "after_chars": 500}}));
        assert_eq!(c.cancel.after_secs, 60);
        assert_eq!(c.cancel.after_chars, 500);
    }

    #[test]
    fn the_trailing_slash_on_server_is_dropped() {
        let c = Config::from_settings(&json!({"server": "http://127.0.0.1:9000/"}));
        assert_eq!(c.server, "http://127.0.0.1:9000");
    }

    #[test]
    fn model_gating_is_a_substring_match_and_empty_means_all() {
        let c = Config::from_settings(&json!({"models": []}));
        assert!(c.covers_model("anything"));
        let c = Config::default();
        assert!(c.covers_model("surplus/deepseek-v4.1-flash-1017"));
        assert!(c.covers_model("DeepSeek-V4-Pro"));
        assert!(!c.covers_model("codex/gpt-6.1-terra"));
    }

    #[test]
    fn drift_can_be_turned_off_by_a_flag_or_by_zero_turns() {
        assert!(Config::from_settings(&json!({"drift": false}))
            .drift
            .is_off());
        assert!(Config::from_settings(&json!({"drift": {"after_turns": 0}}))
            .drift
            .is_off());
        assert!(
            !Config::from_settings(&json!({"drift": {"after_turns": 3}}))
                .drift
                .is_off()
        );
    }
}
