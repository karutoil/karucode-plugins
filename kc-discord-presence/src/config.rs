//! Settings and the one piece of state this plugin persists.
//!
//! Two layers, and the precedence is deliberate:
//!
//! 1. **The operator's manifest / `[[plugins]]` `settings` table** — what the
//!    host hands over in `KARUCODE_PLUGIN_SETTINGS`.
//! 2. **A state file under the karucode data dir** — what `discord_configure`
//!    wrote when the operator pasted a webhook URL into a chat. It wins, because
//!    the person who just pasted a URL expects it to take effect without editing
//!    a config file and reloading a plugin.
//!
//! The state file holds a secret (the webhook token is the whole credential), so
//! it is written `0600` and every surface that shows it goes through
//! [`Effective::redacted_webhook`].

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Discord's REST base, used when a webhook URL carries no versioned prefix.
pub const API_VERSION: &str = "v10";
/// Seconds between two edits of the live card. Discord allows about five
/// message edits per five seconds per channel, so this is deliberately slow.
pub const DEFAULT_REFRESH_SECS: u64 = 20;
/// Floor for `refresh_secs` when the board is on. Below this the card would
/// compete with the channel's own edit budget for no extra information.
pub const MIN_REFRESH_SECS: u64 = 10;
pub const MAX_REFRESH_SECS: u64 = 3600;
/// Notification kinds posted as their own message by default: the two that
/// actually want a human. `run_finished` is one setting away.
pub const DEFAULT_NOTIFY: &str = "needs_input,error";
pub const DEFAULT_USERNAME: &str = "karucode";
pub const DEFAULT_MAX_SESSIONS: usize = 3;
pub const MAX_SESSIONS: usize = 8;
pub const STATE_DIR: &str = "discord-presence";
pub const STATE_FILE: &str = "state.json";
/// Every notification kind karucode can emit.
pub const NOTIFY_KINDS: &[&str] = &["run_finished", "needs_input", "error", "info"];
/// The variable the host uses to hand a plugin its `settings` table.
pub const SETTINGS_ENV: &str = "KARUCODE_PLUGIN_SETTINGS";

fn default_refresh() -> u64 {
    DEFAULT_REFRESH_SECS
}
fn default_notify() -> String {
    DEFAULT_NOTIFY.to_string()
}
fn default_username() -> String {
    DEFAULT_USERNAME.to_string()
}
fn default_max_sessions() -> usize {
    DEFAULT_MAX_SESSIONS
}
fn default_true() -> bool {
    true
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

/// The operator's `settings` table, as the host hands it over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The Discord **Application ID** the presence is published under. This is
    /// what makes the profile say "Playing <app>"; it is a public number, not a
    /// secret. From <https://discord.com/developers/applications>.
    #[serde(default)]
    pub application_id: Option<String>,
    /// Turn the profile presence off without forgetting the Application ID.
    #[serde(default = "default_true")]
    pub presence: bool,
    /// A Rich Presence asset key (or external URL) for the large image.
    #[serde(default)]
    pub large_image: Option<String>,
    #[serde(default)]
    pub large_text: Option<String>,
    /// An explicit `discord-ipc-N` path, for a client that is not where
    /// discovery looks (a container, a flatpak under another runtime dir).
    #[serde(default)]
    pub presence_socket: Option<String>,
    /// The channel webhook to publish to. Optional: the presence needs none.
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Post into one thread of the webhook's channel instead of the channel.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Override karucode's data dir (the plugin cannot see `KARUCODE_DATA_DIR`;
    /// the host clears the environment down to an allowlist).
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Override the state file path outright.
    #[serde(default)]
    pub state_file: Option<String>,
    /// Discord REST base override (tests, or a proxy).
    #[serde(default)]
    pub api_base: Option<String>,
    /// Seconds between card edits; 0 turns the live card off.
    #[serde(default = "default_refresh", deserialize_with = "de_lenient_u64")]
    pub refresh_secs: u64,
    /// Comma list of notification kinds to post as their own message.
    #[serde(default = "default_notify")]
    pub notify: String,
    /// The webhook's display name in the channel.
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
    /// Include the last hour's run outcomes in the card.
    #[serde(default = "default_true")]
    pub include_recent: bool,
    /// How many sessions the card may name.
    #[serde(default = "default_max_sessions", deserialize_with = "de_lenient_usize")]
    pub max_sessions: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            application_id: None,
            presence: true,
            large_image: None,
            large_text: None,
            presence_socket: None,
            webhook_url: None,
            thread_id: None,
            data_dir: None,
            state_file: None,
            api_base: None,
            refresh_secs: DEFAULT_REFRESH_SECS,
            notify: DEFAULT_NOTIFY.to_string(),
            username: DEFAULT_USERNAME.to_string(),
            avatar_url: None,
            include_recent: true,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }
}

