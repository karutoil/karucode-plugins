//! Process settings. The plugin talks only to the karucode server that
//! spawned it, and the vision model it uses comes from karucode's own
//! provider configuration - so the settings are the server address, a token,
//! and bounds. No endpoint, model id or API key of its own.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The loopback server kc-goals and this plugin speak to.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8787";
/// How long one vision run may take end to end. The host caps a plugin tool
/// call at 60s, so the default must fit inside it.
pub const DEFAULT_RUN_TIMEOUT_SECS: u64 = 55;
pub const DEFAULT_MAX_IMAGE_MB: usize = 8;
/// The upload route's own ceiling (`MAX_UPLOAD_BYTES`); keep in step.
pub const DEFAULT_MAX_QUESTION_CHARS: usize = 4_000;
/// Environment variable consulted for the API token when none is set.
pub const DEFAULT_TOKEN_ENV: &str = "IMAGE_HANDOFF_TOKEN";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The karucode server, plain HTTP, loopback.
    #[serde(default = "default_server")]
    pub server: String,
    /// Bearer token for the server. The host clears a plugin's environment,
    /// so this comes from the `[[plugins]]` `env` table (see README §2) or
    /// here directly.
    #[serde(default)]
    pub api_token: Option<String>,
    /// Environment variable holding the token (`IMAGE_HANDOFF_TOKEN` default).
    #[serde(default)]
    pub api_token_env: Option<String>,
    /// A vision model override, resolved through karucode's provider config
    /// exactly like the automatic choice - as `{provider, model}`, or a bare
    /// model string when it is unambiguous.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// Wall-clock bound for one whole handoff (session → run → answer).
    #[serde(default = "default_run_timeout", deserialize_with = "de_lenient_u64")]
    pub run_timeout_secs: u64,
    /// Largest image accepted, in MiB (the upload ceiling is 8 MiB).
    #[serde(
        default = "default_max_image_mb",
        deserialize_with = "de_lenient_usize"
    )]
    pub max_image_mb: usize,
    /// Largest answer returned to the calling model, in characters.
    #[serde(
        default = "default_max_output_chars",
        deserialize_with = "de_lenient_usize"
    )]
    pub max_output_chars: usize,
}

fn de_lenient_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0 && *f <= u64::MAX as f64)
                .map(|f| f.round() as u64)
        })
        .ok_or_else(|| serde::de::Error::custom("expected a non-negative number"))
}

fn de_lenient_usize<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0 && *f <= usize::MAX as f64)
                .map(|f| f.round() as u64)
        })
        .map(|n| n as usize)
        .ok_or_else(|| serde::de::Error::custom("expected a non-negative integer"))
}

fn default_server() -> String {
    DEFAULT_SERVER.to_string()
}
fn default_run_timeout() -> u64 {
    DEFAULT_RUN_TIMEOUT_SECS
}
fn default_max_image_mb() -> usize {
    DEFAULT_MAX_IMAGE_MB
}
fn default_max_output_chars() -> usize {
    12_000
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: default_server(),
            api_token: None,
            api_token_env: None,
            model: None,
            provider: None,
            run_timeout_secs: DEFAULT_RUN_TIMEOUT_SECS,
            max_image_mb: DEFAULT_MAX_IMAGE_MB,
            max_output_chars: 12_000,
        }
    }
}

