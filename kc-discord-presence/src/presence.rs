//! Discord Rich Presence over the local client's IPC socket.
//!
//! This is the surface the plugin exists for: "Playing <app>" on the operator's
//! Discord profile, with the project and the session on the two detail lines and
//! an elapsed timer that ticks while a run is going.
//!
//! It needs **no bot, no token and no network**. The Discord desktop client
//! listens on a Unix socket in the runtime directory (`discord-ipc-0` … `-9`), and
//! this module speaks its framing: a 4-byte little-endian opcode, a 4-byte
//! little-endian payload length, then JSON. The only thing the operator supplies
//! is an **Application ID** - a public number from
//! <https://discord.com/developers/applications>, not a secret - because that is
//! what gives the presence its name and icon.
//!
//! What Discord does and does not accept is enforced here rather than hoped for:
//! `details`/`state`/`large_text` are capped at **128 bytes** (bytes, not
//! characters, so a multi-byte title is cut on a character boundary), a field
//! shorter than two characters is dropped, and updates are spaced because the
//! client silently drops them when they arrive too fast.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Effective;
use crate::karucode::Snapshot;

pub const OP_HANDSHAKE: u32 = 0;
pub const OP_FRAME: u32 = 1;
pub const OP_CLOSE: u32 = 2;
pub const OP_PING: u32 = 3;
pub const OP_PONG: u32 = 4;

/// Discord's ceiling for `details`, `state` and `large_text`, in UTF-8 bytes.
pub const MAX_FIELD_BYTES: usize = 128;
/// A frame larger than this is a broken client, not a big message.
pub const MAX_FRAME_BYTES: usize = 1 << 20;
/// A wedged client must not hold the publisher for longer than this.
pub const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// Discord drops `SET_ACTIVITY` calls that arrive faster than this.
pub const MIN_UPDATE_GAP_SECS: u64 = 15;
/// How long to wait before looking for a client again after it went away.
pub const RECONNECT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceError {
    /// No `discord-ipc-*` socket anywhere we look: the desktop client is not
    /// running (or it is running as another user / in another namespace).
    NoClient(String),
    Io(String),
    Protocol(String),
    /// The client answered with an `ERROR` frame - almost always a bad
    /// Application ID.
    Rejected { code: i64, message: String },
}

impl PresenceError {
    pub fn user_message(&self) -> String {
        match self {
            PresenceError::NoClient(detail) => format!(
                "no Discord client is running on this machine ({detail}); start the Discord \
                 desktop app and the presence attaches within {}s",
                RECONNECT_SECS
            ),
            PresenceError::Io(detail) => format!("the Discord client connection failed: {detail}"),
            PresenceError::Protocol(detail) => {
                format!("the Discord client answered something unexpected: {detail}")
            }
            PresenceError::Rejected { code, message } => format!(
                "Discord rejected the presence (code {code}): {message} - check the Application ID \
                 at discord.com/developers/applications"
            ),
        }
    }
}

impl std::fmt::Display for PresenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.user_message())
    }
}

impl std::error::Error for PresenceError {}

/// One rich presence: the two lines Discord shows, an optional elapsed timer and
/// an optional asset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RichActivity {
    pub details: String,
    pub state: String,
    /// Milliseconds since the epoch; Discord renders a ticking elapsed time.
    pub start: Option<i64>,
    pub large_image: Option<String>,
    pub large_text: Option<String>,
}