impl Settings {
    /// Parse the host's settings blob. A malformed blob is a reported problem,
    /// never a failed start: a plugin that will not boot is worse than one with
    /// defaults and a note.
    pub fn parse(raw: Option<&str>) -> (Self, Vec<String>) {
        let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
            return (Self::default(), Vec::new());
        };
        match serde_json::from_str::<Value>(raw) {
            Ok(Value::Null) => (Self::default(), Vec::new()),
            Ok(value) => match serde_json::from_value::<Self>(value) {
                Ok(settings) => (settings, Vec::new()),
                Err(e) => (
                    Self::default(),
                    vec![format!("plugin settings are invalid ({e}); using defaults")],
                ),
            },
            Err(e) => (
                Self::default(),
                vec![format!(
                    "plugin settings are not valid JSON ({e}); using defaults"
                )],
            ),
        }
    }

    fn normalized(mut self) -> Self {
        self.application_id = clean(self.application_id);
        self.presence_socket = clean(self.presence_socket);
        self.large_image = clean(self.large_image);
        self.large_text = clean(self.large_text);
        self.webhook_url = clean(self.webhook_url);
        self.thread_id = clean(self.thread_id);
        self.data_dir = clean(self.data_dir);
        self.state_file = clean(self.state_file);
        self.api_base = clean(self.api_base).map(|b| b.trim_end_matches('/').to_string());
        self.avatar_url = clean(self.avatar_url);
        self.username = self.username.trim().to_string();
        if self.username.is_empty() {
            self.username = DEFAULT_USERNAME.to_string();
        }
        self.max_sessions = self.max_sessions.clamp(1, MAX_SESSIONS);
        self
    }
}

/// The state file: what a chat-side `discord_configure` wrote.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Persisted {
    #[serde(default)]
    pub application_id: Option<String>,
    #[serde(default)]
    pub large_image: Option<String>,
    #[serde(default)]
    pub large_text: Option<String>,
    #[serde(default)]
    pub presence_socket: Option<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub board_message_id: Option<String>,
    #[serde(default)]
    pub board_channel_id: Option<String>,
    #[serde(default)]
    pub refresh_secs: Option<u64>,
    #[serde(default)]
    pub notify: Option<String>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

/// Settings merged with the state file: what the publisher actually obeys.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    /// The Application ID the presence is published under.
    pub application_id: Option<String>,
    /// Whether the profile presence is on at all.
    pub presence: bool,
    pub large_image: Option<String>,
    pub large_text: Option<String>,
    pub presence_socket: Option<String>,
    pub webhook_url: Option<String>,
    pub thread_id: Option<String>,
    pub api_base: Option<String>,
    pub refresh_secs: u64,
    pub notify: Vec<String>,
    pub notify_unknown: Vec<String>,
    pub username: String,
    pub avatar_url: Option<String>,
    pub include_recent: bool,
    pub max_sessions: usize,
    pub data_dir: PathBuf,
    pub state_file: PathBuf,
    pub problems: Vec<String>,
    /// The manifest settings this was built from, kept so a reload re-merges the
    /// state file without re-reading an environment that a test cannot set.
    settings: Settings,
    /// Problems that came from the manifest alone, so a reload keeps reporting
    /// them.
    settings_problems: Vec<String>,
}

