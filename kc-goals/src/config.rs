//! Settings for `kc-goals`.
//!
//! Everything arrives as JSON in `KARUCODE_PLUGIN_SETTINGS` (the `settings` table
//! of the `[[plugins]]` entry). Every key is optional, and with no settings at all
//! the plugin runs with the defaults below.
//!
//! Two of them cannot be discovered and have to be stated:
//!
//! * `server` - the host clears the plugin's environment and the protocol carries
//!   no server address, so the operator names it. Loopback only: the plugin talks
//!   to the server that spawned it, over plain HTTP (see `http.rs`).
//! * `token` - only when the server runs with `KARUCODE_AUTH_TOKEN`. The plugin
//!   cannot read it from the environment by design, so it goes here or the calls
//!   are refused with 401.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8787";

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Master switch. Off means no prompt block, no ticks, no continuation.
    pub enabled: bool,
    /// Put the orientation block in the system prompt while a goal is active.
    pub prompt: bool,
    /// The engine resumes a stopped run.
    pub auto_continue: bool,
    /// The karucode server the plugin talks to, without a trailing slash.
    pub server: String,
    /// Bearer token, when the server requires one.
    pub token: String,
    /// How often the reconciler re-examines sessions with an open goal.
    pub tick_secs: u64,
    /// Floor between two continuations for one session, so an instantly-ending
    /// run cannot become a hot loop.
    pub min_gap_secs: u64,
    /// Defaults a goal inherits when `goal_set` omits a budget.
    pub default_budget: super::model::Budget,
    /// Cap on the continuation message we post. A prompt that is too long is a
    /// bug in this plugin, not something to send and hope.
    pub max_prompt_chars: usize,
    /// Run a criterion's `check` command instead of trusting the agent's word.
    ///
    /// Off by default: it is the only place this plugin executes anything, it
    /// needs the `shell` permission, and the command has the plugin's own OS
    /// rights. On, a criterion with a check cannot be marked met until the
    /// command exits 0.
    pub verify_commands: bool,
    /// Seconds a check command may run before it is killed.
    pub check_timeout_secs: u64,
    /// Where the session -> workspace index lives. It is a hint, never the truth:
    /// the goal files themselves are the truth.
    pub state_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            prompt: true,
            auto_continue: true,
            server: DEFAULT_SERVER.to_string(),
            token: String::new(),
            tick_secs: 20,
            min_gap_secs: 5,
            default_budget: super::model::Budget::default(),
            max_prompt_chars: 5000,
            verify_commands: false,
            check_timeout_secs: 120,
            state_dir: default_state_dir(),
        }
    }
}

/// `$HOME/.local/share/karucode/kc-goals`, falling back to the temp dir.
fn default_state_dir() -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => format!("{home}/.local/share/karucode/kc-goals"),
        _ => std::env::temp_dir()
            .join("karucode-kc-goals")
            .to_string_lossy()
            .to_string(),
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0 && *f <= u64::MAX as f64)
            .map(|f| f.round() as u64)
    })
}

pub fn settings() -> Value {
    let env = std::env::var("KARUCODE_PLUGIN_SETTINGS")
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let rpc = if crate::sdk::HostClient::global().is_live() {
        crate::sdk::settings()
    } else {
        BTreeMap::new()
    };
    if rpc.is_empty() {
        return env;
    }
    let mut map = match env {
        Value::Object(obj) => obj,
        _ => Map::new(),
    };
    for (k, v) in rpc {
        map.insert(k, v);
    }
    Value::Object(map)
}