impl RichActivity {
    /// The `activity` object Discord accepts. Fields Discord would refuse are
    /// left out rather than sent and rejected.
    pub fn to_json(&self) -> Value {
        let mut activity = serde_json::Map::new();
        let details = clamp_bytes(&self.details, MAX_FIELD_BYTES);
        if details.chars().count() >= 2 {
            activity.insert("details".into(), json!(details));
        }
        let state = clamp_bytes(&self.state, MAX_FIELD_BYTES);
        if state.chars().count() >= 2 {
            activity.insert("state".into(), json!(state));
        }
        if let Some(start) = self.start {
            activity.insert("timestamps".into(), json!({ "start": start }));
        }
        let mut assets = serde_json::Map::new();
        if let Some(image) = self.large_image.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
            assets.insert("large_image".into(), json!(clamp_bytes(image, MAX_FIELD_BYTES)));
        }
        if let Some(text) = self.large_text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            assets.insert("large_text".into(), json!(clamp_bytes(text, MAX_FIELD_BYTES)));
        }
        if !assets.is_empty() {
            activity.insert("assets".into(), Value::Object(assets));
        }
        Value::Object(activity)
    }

    /// What changed between two presences, so the publisher only talks to
    /// Discord when something is actually different.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.details,
            self.state,
            self.start.map(|s| s.to_string()).unwrap_or_default(),
            self.large_image.clone().unwrap_or_default(),
            self.large_text.clone().unwrap_or_default()
        )
    }
}

/// The presence built from karucode's state: project on the first line, session
/// (or what is blocking it) on the second, elapsed from the run's start.
pub fn activity_for(snap: &Snapshot, cfg: &Effective) -> RichActivity {
    let asset = |activity: RichActivity| RichActivity {
        large_image: cfg.large_image.clone(),
        large_text: cfg.large_text.clone(),
        ..activity
    };
    if !snap.readable {
        return asset(RichActivity {
            details: "no data".to_string(),
            state: snap
                .problems
                .first()
                .cloned()
                .unwrap_or_else(|| "karucode's database could not be read".to_string()),
            ..Default::default()
        });
    }
    let Some((session, run)) = snap.focus() else {
        return asset(RichActivity {
            details: "idle".to_string(),
            state: "no sessions yet".to_string(),
            ..Default::default()
        });
    };
    let project = session
        .project
        .clone()
        .or_else(|| basename(&session.workspace))
        .unwrap_or_else(|| "no project".to_string());
    if run.is_none() {
        return asset(RichActivity {
            details: "idle".to_string(),
            state: format!("last: {}", session.title),
            ..Default::default()
        });
    }
    let state = if snap.approvals.pending > 0 {
        let what = if snap.approvals.newest_title.trim().is_empty() {
            format!("{} approval(s) pending", snap.approvals.pending)
        } else {
            snap.approvals.newest_title.clone()
        };
        format!("⏸ {what}")
    } else if snap.agents_running > 0 {
        format!(
            "{} · {} agent{}",
            session.title,
            snap.agents_running,
            if snap.agents_running == 1 { "" } else { "s" }
        )
    } else {
        session.title.clone()
    };
    asset(RichActivity {
        details: project,
        state,
        start: run.map(|run| run.started_at),
        ..Default::default()
    })
}

/// A live connection to one Discord client.
#[derive(Debug)]
pub struct Presence {
    pub app_id: String,
    /// An explicit socket path, for a client that is not where discovery looks.
    pub socket_override: Option<PathBuf>,
    stream: Option<UnixStream>,
    path: Option<PathBuf>,
    connected_at: Option<i64>,
    last_sent_at: Option<i64>,
    last_fingerprint: Option<String>,
    sent: u64,
    error: Option<String>,
    error_at: Option<i64>,
    nonce: u64,
}