impl Effective {
    /// Read `plugin/settings/get` merged over `KARUCODE_PLUGIN_SETTINGS`, and
    /// the state file. Never fails: every problem is collected so
    /// `discord_status` and the panel can report it.
    pub fn load() -> Self {
        let raw_env = std::env::var(SETTINGS_ENV).ok();
        let rpc = if crate::sdk::HostClient::global().is_live() {
            crate::sdk::settings()
        } else {
            BTreeMap::new()
        };
        if rpc.is_empty() {
            return Self::load_from(raw_env.as_deref());
        }
        let mut parse_problems = Vec::new();
        let env_val = match raw_env.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            None => Value::Null,
            Some(r) => match serde_json::from_str::<Value>(r) {
                Ok(v) => v,
                Err(e) => {
                    parse_problems.push(format!("plugin settings are not valid JSON ({e}); using defaults"));
                    Value::Null
                }
            },
        };
        let mut map = match env_val {
            Value::Object(obj) => obj,
            Value::Null => Map::new(),
            _ => {
                parse_problems.push("plugin settings are invalid (expected an object); using defaults".into());
                Map::new()
            }
        };
        for (k, v) in rpc {
            map.insert(k, v);
        }
        let merged = Value::Object(map).to_string();
        let mut effective = Self::load_from(Some(&merged));
        if !parse_problems.is_empty() {
            parse_problems.extend(effective.problems);
            effective.problems = parse_problems;
        }
        effective
    }

    pub fn load_from(raw: Option<&str>) -> Self {
        let (settings, problems) = Settings::parse(raw);
        let settings = settings.normalized();
        let mut problems = problems;
        let data_dir = match settings.data_dir.as_deref() {
            Some(dir) => PathBuf::from(dir),
            None => default_data_dir(&mut problems),
        };
        let state_file = match settings.state_file.as_deref() {
            Some(path) => PathBuf::from(path),
            None => data_dir.join(STATE_DIR).join(STATE_FILE),
        };
        Self::merge(settings, problems, data_dir, state_file)
    }

    /// Manifest settings + state file -> what the publisher obeys.
    fn merge(
        settings: Settings,
        settings_problems: Vec<String>,
        data_dir: PathBuf,
        state_file: PathBuf,
    ) -> Self {
        let mut problems = settings_problems.clone();
        let persisted = match Persisted::load(&state_file) {
            Ok(state) => state,
            Err(e) => {
                problems.push(e);
                Persisted::default()
            }
        };
        let application_id = match persisted.application_id.clone() {
            Some(id) => Some(id).filter(|i| !i.trim().is_empty()),
            None => settings.application_id.clone(),
        };
        let application_id = match application_id {
            Some(id) if valid_application_id(&id) => Some(id.trim().to_string()),
            Some(id) => {
                problems.push(format!(
                    "`{id}` is not a Discord Application ID (it is a number of 15-25 digits, from \
                     discord.com/developers/applications); the presence is off until it is fixed"
                ));
                None
            }
            None => None,
        };
        let large_image = persisted
            .large_image
            .clone()
            .filter(|v| !v.trim().is_empty())
            .or(settings.large_image.clone());
        let large_text = persisted
            .large_text
            .clone()
            .filter(|v| !v.trim().is_empty())
            .or(settings.large_text.clone());
        let presence_socket = persisted
            .presence_socket
            .clone()
            .filter(|v| !v.trim().is_empty())
            .or(settings.presence_socket.clone());
        let webhook_url = match persisted.webhook_url.clone() {
            // A persisted value is authoritative, even an empty one: that is how
            // a chat-side `discord_configure` clears a manifest's URL.
            Some(url) => Some(url).filter(|u| !u.trim().is_empty()),
            None => settings.webhook_url.clone(),
        };
        let thread_id = match persisted.thread_id.clone() {
            Some(thread) => Some(thread).filter(|t| !t.trim().is_empty()),
            None => settings.thread_id.clone(),
        };
        let refresh_secs = persisted.refresh_secs.unwrap_or(settings.refresh_secs);
        let notify_raw = persisted.notify.clone().unwrap_or(settings.notify.clone());
        let (notify, notify_unknown) = parse_notify(&notify_raw);
        if !notify_unknown.is_empty() {
            problems.push(format!(
                "unknown notification kind(s) {}: expected one of {}",
                notify_unknown.join(", "),
                NOTIFY_KINDS.join(", ")
            ));
        }
        let refresh_secs = if refresh_secs == 0 {
            0
        } else {
            refresh_secs.clamp(MIN_REFRESH_SECS, MAX_REFRESH_SECS)
        };
        Self {
            application_id,
            presence: settings.presence,
            large_image,
            large_text,
            presence_socket,
            webhook_url,
            thread_id,
            api_base: settings.api_base.clone(),
            refresh_secs,
            notify,
            notify_unknown,
            username: settings.username.clone(),
            avatar_url: settings.avatar_url.clone(),
            include_recent: settings.include_recent,
            max_sessions: settings.max_sessions,
            data_dir,
            state_file,
            problems,
            settings,
            settings_problems,
        }
    }

    /// Re-read the state file (after `discord_configure` wrote it), keeping the
    /// manifest settings this instance was built from.
    pub fn reload(&self) -> Self {
        Self::merge(
            self.settings.clone(),
            self.settings_problems.clone(),
            self.data_dir.clone(),
            self.state_file.clone(),
        )
    }

    /// The live card is on when there is somewhere to publish and a non-zero
    /// interval.
    pub fn board_enabled(&self) -> bool {
        self.webhook_url.is_some() && self.refresh_secs > 0
    }

    /// The profile presence is on when an Application ID is configured and the
    /// operator has not switched it off.
    pub fn presence_enabled(&self) -> bool {
        self.presence && self.application_id.is_some()
    }

    /// Anything at all to publish to.
    pub fn configured(&self) -> bool {
        self.presence_enabled() || self.webhook_url.is_some()
    }

    pub fn notify_enabled(&self, kind: &str) -> bool {
        self.notify.iter().any(|k| k == kind)
    }

    /// The webhook URL with its token replaced, for logs and panels.
    pub fn redacted_webhook(&self) -> Option<String> {
        self.webhook_url.as_deref().map(redact_webhook)
    }

    /// The state file's contents, or defaults when it is unreadable.
    pub fn persisted(&self) -> Persisted {
        Persisted::load(&self.state_file).unwrap_or_default()
    }

    pub fn save(&self, state: &Persisted) -> Result<(), String> {
        state.save(&self.state_file)
    }

    /// A redacted, serialisable view for tools and the panel.
    pub fn public_json(&self) -> Value {
        serde_json::json!({
            "configured": self.configured(),
            "presence": {
                "enabled": self.presence_enabled(),
                "application_id": self.application_id,
                "large_image": self.large_image,
                "large_text": self.large_text,
                "socket": self.presence_socket,
            },
            "webhook": self.redacted_webhook(),
            "thread_id": self.thread_id,
            "board": self.board_enabled(),
            "refresh_secs": self.refresh_secs,
            "notify": self.notify,
            "username": self.username,
            "include_recent": self.include_recent,
            "max_sessions": self.max_sessions,
            "data_dir": self.data_dir.display().to_string(),
            "state_file": self.state_file.display().to_string(),
            "problems": self.problems,
        })
    }
}