impl Settings {
    pub fn from_settings(settings: &Value) -> Self {
        let mut config = Settings::default();
        let s = |k: &str| settings.get(k).and_then(Value::as_str);
        let b = |k: &str| settings.get(k).and_then(Value::as_bool);
        let u = |k: &str| settings.get(k).and_then(json_u64);

        if let Some(v) = b("enabled") {
            config.enabled = v;
        }
        if let Some(v) = b("prompt") {
            config.prompt = v;
        }
        if let Some(v) = b("auto_continue") {
            config.auto_continue = v;
        }
        if let Some(v) = s("server") {
            let trimmed = v.trim().trim_end_matches('/');
            if !trimmed.is_empty() {
                config.server = trimmed.to_string();
            }
        }
        if let Some(v) = s("token") {
            config.token = v.to_string();
        }
        if let Some(v) = u("tick_secs") {
            config.tick_secs = v.clamp(2, 600);
        }
        if let Some(v) = u("min_gap_secs") {
            config.min_gap_secs = v.clamp(0, 3600);
        }
        if let Some(v) = u("max_prompt_chars") {
            config.max_prompt_chars = (v as usize).clamp(500, 40_000);
        }
        if let Some(v) = b("verify_commands") {
            config.verify_commands = v;
        }
        if let Some(v) = u("check_timeout_secs") {
            config.check_timeout_secs = v.clamp(5, 600);
        }
        if let Some(v) = s("state_dir") {
            let trimmed = v.trim().trim_end_matches('/');
            if !trimmed.is_empty() {
                config.state_dir = trimmed.to_string();
            }
        }
        let budget_obj = settings.get("budget");
        if let Some(budget) = budget_obj {
            config.default_budget = config.default_budget.merged_with(budget).clamped();
        }
        let default_stalled = super::model::Budget::default().max_stalled_resumes as u64;
        let has_nested_stalled = budget_obj.is_some_and(|b| {
            b.get("max_stalled_resumes").is_some() || b.get("max_no_progress").is_some()
        });
        let flat_stalled = u("max_stalled_resumes")
            .filter(|v| !has_nested_stalled || *v != default_stalled)
            .or_else(|| u("max_no_progress"));
        // Flat spellings are convenient in a TOML `[plugins.settings]` table and
        // in Settings -> Plugins & MCP.
        if u("max_continuations").is_some()
            || u("max_minutes").is_some()
            || flat_stalled.is_some()
            || u("max_failed_retries").is_some()
        {
            config.default_budget = config.default_budget.merged_with(&serde_json::json!({
                "max_continuations": u("max_continuations"),
                "max_minutes": u("max_minutes"),
                "max_stalled_resumes": flat_stalled,
                "max_failed_retries": u("max_failed_retries"),
            })).clamped();
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_settings_are_the_defaults() {
        let c = Settings::from_settings(&Value::Null);
        assert_eq!(c, Settings::default());
        assert!(c.enabled && c.auto_continue && c.prompt);
        assert_eq!(c.server, DEFAULT_SERVER);
        assert_eq!(c.tick_secs, 20);
        // No budget by default: a goal runs until it is finished.
        assert_eq!(c.default_budget.max_continuations, 0);
        assert_eq!(c.default_budget.max_minutes, 0);
        assert_eq!(c.default_budget.max_failed_retries, 0);
        assert_eq!(c.default_budget.max_stalled_resumes, 5);
        assert!(!c.verify_commands, "running commands is opt-in");
        assert_eq!(c.check_timeout_secs, 120);
    }

    #[test]
    fn explicit_null_is_the_defaults_not_a_crash() {
        let c = Settings::from_settings(&Value::Null);
        assert_eq!(c.tick_secs, Settings::default().tick_secs);
    }

    #[test]
    fn a_partial_object_keeps_the_other_defaults() {
        let c = Settings::from_settings(&json!({"tick_secs": 5}));
        assert_eq!(c.tick_secs, 5);
        assert_eq!(c.server, DEFAULT_SERVER);
        assert_eq!(c.token, "");
        assert_eq!(c.default_budget, super::super::model::Budget::default());
    }

    #[test]
    fn a_broken_setting_does_not_take_the_plugin_down() {
        // A string where a number belongs is ignored, not fatal.
        let c = Settings::from_settings(&json!({"tick_secs": "soon"}));
        assert_eq!(c.tick_secs, 20);
    }

    #[test]
    fn the_server_url_loses_its_trailing_slash() {
        let c = Settings::from_settings(&json!({"server": "http://127.0.0.1:9000/"}));
        assert_eq!(c.server, "http://127.0.0.1:9000");
    }

    #[test]
    fn intervals_are_clamped_to_something_runnable() {
        assert_eq!(Settings::from_settings(&json!({"tick_secs": 0})).tick_secs, 2);
        assert_eq!(
            Settings::from_settings(&json!({"tick_secs": 100_000})).tick_secs,
            600
        );
        assert_eq!(
            Settings::from_settings(&json!({"max_prompt_chars": 1})).max_prompt_chars,
            500
        );
    }

    #[test]
    fn a_nested_budget_overrides_only_what_it_names() {
        let c = Settings::from_settings(&json!({"budget": {"max_continuations": 4}}));
        assert_eq!(c.default_budget.max_continuations, 4);
        assert_eq!(c.default_budget.max_minutes, 0, "still unlimited");
        assert_eq!(c.default_budget.max_stalled_resumes, 5);
    }

    #[test]
    fn flat_budget_spellings_work_too() {
        let c = Settings::from_settings(&json!({"max_continuations": 7, "max_minutes": 30}));
        assert_eq!(c.default_budget.max_continuations, 7);
        assert_eq!(c.default_budget.max_minutes, 30);
        assert_eq!(c.default_budget.max_stalled_resumes, 5);
    }

    #[test]
    fn the_old_no_progress_spelling_still_means_the_stall_detector() {
        let c = Settings::from_settings(&json!({"max_no_progress": 2}));
        assert_eq!(c.default_budget.max_stalled_resumes, 2);
    }

    #[test]
    fn verification_is_off_unless_the_operator_turns_it_on() {
        assert!(!Settings::from_settings(&json!({})).verify_commands);
        let c = Settings::from_settings(&json!({"verify_commands": true}));
        assert!(c.verify_commands);
        assert_eq!(
            Settings::from_settings(&json!({"check_timeout_secs": 1})).check_timeout_secs,
            5
        );
        assert_eq!(
            Settings::from_settings(&json!({"check_timeout_secs": 9999})).check_timeout_secs,
            600
        );
    }

    #[test]
    fn a_state_dir_override_is_used() {
        let c = Settings::from_settings(&json!({"state_dir": "/tmp/x/"}));
        assert_eq!(c.state_dir, "/tmp/x");
    }
}