impl Presence {
    pub fn new(app_id: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            socket_override: None,
            stream: None,
            path: None,
            connected_at: None,
            last_sent_at: None,
            last_fingerprint: None,
            sent: 0,
            error: None,
            error_at: None,
            nonce: 0,
        }
    }

    pub fn with_socket(mut self, path: Option<PathBuf>) -> Self {
        self.socket_override = path;
        self
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// How many `SET_ACTIVITY` frames this connection has delivered.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    pub fn socket_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Connect (if needed) and hand the client the presence.
    pub fn set_activity(&mut self, activity: &RichActivity, now: i64) -> Result<(), PresenceError> {
        if self.stream.is_none() {
            self.connect(now)?;
        }
        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id(), "activity": activity.to_json() },
            "nonce": self.next_nonce(),
        });
        let result = self.exchange(&payload);
        match result {
            Ok(()) => {
                self.last_sent_at = Some(now);
                self.last_fingerprint = Some(activity.fingerprint());
                self.sent += 1;
                self.error = None;
                Ok(())
            }
            Err(error) => {
                self.record(&error, now);
                Err(error)
            }
        }
    }

    /// Remove the presence, so a stopped plugin does not leave "Playing" forever.
    pub fn clear(&mut self, now: i64) -> Result<(), PresenceError> {
        if self.stream.is_none() {
            return Ok(());
        }
        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id(), "activity": Value::Null },
            "nonce": self.next_nonce(),
        });
        let result = self.exchange(&payload);
        // Say goodbye the way the protocol expects, then forget the socket.
        if let Some(stream) = self.stream.as_mut() {
            let _ = write_frame(stream, OP_CLOSE, &json!({}));
        }
        self.stream = None;
        self.last_fingerprint = None;
        let _ = now;
        result
    }

    /// Whether an update is due: either the presence changed, or the client went
    /// away and it is time to look for it again.
    pub fn wants_update(&self, activity: &RichActivity, now: i64) -> bool {
        if self.stream.is_none() {
            return true;
        }
        if self.last_fingerprint.as_deref() != Some(activity.fingerprint().as_str()) {
            return match self.last_sent_at {
                Some(at) => now.saturating_sub(at) >= (MIN_UPDATE_GAP_SECS as i64) * 1000,
                None => true,
            };
        }
        false
    }

    /// Connect to the client, handshaking with the Application ID.
    pub fn connect(&mut self, now: i64) -> Result<(), PresenceError> {
        let path = discover(self.socket_override.as_deref()).ok_or_else(|| {
            let error = PresenceError::NoClient(format!(
                "looked for discord-ipc-0..9 in {}",
                candidate_dirs()
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            self.record(&error, now);
            error
        })?;
        let mut stream = UnixStream::connect(&path).map_err(|e| {
            let error = PresenceError::Io(format!("{}: {e}", path.display()));
            self.record(&error, now);
            error
        })?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .and_then(|_| stream.set_write_timeout(Some(IO_TIMEOUT)))
            .map_err(|e| PresenceError::Io(format!("cannot set a socket timeout: {e}")))?;
        write_frame(
            &mut stream,
            OP_HANDSHAKE,
            &json!({ "v": 1, "client_id": self.app_id }),
        )?;
        let (opcode, frame) = read_frame(&mut stream)?;
        if frame.get("evt").and_then(Value::as_str) == Some("ERROR") {
            let code = frame
                .get("data")
                .and_then(|d| d.get("code"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let message = frame
                .get("data")
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("no message")
                .to_string();
            let error = PresenceError::Rejected { code, message };
            self.record(&error, now);
            return Err(error);
        }
        if opcode != OP_FRAME {
            let error = PresenceError::Protocol(format!("handshake answered with opcode {opcode}"));
            self.record(&error, now);
            return Err(error);
        }
        self.stream = Some(stream);
        self.path = Some(path);
        self.connected_at = Some(now);
        self.error = None;
        self.error_at = None;
        Ok(())
    }

    fn exchange(&mut self, payload: &Value) -> Result<(), PresenceError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(PresenceError::NoClient("not connected".to_string()));
        };
        write_frame(stream, OP_FRAME, payload)?;
        loop {
            let (opcode, reply) = read_frame(stream)?;
            // The client may ping at any time; answering keeps our read stream
            // aligned with the frame we are actually waiting for.
            if opcode == OP_PING {
                write_frame(stream, OP_PONG, &json!({}))?;
                continue;
            }
            if reply.get("evt").and_then(Value::as_str) == Some("ERROR") {
                let code = reply
                    .get("data")
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let message = reply
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("no message")
                    .to_string();
                return Err(PresenceError::Rejected { code, message });
            }
            return Ok(());
        }
    }

    fn next_nonce(&mut self) -> String {
        self.nonce += 1;
        format!("{}-{}", std::process::id(), self.nonce)
    }

    fn record(&mut self, error: &PresenceError, now: i64) {
        // A broken pipe is not a state worth keeping: drop it so the next
        // attempt reconnects instead of writing into a dead socket.
        if !matches!(error, PresenceError::Rejected { .. }) {
            self.stream = None;
            self.path = None;
            self.connected_at = None;
        }
        self.error = Some(error.user_message());
        self.error_at = Some(now);
    }

    pub fn status_json(&self) -> Value {
        json!({
            "configured": !self.app_id.is_empty(),
            "application_id": self.app_id,
            "connected": self.is_connected(),
            "socket": self.path.as_ref().map(|p| p.display().to_string()),
            "connected_at": self.connected_at,
            "last_sent_at": self.last_sent_at,
            "sent": self.sent,
            "last_fingerprint": self.last_fingerprint,
            "error": self.error,
            "error_at": self.error_at,
            "candidates": candidate_dirs().iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
        })
    }
}

/// The socket paths a Discord client may be listening on, in the order worth
/// trying. The plugin's environment is cleared by the host, so
/// `XDG_RUNTIME_DIR` is usually absent and `/run/user/<uid>` is the real answer.
pub fn candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for dir in candidate_dirs() {
        for index in 0..10 {
            let path = dir.join(format!("discord-ipc-{index}"));
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.trim().is_empty() {
            let dir = PathBuf::from(dir);
            // Flatpak and snap keep the socket in their own subdirectory.
            push(dir.join("app/com.discordapp.Discord"));
            push(dir.join("app/com.discordapp.DiscordCanary"));
            push(dir.join("snap.discord"));
            push(dir.join("snap.discord-canary"));
            push(dir);
        }
    }
    if let Some(uid) = current_uid() {
        let run = PathBuf::from(format!("/run/user/{uid}"));
        push(run.join("app/com.discordapp.Discord"));
        push(run.join("app/com.discordapp.DiscordCanary"));
        push(run.join("snap.discord"));
        push(run.join("snap.discord-canary"));
        push(run);
    }
    if let Ok(dir) = std::env::var("TMPDIR") {
        if !dir.trim().is_empty() {
            push(PathBuf::from(dir));
        }
    }
    push(PathBuf::from("/tmp"));
    dirs
}

/// The first socket that exists and can be connected to.
pub fn discover(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return if path.exists() { Some(path.to_path_buf()) } else { None };
    }
    candidate_paths().into_iter().find(|path| path.exists())
}