impl Settings {
    #[cfg(test)]
    pub fn parse(raw: Option<&str>) -> (Self, Option<String>) {
        let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
            return (Self::default(), None);
        };
        match serde_json::from_str::<Value>(raw) {
            Ok(Value::Null) => (Self::default(), None),
            Ok(value) => match serde_json::from_value::<Self>(value) {
                Ok(settings) => (settings.normalized(), None),
                Err(e) => (
                    Self::default(),
                    Some(format!("plugin settings are invalid ({e}); using defaults")),
                ),
            },
            Err(e) => (
                Self::default(),
                Some(format!(
                    "plugin settings are not valid JSON ({e}); using defaults"
                )),
            ),
        }
    }

    pub fn normalized(mut self) -> Self {
        let server = self.server.trim().trim_end_matches('/').to_string();
        self.server = if server.is_empty() {
            default_server()
        } else {
            server
        };
        self.run_timeout_secs = self.run_timeout_secs.clamp(1, 600);
        // The upload route's hard ceiling; do not configure past it.
        self.max_image_mb = self.max_image_mb.clamp(1, 8);
        self.max_output_chars = self.max_output_chars.clamp(1_000, 20_000);
        self.api_token = self
            .api_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        self.api_token_env = self
            .api_token_env
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        self.model = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        self.provider = self
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        self
    }

    pub fn merge_env_and_rpc(env: Value, rpc: BTreeMap<String, Value>) -> Value {
        let mut map = match env {
            Value::Object(obj) => obj,
            _ => Map::new(),
        };
        for (k, v) in rpc {
            map.insert(k, v);
        }
        Value::Object(map)
    }

    /// The token: the explicit setting, then the named environment variable.
    pub fn resolve_token(&self) -> Result<String, String> {
        if let Some(token) = self
            .api_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return Ok(token.to_string());
        }
        let var = self
            .api_token_env
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_TOKEN_ENV);
        if let Ok(token) = std::env::var(var) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
        // Keyless is worth trying: a server without web auth or auth token
        // accepts plain requests, and the token is only needed when it does not.
        Ok(String::new())
    }

    /// The model pin to request, when settings name one. `None` means the
    /// session follows karucode's default and the plugin's own vision
    /// resolution decides what to ask for.
    pub fn requested_model(&self) -> Option<(String, String)> {
        let model = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())?;
        let provider = self
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or_default();
        Some((provider.to_string(), model.to_string()))
    }

    /// Redacted view for the `status` tool; never echoes a resolved token.
    pub fn public_json(
        &self,
        endpoint: Option<&crate::karucode::Resolved>,
        problem: Option<String>,
    ) -> Value {
        serde_json::json!({
            "ready": problem.is_none(),
            "server": self.server,
            "token_set": !self.resolve_token().unwrap_or_default().is_empty(),
            "endpoint": endpoint.map(crate::karucode::status_json_endpoint),
            "requested_model": self.model,
            "requested_provider": self.provider,
            "problem": problem,
            "run_timeout_secs": self.run_timeout_secs,
            "max_image_mb": self.max_image_mb,
            "max_output_chars": self.max_output_chars,
            "mode": "karucode temporary sessions",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_target_the_loopback_server() {
        let (settings, problem) = Settings::parse(None);
        assert!(problem.is_none());
        assert_eq!(settings.server, DEFAULT_SERVER);
        assert_eq!(settings.run_timeout_secs, DEFAULT_RUN_TIMEOUT_SECS);
        assert_eq!(settings.max_image_mb, DEFAULT_MAX_IMAGE_MB);
    }

    #[test]
    fn null_and_partial_settings_keep_defaults() {
        for raw in ["null", r#"{"run_timeout_secs":30}"#] {
            let (settings, problem) = Settings::parse(Some(raw));
            assert!(problem.is_none());
            assert_eq!(settings.server, DEFAULT_SERVER);
        }
    }

    #[test]
    fn malformed_settings_fall_back_loudly() {
        for raw in ["{broken", r#"{"run_timeout_secs":"slow"}"#] {
            let (settings, problem) = Settings::parse(Some(raw));
            assert_eq!(settings, Settings::default());
            assert!(problem.is_some());
        }
    }

    #[test]
    fn clamps_match_the_upload_ceiling() {
        let (settings, _) = Settings::parse(Some(
            r#"{"run_timeout_secs":9999,"max_image_mb":50,"server":"http://x//"}"#,
        ));
        assert_eq!(settings.run_timeout_secs, 600);
        assert_eq!(settings.max_image_mb, 8);
        assert_eq!(settings.server, "http://x");
    }

    #[test]
    fn requested_model_pairs_provider_and_model() {
        let (settings, _) = Settings::parse(Some(r#"{"model":"m"}"#));
        assert_eq!(settings.requested_model(), Some(("".into(), "m".into())));
        let (settings, _) = Settings::parse(Some(r#"{"model":"m","provider":"omniroute"}"#));
        assert_eq!(
            settings.requested_model(),
            Some(("omniroute".into(), "m".into()))
        );
        let (settings, _) = Settings::parse(Some(r#"{"provider":"p"}"#));
        assert_eq!(settings.requested_model(), None);
    }

    #[test]
    fn public_json_never_leaks_a_token() {
        let (settings, _) = Settings::parse(Some(r#"{"api_token":"sk-topsecret"}"#));
        let public = settings.public_json(None, None).to_string();
        assert!(!public.contains("sk-topsecret"));
        assert!(public.contains("\"token_set\":true"));
    }
}