impl Persisted {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&raw).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))
    }

    /// Write the file atomically, `0600`, so the webhook token is not readable
    /// by other users on the box.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| format!("cannot serialise the state: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            file.write_all(body.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| {
            format!(
                "cannot move {} into place at {}: {e}",
                tmp.display(),
                path.display()
            )
        })
    }
}

/// A Discord Application ID is a snowflake: digits only. Anything else is a
/// client *secret* pasted by mistake, or a truncated copy.
pub fn valid_application_id(id: &str) -> bool {
    let id = id.trim();
    (15..=25).contains(&id.len()) && id.chars().all(|c| c.is_ascii_digit())
}

/// Replace a webhook's token with `***`, keeping the id so a person can tell
/// two webhooks apart.
pub fn redact_webhook(url: &str) -> String {
    let trimmed = url.trim();
    let (head, query) = match trimmed.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (trimmed, None),
    };
    let mut parts: Vec<&str> = head.split('/').collect();
    let mut redacted = false;
    if let Some(pos) = parts.iter().position(|p| *p == "webhooks") {
        if let Some(token) = parts.get_mut(pos + 2) {
            if !token.is_empty() {
                *token = "***";
                redacted = true;
            }
        }
    }
    let mut out = parts.join("/");
    if !redacted {
        // Not a shape we recognise: keep the scheme and host, hide the rest.
        out = match out.split_once("://") {
            Some((scheme, rest)) => match rest.split_once('/') {
                Some((host, _)) => format!("{scheme}://{host}/…"),
                None => out,
            },
            None => out,
        };
    }
    match query {
        Some(query) => format!("{out}?{query}"),
        None => out,
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `needs_input,error` -> kinds. `all` expands, `none` clears, anything else is
/// reported rather than silently dropped.
pub fn parse_notify(raw: &str) -> (Vec<String>, Vec<String>) {
    let mut kinds: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let name = part.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        if name == "none" {
            kinds.clear();
            continue;
        }
        if name == "all" {
            for kind in NOTIFY_KINDS {
                if !kinds.iter().any(|k| k == kind) {
                    kinds.push((*kind).to_string());
                }
            }
            continue;
        }
        if NOTIFY_KINDS.contains(&name.as_str()) {
            if !kinds.contains(&name) {
                kinds.push(name);
            }
        } else {
            unknown.push(name);
        }
    }
    (kinds, unknown)
}