/// Our own uid without a `libc` dependency: `/proc/self` is owned by us.
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

/// Cut `text` to at most `max` **bytes** on a character boundary, with an
/// ellipsis. Discord's limits are bytes, so a 128-character title of emoji is
/// three times too long.
pub fn clamp_bytes(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.len() <= max {
        return text.to_string();
    }
    if max <= 3 {
        return "…".to_string();
    }
    let mut end = max - 3;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn write_frame(stream: &mut UnixStream, opcode: u32, payload: &Value) -> Result<(), PresenceError> {
    let body = serde_json::to_vec(payload)
        .map_err(|e| PresenceError::Protocol(format!("cannot encode a frame: {e}")))?;
    let mut buffer = Vec::with_capacity(8 + body.len());
    buffer.extend_from_slice(&opcode.to_le_bytes());
    buffer.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&body);
    stream
        .write_all(&buffer)
        .and_then(|_| stream.flush())
        .map_err(|e| PresenceError::Io(e.to_string()))
}

fn read_frame(stream: &mut UnixStream) -> Result<(u32, Value), PresenceError> {
    let mut head = [0u8; 8];
    stream
        .read_exact(&mut head)
        .map_err(|e| PresenceError::Io(e.to_string()))?;
    let opcode = u32::from_le_bytes(head[0..4].try_into().expect("4 bytes"));
    let length = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes")) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(PresenceError::Protocol(format!(
            "frame of {length} bytes is larger than the {MAX_FRAME_BYTES}-byte cap"
        )));
    }
    let mut body = vec![0u8; length];
    stream
        .read_exact(&mut body)
        .map_err(|e| PresenceError::Io(e.to_string()))?;
    let value = serde_json::from_slice(&body)
        .map_err(|e| PresenceError::Protocol(format!("frame body is not JSON: {e}")))?;
    Ok((opcode, value))
}

