//! Process settings. Research budgets belong to each job; these settings describe
//! the supervised free-search-mcp backend shared by all jobs in this process.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const DEFAULT_COMMAND: [&str; 4] = ["uvx", "--from", "free-search-mcp", "search-mcp"];
pub const DEFAULT_CALL_TIMEOUT_SECS: u64 = 50;
pub const DEFAULT_START_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_command")]
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cache_dir: Option<String>,
    #[serde(default = "default_call_timeout")]
    pub call_timeout_secs: u64,
    #[serde(default = "default_start_timeout")]
    pub start_timeout_secs: u64,
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
    /// Each active job owns one backend process. Two keeps memory/process use
    /// bounded while still allowing independent sessions to research concurrently.
    #[serde(default = "default_max_active_jobs")]
    pub max_active_jobs: usize,
}

fn default_command() -> Vec<String> {
    DEFAULT_COMMAND.iter().map(|s| s.to_string()).collect()
}
fn default_call_timeout() -> u64 {
    DEFAULT_CALL_TIMEOUT_SECS
}
fn default_start_timeout() -> u64 {
    DEFAULT_START_TIMEOUT_SECS
}
fn default_max_output() -> usize {
    DEFAULT_MAX_OUTPUT_BYTES
}
fn default_max_active_jobs() -> usize {
    2
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            command: default_command(),
            env: BTreeMap::new(),
            cache_dir: None,
            call_timeout_secs: default_call_timeout(),
            start_timeout_secs: default_start_timeout(),
            max_output_bytes: default_max_output(),
            max_active_jobs: default_max_active_jobs(),
        }
    }
}

impl Settings {
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
        if self.command.is_empty() {
            self.command = default_command();
        }
        // A request runs in a worker, not under the host's 60s tool-call budget,
        // but bounding every backend interaction still makes cancellation and a
        // dead backend recoverable.
        self.call_timeout_secs = self.call_timeout_secs.clamp(1, 300);
        self.start_timeout_secs = self.start_timeout_secs.clamp(1, 300);
        self.max_output_bytes = self.max_output_bytes.clamp(1_000, 20_000);
        self.max_active_jobs = self.max_active_jobs.clamp(1, 4);
        self
    }

    pub fn backend_env(&self) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        if let Some(dir) = &self.cache_dir {
            env.entry("SEARCH_MCP_CACHE_DIR".into())
                .or_insert_with(|| dir.clone());
        }
        env
    }

    pub fn command_line(&self) -> String {
        self.command.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_need_no_configuration() {
        let (settings, problem) = Settings::parse(None);
        assert!(problem.is_none());
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.command, DEFAULT_COMMAND);
        assert_eq!(settings.max_active_jobs, 2);
    }

    #[test]
    fn partial_and_null_settings_keep_defaults() {
        for raw in ["null", r#"{"call_timeout_secs":20}"#] {
            let (settings, problem) = Settings::parse(Some(raw));
            assert!(problem.is_none());
            assert_eq!(settings.command, DEFAULT_COMMAND);
            assert_eq!(settings.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        }
    }

    #[test]
    fn malformed_settings_fall_back_loudly() {
        for raw in ["{broken", r#"{"command":false}"#] {
            let (settings, problem) = Settings::parse(Some(raw));
            assert_eq!(settings, Settings::default());
            assert!(problem.is_some());
        }
    }

    #[test]
    fn limits_are_bounded_and_cache_env_is_specific() {
        let (settings, _) = Settings::parse(Some(
            r#"{"call_timeout_secs":0,"max_active_jobs":100,"cache_dir":"/tmp/cache"}"#,
        ));
        assert_eq!(settings.call_timeout_secs, 1);
        assert_eq!(settings.max_active_jobs, 4);
        assert_eq!(
            settings.backend_env().get("SEARCH_MCP_CACHE_DIR").unwrap(),
            "/tmp/cache"
        );
    }
}