/// `$HOME/.local/share/karucode`, the default the server uses.
fn default_data_dir(problems: &mut Vec<String>) -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) if !home.trim().is_empty() => PathBuf::from(home).join(".local/share/karucode"),
        _ => {
            problems.push(
                "HOME is not set, so karucode's data dir cannot be guessed; set `data_dir` in the \
                 plugin settings"
                    .to_string(),
            );
            PathBuf::from(".local/share/karucode")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane_and_do_not_publish_anything() {
        let settings = Settings::default();
        assert_eq!(settings.refresh_secs, DEFAULT_REFRESH_SECS);
        assert_eq!(settings.notify, DEFAULT_NOTIFY);
        assert!(settings.webhook_url.is_none());
        // A temp data dir: the default one is the machine's real karucode data
        // dir, and a state file there would leak into this test.
        let dir = tempdir();
        let effective = Effective::load_from(Some(&format!(
            r#"{{"data_dir":"{}"}}"#,
            dir.display()
        )));
        assert!(!effective.board_enabled(), "no webhook means no board");
        assert_eq!(effective.notify, vec!["needs_input", "error"]);
        assert!(effective.notify_unknown.is_empty());
    }

    #[test]
    fn malformed_settings_are_a_problem_not_a_failure() {
        let (settings, problems) = Settings::parse(Some("{not json"));
        assert_eq!(settings, Settings::default());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("not valid JSON"));

        let (settings, problems) = Settings::parse(Some(r#"{"refresh_secs":"soon"}"#));
        assert_eq!(settings, Settings::default());
        assert!(problems[0].contains("invalid"));
    }

    #[test]
    fn refresh_is_clamped_and_zero_disables_the_board() {
        let dir = tempdir();
        let raw = format!(
            r#"{{"webhook_url":"https://discord.com/api/webhooks/1/t","refresh_secs":1,"data_dir":"{}"}}"#,
            dir.display()
        );
        let effective = Effective::load_from(Some(&raw));
        assert_eq!(effective.refresh_secs, MIN_REFRESH_SECS);
        assert!(effective.board_enabled());

        let raw = format!(
            r#"{{"webhook_url":"https://discord.com/api/webhooks/1/t","refresh_secs":0,"data_dir":"{}"}}"#,
            dir.display()
        );
        let effective = Effective::load_from(Some(&raw));
        assert_eq!(effective.refresh_secs, 0);
        assert!(!effective.board_enabled());
    }

    #[test]
    fn notify_parses_all_none_and_reports_typos() {
        assert_eq!(
            parse_notify("needs_input, error"),
            (
                vec!["needs_input".to_string(), "error".to_string()],
                Vec::new()
            )
        );
        assert_eq!(parse_notify("all").0.len(), NOTIFY_KINDS.len());
        assert!(parse_notify("none").0.is_empty());
        assert!(parse_notify("").0.is_empty());
        let (kinds, unknown) = parse_notify("needs_input,need_input");
        assert_eq!(kinds, vec!["needs_input".to_string()]);
        assert_eq!(unknown, vec!["need_input".to_string()]);
    }

    #[test]
    fn the_state_file_wins_over_the_manifest_and_is_private() {
        let dir = tempdir();
        let state_path = dir.join("discord-presence/state.json");
        let raw = format!(
            r#"{{"webhook_url":"https://discord.com/api/webhooks/1/manifest","data_dir":"{}"}}"#,
            dir.display()
        );
        let effective = Effective::load_from(Some(&raw));
        assert_eq!(
            effective.webhook_url.as_deref(),
            Some("https://discord.com/api/webhooks/1/manifest")
        );

        Persisted {
            webhook_url: Some("https://discord.com/api/webhooks/2/pasted".to_string()),
            board_message_id: Some("m_1".to_string()),
            ..Default::default()
        }
        .save(&state_path)
        .unwrap();

        let effective = Effective::load_from(Some(&raw));
        assert_eq!(
            effective.webhook_url.as_deref(),
            Some("https://discord.com/api/webhooks/2/pasted")
        );
        assert_eq!(effective.persisted().board_message_id.as_deref(), Some("m_1"));

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the webhook token must not be world readable");
    }

    #[test]
    fn redaction_keeps_the_id_and_hides_the_token() {
        assert_eq!(
            redact_webhook("https://discord.com/api/webhooks/123/abcDEF"),
            "https://discord.com/api/webhooks/123/***"
        );
        assert_eq!(
            redact_webhook("https://discord.com/api/webhooks/123/abcDEF?thread_id=9"),
            "https://discord.com/api/webhooks/123/***?thread_id=9"
        );
        assert_eq!(
            redact_webhook("https://example.test/hook/secret"),
            "https://example.test/…"
        );
    }

    #[test]
    fn an_unknown_notify_kind_is_reported_once() {
        let dir = tempdir();
        let raw = format!(
            r#"{{"data_dir":"{}","notify":"needs_input,nope"}}"#,
            dir.display()
        );
        let effective = Effective::load_from(Some(&raw));
        assert_eq!(effective.notify, vec!["needs_input".to_string()]);
        assert_eq!(effective.notify_unknown, vec!["nope".to_string()]);
        assert!(effective.problems.iter().any(|p| p.contains("nope")));
    }

    #[test]
    fn an_application_id_is_validated_as_a_snowflake() {
        assert!(valid_application_id("123456789012345678"));
        assert!(valid_application_id(" 1234567890123456789 "));
        assert!(!valid_application_id("abc"));
        assert!(!valid_application_id("1234"), "too short");
        // A client secret pasted by mistake is the common failure.
        assert!(!valid_application_id("aBcD1234efGh5678iJkL9012mNoP3456"));
        assert!(!valid_application_id(""));
    }

    #[test]
    fn a_presence_needs_only_an_application_id_and_a_bad_one_is_reported() {
        let dir = tempdir();
        let raw = format!(
            r#"{{"application_id":"123456789012345678","data_dir":"{}"}}"#,
            dir.display()
        );
        let effective = Effective::load_from(Some(&raw));
        assert!(effective.presence_enabled());
        assert!(effective.configured());
        assert!(!effective.board_enabled(), "a presence alone needs no webhook");

        let raw = format!(
            r#"{{"application_id":"not-a-number","data_dir":"{}"}}"#,
            dir.display()
        );
        let effective = Effective::load_from(Some(&raw));
        assert!(!effective.presence_enabled());
        assert!(effective.problems.iter().any(|p| p.contains("Application ID")));
    }

    #[test]
    fn the_presence_can_be_switched_off_without_forgetting_the_id() {
        let dir = tempdir();
        let raw = format!(
            r#"{{"application_id":"123456789012345678","presence":false,"data_dir":"{}"}}"#,
            dir.display()
        );
        let effective = Effective::load_from(Some(&raw));
        assert!(!effective.presence_enabled());
        assert_eq!(
            effective.application_id.as_deref(),
            Some("123456789012345678")
        );
    }

    #[test]
    fn the_state_file_carries_the_application_id_too() {
        let dir = tempdir();
        let state_path = dir.join("discord-presence/state.json");
        Persisted {
            application_id: Some("987654321098765432".to_string()),
            large_image: Some("karucode".to_string()),
            ..Default::default()
        }
        .save(&state_path)
        .unwrap();
        let raw = format!(r#"{{"data_dir":"{}"}}"#, dir.display());
        let effective = Effective::load_from(Some(&raw));
        assert_eq!(
            effective.application_id.as_deref(),
            Some("987654321098765432")
        );
        assert_eq!(effective.large_image.as_deref(), Some("karucode"));
        assert!(effective.presence_enabled());
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "kc-discord-presence-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