fn basename(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
#[allow(clippy::duplicate_mod)]
#[path = "../tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use super::support::FakeDiscordIpc;
    use super::*;
    use crate::karucode::{ApprovalRef, RunRef, SessionRef};
    use serde_json::json;

    fn cfg() -> Effective {
        Effective::load_from(Some(r#"{"application_id":"123456789012345678"}"#))
    }

    fn session(id: &str, title: &str) -> SessionRef {
        SessionRef {
            id: id.to_string(),
            title: title.to_string(),
            project: Some("karucode-plugins".to_string()),
            workspace: "/home/karutoil/karucode-plugins".to_string(),
            updated_at: 1_000,
            ..Default::default()
        }
    }

    fn running() -> Snapshot {
        Snapshot {
            readable: true,
            running: vec![RunRef {
                run_id: "r_1".to_string(),
                started_at: 9_000,
                prompt: "build the discord plugin".to_string(),
                session: session("s_live", "build the discord plugin"),
                ..Default::default()
            }],
            recent: vec![session("s_live", "build the discord plugin")],
            ..Default::default()
        }
    }

    #[test]
    fn a_running_session_becomes_the_project_and_the_session_with_a_timer() {
        let activity = activity_for(&running(), &cfg());
        assert_eq!(activity.details, "karucode-plugins");
        assert_eq!(activity.state, "build the discord plugin");
        assert_eq!(activity.start, Some(9_000));
        let json = activity.to_json();
        assert_eq!(json["timestamps"]["start"], 9_000);
        assert!(json.get("assets").is_none(), "no asset configured, no asset sent");
    }

    #[test]
    fn an_approval_replaces_the_session_line_because_it_is_what_needs_you() {
        let mut snap = running();
        snap.approvals = ApprovalRef {
            pending: 1,
            newest_title: "write_file docs/plan.md".to_string(),
            newest_session: "s_live".to_string(),
        };
        let activity = activity_for(&snap, &cfg());
        assert!(activity.state.starts_with('⏸'), "{}", activity.state);
        assert!(activity.state.contains("write_file docs/plan.md"));
        assert_eq!(activity.details, "karucode-plugins");
    }

    #[test]
    fn agents_are_named_only_when_they_are_running() {
        let mut snap = running();
        snap.agents_running = 2;
        let activity = activity_for(&snap, &cfg());
        assert!(activity.state.contains("2 agents"), "{}", activity.state);
    }

    #[test]
    fn an_idle_presence_says_idle_rather_than_claiming_the_last_session() {
        let snap = Snapshot {
            readable: true,
            recent: vec![session("s_old", "older session")],
            ..Default::default()
        };
        let activity = activity_for(&snap, &cfg());
        assert_eq!(activity.details, "idle");
        assert_eq!(activity.state, "last: older session");
        assert_eq!(activity.start, None, "an idle presence must not run a timer");
    }

    #[test]
    fn an_unreadable_database_is_visible_on_the_profile() {
        let snap = Snapshot {
            readable: false,
            problems: vec!["karucode's database is not at /x".to_string()],
            ..Default::default()
        };
        let activity = activity_for(&snap, &cfg());
        assert_eq!(activity.details, "no data");
        assert!(activity.state.contains("is not at"));
    }

    #[test]
    fn discord_limits_are_bytes_and_never_split_a_character() {
        let emoji = "🎧".repeat(100); // 4 bytes each: 400 bytes
        let cut = clamp_bytes(&emoji, MAX_FIELD_BYTES);
        assert!(cut.len() <= MAX_FIELD_BYTES, "{} bytes", cut.len());
        assert!(cut.is_char_boundary(cut.len()));
        assert!(cut.ends_with('…'));

        let accented = "é".repeat(200);
        let cut = clamp_bytes(&accented, MAX_FIELD_BYTES);
        assert!(cut.len() <= MAX_FIELD_BYTES);
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());

        assert_eq!(clamp_bytes("short", MAX_FIELD_BYTES), "short");
        assert_eq!(clamp_bytes("  padded  ", MAX_FIELD_BYTES), "padded");
    }

    #[test]
    fn a_one_character_field_is_dropped_rather_than_rejected() {
        let activity = RichActivity {
            details: "x".to_string(),
            state: "ok".to_string(),
            ..Default::default()
        };
        let json = activity.to_json();
        assert!(json.get("details").is_none());
        assert_eq!(json["state"], "ok");
    }

    #[test]
    fn an_asset_is_sent_only_when_one_is_configured() {
        let mut cfg = cfg();
        cfg.large_image = Some("karucode".to_string());
        cfg.large_text = Some("karucode".to_string());
        let activity = activity_for(&running(), &cfg);
        let json = activity.to_json();
        assert_eq!(json["assets"]["large_image"], "karucode");
        assert_eq!(json["assets"]["large_text"], "karucode");
    }

    #[test]
    fn the_fingerprint_changes_when_the_presence_does() {
        let a = activity_for(&running(), &cfg());
        let mut snap = running();
        snap.running[0].session.title = "something else".to_string();
        let b = activity_for(&snap, &cfg());
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), activity_for(&running(), &cfg()).fingerprint());
    }

    #[test]
    fn discovery_looks_where_discord_actually_listens() {
        let paths = candidate_paths();
        let rendered: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert!(rendered.iter().any(|p| p.ends_with("/discord-ipc-0")));
        assert!(rendered.iter().any(|p| p.contains("/run/user/")));
        assert!(rendered.iter().any(|p| p.contains("com.discordapp.Discord")));
        assert!(rendered.iter().any(|p| p.contains("snap.discord")));
        // No duplicates: the first socket that exists wins deterministically.
        let mut sorted = rendered.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), rendered.len());
    }

    #[test]
    fn an_explicit_socket_that_is_missing_is_not_a_connection() {
        assert!(discover(Some(Path::new("/nonexistent/discord-ipc-0"))).is_none());
    }

    #[test]
    fn a_handshake_and_an_activity_reach_the_client() {
        let fake = FakeDiscordIpc::start();
        let mut presence = Presence::new("123456789012345678")
            .with_socket(Some(fake.path().to_path_buf()));
        let activity = activity_for(&running(), &cfg());
        presence.set_activity(&activity, 10_000).expect("set activity");
        assert!(presence.is_connected());
        assert_eq!(presence.sent, 1);

        let frames = fake.frames();
        assert_eq!(frames[0].opcode, OP_HANDSHAKE);
        assert_eq!(frames[0].body["client_id"], "123456789012345678");
        assert_eq!(frames[0].body["v"], 1);
        assert_eq!(frames[1].body["cmd"], "SET_ACTIVITY");
        assert_eq!(frames[1].body["args"]["activity"]["details"], "karucode-plugins");
        assert_eq!(
            frames[1].body["args"]["activity"]["state"],
            "build the discord plugin"
        );
        assert!(frames[1].body["args"]["pid"].as_u64().unwrap() > 0);
        assert!(!frames[1].body["nonce"].as_str().unwrap().is_empty());
    }

    #[test]
    fn a_rejected_application_id_is_reported_in_discords_words() {
        let fake = FakeDiscordIpc::start();
        fake.reject_handshake(4000, "Invalid Client ID");
        let mut presence =
            Presence::new("1").with_socket(Some(fake.path().to_path_buf()));
        let error = presence
            .set_activity(&activity_for(&running(), &cfg()), 10_000)
            .expect_err("the client rejected the id");
        assert!(matches!(error, PresenceError::Rejected { code: 4000, .. }));
        assert!(error.user_message().contains("Invalid Client ID"));
        assert!(error.user_message().contains("Application ID"));
        assert!(!presence.is_connected());
    }

    #[test]
    fn a_dead_socket_is_dropped_and_the_next_call_reconnects() {
        let fake = FakeDiscordIpc::start();
        let mut presence =
            Presence::new("123456789012345678").with_socket(Some(fake.path().to_path_buf()));
        presence
            .set_activity(&activity_for(&running(), &cfg()), 10_000)
            .expect("first");
        fake.drop_connections();
        // The next write fails; the presence must forget the socket...
        let _ = presence.set_activity(&activity_for(&running(), &cfg()), 20_000);
        assert!(!presence.is_connected());
        // ...and reconnect on the following attempt.
        presence
            .set_activity(&activity_for(&running(), &cfg()), 40_000)
            .expect("reconnect");
        assert!(presence.is_connected());
        assert!(presence.sent >= 2);
    }

    #[test]
    fn updates_are_spaced_even_when_the_presence_changes() {
        let fake = FakeDiscordIpc::start();
        let mut presence =
            Presence::new("123456789012345678").with_socket(Some(fake.path().to_path_buf()));
        let activity = activity_for(&running(), &cfg());
        presence.set_activity(&activity, 10_000).expect("first");
        assert!(!presence.wants_update(&activity, 11_000), "unchanged");
        let mut other = activity.clone();
        other.state = "something else".to_string();
        assert!(!presence.wants_update(&other, 11_000), "too soon");
        assert!(presence.wants_update(&other, 30_000), "changed and spaced");
    }

    #[test]
    fn clearing_removes_the_presence_rather_than_leaving_it_stuck() {
        let fake = FakeDiscordIpc::start();
        let mut presence =
            Presence::new("123456789012345678").with_socket(Some(fake.path().to_path_buf()));
        presence
            .set_activity(&activity_for(&running(), &cfg()), 10_000)
            .expect("set");
        let before = fake.count();
        presence.clear(20_000).expect("clear");
        // The null activity and the protocol goodbye are two frames; the second
        // one is written without waiting for a reply, so wait for it.
        assert!(fake.wait_for(before + 2, Duration::from_secs(5)));
        let frames = fake.frames();
        let last = frames.iter().rfind(|f| f.opcode == OP_FRAME)
            .expect("an activity frame");
        assert_eq!(last.body["cmd"], "SET_ACTIVITY");
        assert_eq!(last.body["args"]["activity"], json!(null));
        // The protocol's own goodbye, so the client drops the connection.
        assert!(frames.iter().any(|f| f.opcode == OP_CLOSE));
        assert!(!presence.is_connected());
    }

    #[test]
    fn no_client_is_a_clear_message_not_a_silent_no_op() {
        let mut presence = Presence::new("123456789012345678")
            .with_socket(Some(PathBuf::from("/nonexistent/discord-ipc-0")));
        let error = presence
            .set_activity(&activity_for(&running(), &cfg()), 10_000)
            .expect_err("nothing is listening");
        assert!(matches!(error, PresenceError::NoClient(_)) || matches!(error, PresenceError::Io(_)));
        assert!(!presence.is_connected());
        assert!(presence.status_json()["error"].as_str().map(|e| e.len() > 10).unwrap_or(false));
    }
}
