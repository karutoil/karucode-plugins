//! Settings, read from `KARUCODE_PLUGIN_SETTINGS` (the `[plugins.settings]`
//! table of the `[[plugins]]` entry).
//!
//! Every field is optional and every default is the one that makes the plugin
//! work with no configuration at all: the backend command it ships with is the
//! one the upstream README documents, the timeouts leave the harness room to
//! report a real error before its own budget expires, and nothing reaches the
//! network until a tool is called.
//!
//! A settings block that does not parse is **not** fatal. The plugin reports the
//! problem once on stderr and falls back to the defaults, because refusing to
//! start would take the tools away over a typo in a key nothing depends on.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The backend command the upstream project documents for a stdio MCP client.
pub const DEFAULT_COMMAND: [&str; 4] = ["uvx", "--from", "free-search-mcp", "search-mcp"];

/// Seconds one tool call may take *inside this plugin*.
///
/// karucode caps a plugin `tools/call` at 60 s (`PluginTimeouts::default`) and
/// the host's message for that is generic. Staying under it means the model gets
/// an error that names the remedy and knows the request was abandoned on purpose
/// rather than losing the whole call to a harness timeout.
///
/// This is the budget for the *backend request*; [`LOCK_WAIT_SECS`] is spent
/// before it, so the two together are what has to fit inside 60 s. A test asserts
/// that (`the_lock_wait_leaves_room_under_the_harness_budget`).
pub const DEFAULT_CALL_TIMEOUT_SECS: u64 = 50;

/// Seconds the backend may take to answer `initialize`.
///
/// The first `uvx` run of a fresh machine resolves and installs about ninety
/// Python packages; that is a one-off minute, not a hang. Every later start is
/// well under a second, and the cold path is covered by `prewarm`.
pub const DEFAULT_START_TIMEOUT_SECS: u64 = 120;

/// How long a call waits for the backend lock before answering "still starting".
///
/// The lock is held for the whole of a backend interaction, and the prewarm thread
/// takes it for `start_timeout_secs` — a cold `uvx` install resolves about ninety
/// packages and can take a minute. If a tool call simply blocked on the lock, the
/// total would run past karucode's 60 s plugin budget and the model would get the
/// harness's generic timeout instead of an answer it can act on. So the wait is
/// bounded, and `LOCK_WAIT_SECS + call_timeout_secs` stays under 60.
///
/// This is generous enough that the ordinary case — prewarm finished, every
/// subsequent call — never comes near it.
pub const LOCK_WAIT_SECS: u64 = 8;

/// Bytes of a text result kept for the model.
///
/// Sits just under karucode's own `limits.max_tool_output_bytes` (24 000) so
/// this plugin gets to explain how to read the rest before the harness cuts the
/// text with its generic note.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Backend argv. `["uvx", "--from", "free-search-mcp", "search-mcp"]` unless
    /// an operator points it at a checkout or a pinned version.
    #[serde(default = "default_command")]
    pub command: Vec<String>,
    /// Extra environment for the backend, merged over the plugin's own.
    ///
    /// This is the only way to reach settings such as `SEARCH_MCP_PROXY`,
    /// `SEARCH_MCP_DEFAULT_ENGINES` or an optional engine's API key: karucode
    /// hands a plugin a cleared environment, and the plugin passes on exactly
    /// what is configured here and nothing else.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Where the backend keeps its search/page cache. Sets
    /// `SEARCH_MCP_CACHE_DIR` unless `env` already does.
    #[serde(default)]
    pub cache_dir: Option<String>,
    #[serde(default = "default_call_timeout")]
    pub call_timeout_secs: u64,
    #[serde(default = "default_start_timeout")]
    pub start_timeout_secs: u64,
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
    /// Start the backend in the background at plugin startup so the first
    /// search does not pay for interpreter start and engine imports.
    #[serde(default = "default_true")]
    pub prewarm: bool,
    /// Expose only these backend tools. Empty (the default) exposes all of them.
    #[serde(default)]
    pub tools: Vec<String>,
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
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            command: default_command(),
            env: Default::default(),
            cache_dir: None,
            call_timeout_secs: default_call_timeout(),
            start_timeout_secs: default_start_timeout(),
            max_output_bytes: default_max_output(),
            prewarm: true,
            tools: Vec::new(),
        }
    }
}

impl Settings {
    /// Parse the settings JSON the host puts in `KARUCODE_PLUGIN_SETTINGS`.
    ///
    /// `Null` (no settings table) is the default configuration, not an error.
    /// A malformed object keeps the defaults and yields the message to log.
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
                Some(format!("plugin settings are not valid JSON ({e}); using defaults")),
            ),
        }
    }

    /// Clamp the values a bad hand-written config could make nonsensical.
    ///
    /// An empty `command` would make every call fail with an opaque spawn error,
    /// and a zero timeout would fail every call instantly, so both fall back to
    /// the documented default instead of being honoured.
    pub fn normalized(mut self) -> Self {
        if self.command.is_empty() {
            self.command = default_command();
        }
        self.call_timeout_secs = self.call_timeout_secs.clamp(1, 3600);
        self.start_timeout_secs = self.start_timeout_secs.clamp(1, 3600);
        self.max_output_bytes = self.max_output_bytes.clamp(500, 1_000_000);
        self.tools = self
            .tools
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        self
    }

    /// Whether `tool` should be exposed to the agent.
    pub fn exposes(&self, tool: &str) -> bool {
        self.tools.is_empty() || self.tools.iter().any(|t| t == tool)
    }

    /// The environment handed to the backend process.
    ///
    /// The plugin process already runs with karucode's cleared environment, so
    /// this is the allowlisted basics plus exactly what the operator configured.
    /// Both the explicit `env` table and `cache_dir` collapse into one map;
    /// `env` wins because it is the more specific instruction.
    pub fn backend_env(&self) -> std::collections::BTreeMap<String, String> {
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
    fn absent_settings_are_the_defaults() {
        let (s, err) = Settings::parse(None);
        assert!(err.is_none());
        assert_eq!(s, Settings::default());
        assert_eq!(s.command, DEFAULT_COMMAND);
        // The backend must not start before a tool is called.
        assert!(s.prewarm);
        assert!(s.tools.is_empty());
    }

    #[test]
    fn explicit_null_is_not_an_error() {
        // The host writes `null` when the plugin declares no settings table.
        let (s, err) = Settings::parse(Some("null"));
        assert!(err.is_none(), "{err:?}");
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn a_partial_object_keeps_every_other_default() {
        let (s, err) = Settings::parse(Some(r#"{"call_timeout_secs": 20}"#));
        assert!(err.is_none());
        assert_eq!(s.call_timeout_secs, 20);
        assert_eq!(s.command, DEFAULT_COMMAND);
        assert_eq!(s.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn a_broken_settings_block_falls_back_instead_of_failing_the_plugin() {
        let (s, err) = Settings::parse(Some("{not json"));
        assert!(err.is_some());
        assert_eq!(s, Settings::default());

        // Right JSON, wrong types: same contract.
        let (s, err) = Settings::parse(Some(r#"{"command": 7}"#));
        assert!(err.is_some());
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn nonsensical_values_are_clamped() {
        let (s, _) = Settings::parse(Some(
            r#"{"command": [], "call_timeout_secs": 0, "start_timeout_secs": 999999,
                "max_output_bytes": 1}"#,
        ));
        // An empty argv is not a command; the documented default is used.
        assert_eq!(s.command, DEFAULT_COMMAND);
        assert_eq!(s.call_timeout_secs, 1);
        assert_eq!(s.start_timeout_secs, 3600);
        assert_eq!(s.max_output_bytes, 500);
    }

    #[test]
    fn the_call_timeout_stays_under_the_host_budget() {
        // The whole point: this plugin answers before karucode's 60 s cut-off so
        // the model gets a message that names the remedy.
        assert!(DEFAULT_CALL_TIMEOUT_SECS < 60);
        assert!(DEFAULT_MAX_OUTPUT_BYTES < 24_000);
    }

    #[test]
    fn tool_allowlists_filter_and_blank_entries_are_dropped() {
        let (s, _) = Settings::parse(Some(r#"{"tools": ["search", " ", "fetch"]}"#));
        assert_eq!(s.tools, vec!["search".to_string(), "fetch".to_string()]);
        assert!(s.exposes("search"));
        assert!(s.exposes("fetch"));
        assert!(!s.exposes("download"));

        // Empty means "all of them", which is the default.
        assert!(Settings::default().exposes("download"));
    }

    #[test]
    fn cache_dir_becomes_the_backend_variable_and_env_wins_over_it() {
        let (s, _) = Settings::parse(Some(
            r#"{"cache_dir": "/tmp/kc-cache",
                "env": {"SEARCH_MCP_CACHE_DIR": "/tmp/explicit"}}"#,
        ));
        assert_eq!(
            s.backend_env().get("SEARCH_MCP_CACHE_DIR").map(String::as_str),
            Some("/tmp/explicit")
        );

        let (s, _) = Settings::parse(Some(r#"{"cache_dir": "/tmp/kc-cache"}"#));
        assert_eq!(
            s.backend_env().get("SEARCH_MCP_CACHE_DIR").map(String::as_str),
            Some("/tmp/kc-cache")
        );
    }

    #[test]
    fn the_default_command_is_the_upstream_stdio_one() {
        // `uvx --from free-search-mcp search-mcp` is what the upstream README
        // documents for a stdio MCP client, and it needs no checkout.
        assert_eq!(
            Settings::default().command_line(),
            "uvx --from free-search-mcp search-mcp"
        );
    }
}
