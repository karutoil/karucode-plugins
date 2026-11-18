//! The publisher: one background thread that owns every Discord call.
//!
//! Two sinks, one loop:
//!
//! * **Rich Presence** over the local client's IPC socket - the profile line
//!   ("Playing <app>", project, session, elapsed). It is local, so it is cheap
//!   and needs no credential; it is also the surface that goes away when the
//!   desktop client is not running, which is why its state is reported rather
//!   than hidden.
//! * **The channel card** through a webhook, when one is configured: the same
//!   state as a message that is edited in place, plus the notifications that
//!   want a human.
//!
//! Hooks are not allowed to do I/O - the host gives a hook five seconds and runs
//! it inside a run's teardown - so they only record what happened and wake this
//! thread. Everything that talks to Discord happens here, one call at a time,
//! which is also what makes the rate-limit policy enforceable in one place.
//!
//! The policy:
//!
//! * the presence is sent only when it changed, and never twice inside
//!   [`crate::presence::MIN_UPDATE_GAP_SECS`];
//! * the live card is edited at most once per `refresh_secs` (default 20 s), and
//!   never twice within half of that, however many hooks fire;
//! * a `429` parks every publish for its `retry_after` instead of retrying;
//! * an edit that comes back `404` means the message was deleted by hand: the id
//!   is dropped and the next publish posts a new card;
//! * a failed publish backs off (15 s doubling to 5 min) and the error is kept
//!   for `discord_status` and the panel rather than logged and forgotten;
//! * a missing Discord client is retried every [`crate::presence::RECONNECT_SECS`],
//!   so starting Discord attaches the presence without touching the plugin.

use crate::card::{self, Activity, Card, Notice};
use crate::config::{self, Effective};
use crate::discord::{Client, DiscordError, MessageRef, Webhook};
use crate::karucode::{self, Snapshot};
use crate::presence::{self, Presence};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Never publish twice inside this window, whatever the hooks say.
pub const MIN_PUBLISH_GAP_SECS: u64 = 5;
pub const BACKOFF_START_SECS: u64 = 15;
pub const BACKOFF_MAX_SECS: u64 = 300;
/// How often the presence is re-examined when nothing woke the loop: a run that
/// started without a hook still shows up on the profile inside this window.
pub const PRESENCE_POLL_SECS: u64 = 15;
/// How many notification ids are remembered for de-duplication.
const SEEN_NOTICES: usize = 256;
/// How many sessions keep a "last tool" entry.
const ACTIVITY_SESSIONS: usize = 32;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct Inner {
    cfg: Effective,
    client: Option<Client>,
    client_problem: Option<String>,
    activity: BTreeMap<String, Activity>,
    notices: VecDeque<Notice>,
    seen: VecDeque<String>,
    board_message_id: Option<String>,
    board_channel_id: Option<String>,
    last_card: Option<Card>,
    last_publish_at: Option<i64>,
    last_ok_at: Option<i64>,
    last_error: Option<String>,
    last_error_at: Option<i64>,
    backoff_secs: u64,
    backoff_until: i64,
    published: u64,
    notices_posted: u64,
    notices_dropped: u64,
    last_snapshot: Option<Snapshot>,
    stopped: bool,
    /// The live IPC connection, when a presence is configured. Taken out of the
    /// mutex while it is being used so a wedged client cannot block a tool.
    presence: Option<Presence>,
    presence_app_id: Option<String>,
    presence_socket: Option<String>,
    presence_error: Option<String>,
    presence_error_at: Option<i64>,
    presence_next_attempt: i64,
    presence_sent: u64,
    presence_last_ok: Option<i64>,
    presence_fingerprint: Option<String>,
    presence_last_check: i64,
    presence_connected: bool,
}

/// One `discord_configure` call: a profile presence, a channel card, or both.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigureRequest {
    pub application_id: Option<String>,
    pub webhook_url: Option<String>,
    pub thread_id: Option<String>,
    pub refresh_secs: Option<u64>,
    pub notify: Option<String>,
    pub large_image: Option<String>,
    pub large_text: Option<String>,
    pub presence_socket: Option<String>,
}

/// The plugin's shared state. Cheap to clone (an `Arc`).
pub struct App {
    inner: Mutex<Inner>,
    wake: Condvar,
    publishing: AtomicBool,
}

impl App {
    pub fn new(cfg: Effective) -> Arc<Self> {
        let (client, client_problem) = build_client(&cfg);
        let board_message_id = cfg.persisted().board_message_id;
        let board_channel_id = cfg.persisted().board_channel_id;
        let presence = build_presence(&cfg);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                presence_app_id: cfg.application_id.clone(),
                cfg,
                client,
                client_problem,
                activity: BTreeMap::new(),
                notices: VecDeque::new(),
                seen: VecDeque::new(),
                board_message_id,
                board_channel_id,
                last_card: None,
                last_publish_at: None,
                last_ok_at: None,
                last_error: None,
                last_error_at: None,
                backoff_secs: BACKOFF_START_SECS,
                backoff_until: 0,
                published: 0,
                notices_posted: 0,
                notices_dropped: 0,
                last_snapshot: None,
                stopped: false,
                presence,
                presence_socket: None,
                presence_error: None,
                presence_error_at: None,
                presence_next_attempt: 0,
                presence_sent: 0,
                presence_last_ok: None,
                presence_fingerprint: None,
                presence_last_check: 0,
                presence_connected: false,
            }),
            wake: Condvar::new(),
            publishing: AtomicBool::new(false),
        })
    }

    /// Start the publisher thread. It exits when [`App::shutdown`] is called.
    pub fn spawn(app: Arc<Self>) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("discord-publisher".to_string())
            .spawn(move || loop {
                let wait = app.next_wait();
                {
                    let inner = app.inner.lock().unwrap();
                    if inner.stopped {
                        break;
                    }
                    let _ = app.wake.wait_timeout(inner, wait);
                }
                if app.inner.lock().unwrap().stopped {
                    break;
                }
                app.tick();
            })
            .expect("spawn the publisher thread")
    }

    fn next_wait(&self) -> Duration {
        let inner = self.inner.lock().unwrap();
        let now = now_ms();
        if !inner.notices.is_empty() {
            let wait = (inner.backoff_until - now).max(0) as u64;
            return Duration::from_millis(wait.min(1_000));
        }
        let mut wait_ms: u64 = 30_000;
        if inner.cfg.presence_enabled() {
            let due = match inner.presence.as_ref() {
                // Connected: re-examine on the poll cadence so a run that started
                // without a hook still reaches the profile.
                Some(presence) if presence.is_connected() => {
                    inner.presence_last_check + (PRESENCE_POLL_SECS as i64) * 1000
                }
                // Not connected: try now, or when the reconnect backoff expires.
                Some(_) => inner.presence_next_attempt.max(now),
                // Configured but no client object: do not spin.
                None => now + (PRESENCE_POLL_SECS as i64) * 1000,
            };
            let wait = (due - now).max(0) as u64;
            wait_ms = wait_ms.min(wait);
        }
        if inner.cfg.board_enabled() {
            let gap = inner.cfg.refresh_secs.max(MIN_PUBLISH_GAP_SECS) as i64;
            let due = inner.last_publish_at.map(|at| at + gap * 1000).unwrap_or(0);
            let due = due.max(inner.backoff_until);
            let wait = (due - now).max(0) as u64;
            wait_ms = wait_ms.min(wait);
        }
        Duration::from_millis(wait_ms.clamp(250, 30_000))
    }

    fn tick(&self) {
        let now = now_ms();
        let (notice, client, cfg) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.stopped || now < inner.backoff_until {
                return;
            }
            let notice = inner.notices.pop_front();
            (notice, inner.client.clone(), inner.cfg.clone())
        };
        if let Some(notice) = notice {
            if let Some(client) = client {
                self.post_notice(&client, &cfg, notice, now);
                return;
            }
        }
        // The presence is local and cheap; the card is a network call. Do the
        // local one first so a slow Discord API never delays the profile line.
        self.presence_tick(now);
        let Some(client) = client else {
            return;
        };
        if !cfg.board_enabled() {
            return;
        }
        let gap = cfg.refresh_secs.max(MIN_PUBLISH_GAP_SECS) as i64 * 1000;
        {
            let inner = self.inner.lock().unwrap();
            if let Some(at) = inner.last_publish_at {
                if now - at < gap {
                    return;
                }
            }
        }
        let _ = self.publish_board(&client, &cfg, now, true);
    }

    /// Bring the profile line up to date, connecting or reconnecting as needed.
    ///
    /// The connection is taken out of the mutex for the duration of the IPC so a
    /// wedged client cannot hold up a tool call or the panel.
    fn presence_tick(&self, now: i64) {
        let taken = { self.inner.lock().unwrap().presence.take() };
        let Some(mut presence) = taken else {
            return;
        };
        let (cfg, next_attempt) = {
            let inner = self.inner.lock().unwrap();
            (inner.cfg.clone(), inner.presence_next_attempt)
        };
        if !cfg.presence_enabled() || now < next_attempt {
            self.inner.lock().unwrap().presence = Some(presence);
            return;
        }
        let snapshot = karucode::snapshot(&cfg.data_dir, now, cfg.max_sessions);
        let activity = presence::activity_for(&snapshot, &cfg);
        let mut outcome = Ok(());
        if presence.wants_update(&activity, now) {
            outcome = presence.set_activity(&activity, now);
        }
        let connected = presence.is_connected();
        let socket = presence.socket_path().map(|p| p.display().to_string());
        let sent = presence.sent();
        let mut inner = self.inner.lock().unwrap();
        inner.presence_last_check = now;
        inner.presence_connected = connected;
        match outcome {
            Ok(()) => {
                inner.presence_error = None;
                inner.presence_error_at = None;
                inner.presence_next_attempt = 0;
                inner.presence_sent = sent;
                inner.presence_last_ok = Some(now);
                inner.presence_fingerprint = Some(activity.fingerprint());
                inner.presence_socket = socket;
            }
            Err(error) => {
                inner.presence_error = Some(error.user_message());
                inner.presence_error_at = Some(now);
                inner.presence_next_attempt = now + (presence::RECONNECT_SECS as i64) * 1000;
                inner.presence_sent = sent;
                inner.presence_socket = if connected { socket } else { None };
            }
        }
        inner.presence = Some(presence);
    }

    fn post_notice(&self, client: &Client, cfg: &Effective, notice: Notice, now: i64) {
        let payload = card::notice_payload(&notice, cfg);
        match client.post(&payload) {
            Ok(message) => {
                let mut inner = self.inner.lock().unwrap();
                inner.notices_posted += 1;
                inner.last_ok_at = Some(now);
                inner.last_error = None;
                inner.backoff_secs = BACKOFF_START_SECS;
                inner.backoff_until = 0;
                let _ = message;
            }
            Err(error) => {
                let mut inner = self.inner.lock().unwrap();
                inner.notices_dropped += 1;
                inner.last_error = Some(format!("notification: {}", error.user_message()));
                inner.last_error_at = Some(now);
                if let DiscordError::RateLimited { retry_after, .. } = error {
                    inner.backoff_until = now + (retry_after * 1000.0) as i64;
                }
            }
        }
    }

    /// Render and publish the live card. `record` is false for a probe, which
    /// must not move the board's own bookkeeping.
    fn publish_board(
        &self,
        client: &Client,
        cfg: &Effective,
        now: i64,
        record: bool,
    ) -> Result<(Card, MessageRef), DiscordError> {
        self.publishing.store(true, Ordering::SeqCst);
        let snapshot = karucode::snapshot(&cfg.data_dir, now, cfg.max_sessions);
        let activity = self.inner.lock().unwrap().activity.clone();
        let card = card::render(&snapshot, &activity, cfg, now);
        let payload = card.payload(cfg);
        let existing = self.inner.lock().unwrap().board_message_id.clone();

        let result = match existing {
            Some(id) => match client.edit(&id, &payload) {
                Ok(message) => Ok((card, message)),
                Err(DiscordError::NotFound) => {
                    // The message was deleted by hand: forget it and repost.
                    self.forget_board_message();
                    client.post(&payload).map(|message| (card, message))
                }
                Err(error) => Err(error),
            },
            None => client.post(&payload).map(|message| (card, message)),
        };

        match &result {
            Ok((card, message)) => {
                let mut inner = self.inner.lock().unwrap();
                inner.last_card = Some(card.clone());
                inner.last_snapshot = Some(snapshot);
                inner.last_ok_at = Some(now);
                inner.last_error = None;
                inner.last_error_at = None;
                inner.backoff_secs = BACKOFF_START_SECS;
                inner.backoff_until = 0;
                if record {
                    inner.published += 1;
                    inner.last_publish_at = Some(now);
                }
                let changed = inner.board_message_id.as_deref() != Some(message.id.as_str())
                    || inner.board_channel_id.as_deref() != Some(message.channel_id.as_str());
                inner.board_message_id = Some(message.id.clone());
                inner.board_channel_id = Some(message.channel_id.clone());
                if changed {
                    persist_board(&inner.cfg, &inner.board_message_id, &inner.board_channel_id);
                }
            }
            Err(error) => {
                let mut inner = self.inner.lock().unwrap();
                inner.last_error = Some(error.user_message());
                inner.last_error_at = Some(now);
                if record {
                    inner.published += 1;
                    inner.last_publish_at = Some(now);
                }
                match error {
                    DiscordError::RateLimited { retry_after, .. } => {
                        inner.backoff_until = now + (retry_after * 1000.0) as i64;
                    }
                    _ => {
                        let backoff = inner.backoff_secs.min(BACKOFF_MAX_SECS);
                        inner.backoff_until = now + (backoff * 1000) as i64;
                        inner.backoff_secs = (inner.backoff_secs * 2).min(BACKOFF_MAX_SECS);
                    }
                }
            }
        }
        self.publishing.store(false, Ordering::SeqCst);
        result
    }

    fn forget_board_message(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.board_message_id = None;
        inner.board_channel_id = None;
        persist_board(&inner.cfg, &None, &None);
    }

    // ---- hooks -----------------------------------------------------------

    pub fn on_tool_result(&self, session_id: &str, tool: &str, ok: bool) {
        if session_id.is_empty() || tool.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.activity.len() >= ACTIVITY_SESSIONS && !inner.activity.contains_key(session_id) {
            if let Some(oldest) = inner
                .activity
                .iter()
                .min_by_key(|(_, a)| a.at)
                .map(|(k, _)| k.clone())
            {
                inner.activity.remove(&oldest);
            }
        }
        inner.activity.insert(
            session_id.to_string(),
            Activity {
                tool: tool.to_string(),
                at: now_ms(),
                ok,
            },
        );
        drop(inner);
        self.wake.notify_all();
    }

    pub fn on_run_end(&self, _session_id: &str, _run_id: &str, _status: &str) {
        self.wake.notify_all();
    }

    /// Queue a notification. Returns whether it will be posted.
    pub fn on_notify(&self, payload: &Value) -> bool {
        let now = now_ms();
        let Some(notice) = Notice::parse(payload, now) else {
            return false;
        };
        let mut inner = self.inner.lock().unwrap();
        if !inner.cfg.notify_enabled(&notice.kind) {
            return false;
        }
        if !notice.id.is_empty() {
            if inner.seen.iter().any(|id| id == &notice.id) {
                return false;
            }
            inner.seen.push_back(notice.id.clone());
            while inner.seen.len() > SEEN_NOTICES {
                inner.seen.pop_front();
            }
        }
        inner.notices.push_back(notice);
        drop(inner);
        self.wake.notify_all();
        true
    }

    // ---- tools -----------------------------------------------------------

    /// One setup call: a profile presence (Application ID) and/or a channel card
    /// (webhook URL). Either alone is a complete setup; both together is the
    /// whole picture.
    pub fn configure(&self, request: ConfigureRequest) -> Result<Value, String> {
        let mut result = serde_json::Map::new();
        if let Some(app_id) = request
            .application_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            result.insert(
                "presence".into(),
                self.configure_presence(app_id, &request)?,
            );
        }
        if let Some(url) = request
            .webhook_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            result.insert("card".into(), self.configure_webhook(url, &request)?);
        }
        if result.is_empty() {
            return Err(
                "nothing to configure: pass `application_id` for the profile presence and/or \
                 `webhook_url` for a channel card"
                    .to_string(),
            );
        }
        result.insert("configured".into(), json!(true));
        Ok(Value::Object(result))
    }

    /// Store an Application ID, connect to the local client and publish the
    /// presence.
    ///
    /// The ID is stored even when no client is running: it is valid, and the
    /// publisher attaches as soon as one appears. That is deliberate - refusing
    /// to save it because Discord happened to be closed would make the operator
    /// repeat the paste later.
    fn configure_presence(
        &self,
        app_id: &str,
        request: &ConfigureRequest,
    ) -> Result<Value, String> {
        if !config::valid_application_id(app_id) {
            return Err(format!(
                "`{app_id}` is not a Discord Application ID: it is a number of 15-25 digits \
                 (Developer Portal -> your application -> Application ID), not the client secret"
            ));
        }
        let cfg = self.inner.lock().unwrap().cfg.clone();
        let now = now_ms();
        let mut probe_cfg = cfg.clone();
        probe_cfg.application_id = Some(app_id.to_string());
        probe_cfg.presence = true;
        if let Some(image) = request.large_image.as_deref() {
            probe_cfg.large_image = Some(image.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(text) = request.large_text.as_deref() {
            probe_cfg.large_text = Some(text.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(socket) = request.presence_socket.as_deref() {
            probe_cfg.presence_socket = Some(socket.trim().to_string()).filter(|s| !s.is_empty());
        }

        let mut state = cfg.persisted();
        state.application_id = Some(app_id.to_string());
        if request.large_image.is_some() {
            state.large_image = probe_cfg.large_image.clone();
        }
        if request.large_text.is_some() {
            state.large_text = probe_cfg.large_text.clone();
        }
        if request.presence_socket.is_some() {
            state.presence_socket = probe_cfg.presence_socket.clone();
        }
        state.updated_at = Some(now);
        cfg.save(&state)
            .map_err(|e| format!("cannot save the Application ID: {e}"))?;
        let next = cfg.reload();

        let mut presence = Presence::new(app_id.to_string())
            .with_socket(next.presence_socket.clone().map(PathBuf::from));
        let snapshot = karucode::snapshot(&next.data_dir, now, next.max_sessions);
        let activity = presence::activity_for(&snapshot, &next);
        let probe = presence.set_activity(&activity, now);
        let (connected, error) = match &probe {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.user_message())),
        };
        let socket = presence.socket_path().map(|p| p.display().to_string());
        let sent = presence.sent();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.cfg = next.clone();
            inner.presence_app_id = Some(app_id.to_string());
            inner.presence = Some(presence);
            inner.presence_socket = socket.clone();
            inner.presence_error = error.clone();
            inner.presence_error_at = error.as_ref().map(|_| now);
            inner.presence_next_attempt = if connected {
                0
            } else {
                now + (presence::RECONNECT_SECS as i64) * 1000
            };
            inner.presence_sent = sent;
            inner.presence_last_ok = if connected { Some(now) } else { None };
            inner.presence_fingerprint = if connected {
                Some(activity.fingerprint())
            } else {
                None
            };
            inner.presence_last_check = now;
            inner.presence_connected = connected;
        }
        self.wake.notify_all();
        Ok(json!({
            "application_id": app_id,
            "connected": connected,
            "socket": socket,
            "error": error,
            "activity": activity.to_json(),
            "note": if connected {
                "the profile now shows this presence"
            } else {
                "stored; the presence attaches as soon as a Discord client is reachable"
            },
        }))
    }

    /// Validate a pasted webhook, prove it works by publishing the card, and
    /// persist it. The probe message *becomes* the live card, so the operator
    /// sees their own status the moment setup succeeds.
    ///
    /// Reconfiguring is not destructive: the same webhook reuses the card that is
    /// already in the channel, and a move to a different webhook deletes the old
    /// card, because a status board nobody updates is worse than none.
    fn configure_webhook(
        &self,
        webhook_url: &str,
        request: &ConfigureRequest,
    ) -> Result<Value, String> {
        let (cfg, old_client, old_message_id) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.cfg.clone(),
                inner.client.clone(),
                inner.board_message_id.clone(),
            )
        };
        let webhook = Webhook::parse_with_base(webhook_url, cfg.api_base.as_deref())
            .map_err(|e| e.user_message())?;
        let client = Client::new(webhook.clone());
        let same_webhook = old_client
            .as_ref()
            .map(|old| {
                old.webhook.base == webhook.base
                    && old.webhook.id == webhook.id
                    && old.webhook.token == webhook.token
            })
            .unwrap_or(false);
        let now = now_ms();
        let snapshot = karucode::snapshot(&cfg.data_dir, now, cfg.max_sessions);
        let activity = self.inner.lock().unwrap().activity.clone();
        let mut probe_cfg = cfg.clone();
        probe_cfg.webhook_url = Some(webhook_url.trim().to_string());
        probe_cfg.thread_id = match request.thread_id.clone() {
            Some(thread) => Some(thread).filter(|t| !t.is_empty()),
            None => cfg.thread_id.clone(),
        };
        if let Some(refresh) = request.refresh_secs {
            probe_cfg.refresh_secs = if refresh == 0 {
                0
            } else {
                refresh.clamp(config::MIN_REFRESH_SECS, config::MAX_REFRESH_SECS)
            };
        }
        if let Some(notify) = request.notify.as_deref() {
            probe_cfg.notify = config::parse_notify(notify).0;
        }
        let card = card::render(&snapshot, &activity, &probe_cfg, now);
        let payload = card.payload(&probe_cfg);
        let message = if same_webhook {
            match old_message_id.as_deref() {
                Some(id) => match client.edit(id, &payload) {
                    Ok(message) => message,
                    Err(DiscordError::NotFound) => {
                        client.post(&payload).map_err(|e| e.user_message())?
                    }
                    Err(error) => return Err(error.user_message()),
                },
                None => client.post(&payload).map_err(|e| e.user_message())?,
            }
        } else {
            let message = client.post(&payload).map_err(|e| e.user_message())?;
            // The old card lives behind a webhook this plugin no longer publishes
            // to; only the old client can delete it.
            if let (Some(old_client), Some(old_id)) =
                (old_client.as_ref(), old_message_id.as_deref())
            {
                let _ = old_client.delete(old_id);
            }
            message
        };

        let mut state = cfg.persisted();
        state.webhook_url = Some(webhook_url.trim().to_string());
        if request.thread_id.is_some() {
            state.thread_id = probe_cfg.thread_id.clone();
        }
        state.board_message_id = Some(message.id.clone());
        state.board_channel_id = Some(message.channel_id.clone());
        if let Some(refresh) = request.refresh_secs {
            state.refresh_secs = Some(if refresh == 0 {
                0
            } else {
                refresh.clamp(config::MIN_REFRESH_SECS, config::MAX_REFRESH_SECS)
            });
        }
        if let Some(notify) = request.notify.as_deref() {
            state.notify = Some(notify.to_string());
        }
        state.updated_at = Some(now);
        cfg.save(&state)
            .map_err(|e| format!("the webhook works, but saving it failed: {e}"))?;

        let next = cfg.reload();
        let (client, client_problem) = build_client(&next);
        {
            let mut inner = self.inner.lock().unwrap();
            inner.cfg = next.clone();
            inner.client = client;
            inner.client_problem = client_problem;
            inner.board_message_id = Some(message.id.clone());
            inner.board_channel_id = Some(message.channel_id.clone());
            inner.last_card = Some(card.clone());
            inner.last_ok_at = Some(now);
            inner.last_publish_at = Some(now);
            inner.last_error = None;
            inner.backoff_secs = BACKOFF_START_SECS;
            inner.backoff_until = 0;
            inner.published += 1;
        }
        self.wake.notify_all();
        Ok(json!({
            "connected": true,
            "webhook": webhook.redacted(),
            "thread_id": next.thread_id,
            "message_id": message.id,
            "channel_id": message.channel_id,
            "board": next.board_enabled(),
            "refresh_secs": next.refresh_secs,
            "notify": next.notify,
            "card": card.summary(),
        }))
    }

    /// Publish now, bypassing the cadence (a person asked for it): the profile
    /// presence first, then the card.
    pub fn publish_now(&self) -> Result<Value, String> {
        let now = now_ms();
        let (client, cfg) = {
            let inner = self.inner.lock().unwrap();
            (inner.client.clone(), inner.cfg.clone())
        };
        let mut result = serde_json::Map::new();
        if cfg.presence_enabled() {
            // An explicit push ignores the reconnect backoff: the person asked.
            {
                let mut inner = self.inner.lock().unwrap();
                inner.presence_next_attempt = 0;
            }
            self.presence_tick(now);
            let inner = self.inner.lock().unwrap();
            result.insert(
                "presence".into(),
                json!({
                    "connected": inner.presence_connected,
                    "socket": inner.presence_socket,
                    "sent": inner.presence_sent,
                    "error": inner.presence_error,
                }),
            );
        }
        if let Some(client) = client {
            if cfg.board_enabled() {
                let (card, message) = self
                    .publish_board(&client, &cfg, now, true)
                    .map_err(|e| e.user_message())?;
                result.insert(
                    "card".into(),
                    json!({
                        "published": true,
                        "message_id": message.id,
                        "channel_id": message.channel_id,
                        "summary": card.summary(),
                    }),
                );
            }
        }
        if result.is_empty() {
            return Err(self
                .inner
                .lock()
                .unwrap()
                .client_problem
                .clone()
                .unwrap_or_else(|| {
                    "nothing is configured: call discord_configure with an `application_id` \
                     (profile presence) and/or a `webhook_url` (channel card)"
                        .to_string()
                }));
        }
        result.insert("published".into(), json!(true));
        Ok(Value::Object(result))
    }

    /// Post an ad-hoc message, for `discord_notify`.
    pub fn notify(&self, title: &str, body: &str, kind: &str) -> Result<Value, String> {
        let (client, cfg) = {
            let inner = self.inner.lock().unwrap();
            (inner.client.clone(), inner.cfg.clone())
        };
        let Some(client) = client else {
            return Err("no webhook is configured: call discord_configure first".to_string());
        };
        let now = now_ms();
        let notice = Notice {
            id: format!("tool:{now}"),
            kind: kind.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            source: "karucode".to_string(),
            at: now,
            ..Default::default()
        };
        let message = client
            .post(&card::notice_payload(&notice, &cfg))
            .map_err(|e| e.user_message())?;
        let mut inner = self.inner.lock().unwrap();
        inner.notices_posted += 1;
        inner.last_ok_at = Some(now);
        Ok(json!({
            "posted": true,
            "message_id": message.id,
            "channel_id": message.channel_id,
        }))
    }

    pub fn status_json(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        // A live read, not the last published one: `discord_status` is the tool
        // that answers "is this working", and a stale answer is not an answer.
        let snapshot = karucode::snapshot(&inner.cfg.data_dir, now_ms(), inner.cfg.max_sessions);
        json!({
            "configured": inner.cfg.configured(),
            "presence": {
                "enabled": inner.cfg.presence_enabled(),
                "application_id": inner.presence_app_id,
                "connected": inner.presence_connected,
                "socket": inner.presence_socket,
                "sent": inner.presence_sent,
                "last_ok_at": inner.presence_last_ok,
                "error": inner.presence_error,
                "error_at": inner.presence_error_at,
                "next_attempt_at": inner.presence_next_attempt,
                "large_image": inner.cfg.large_image,
                "large_text": inner.cfg.large_text,
                "connection": inner.presence.as_ref().map(|p| p.status_json()),
            },
            "client_ready": inner.client.is_some(),
            "client_problem": inner.client_problem,
            "webhook": inner.cfg.redacted_webhook(),
            "board": {
                "enabled": inner.cfg.board_enabled(),
                "refresh_secs": inner.cfg.refresh_secs,
                "message_id": inner.board_message_id,
                "channel_id": inner.board_channel_id,
                "last_publish_at": inner.last_publish_at,
                "last_ok_at": inner.last_ok_at,
                "published": inner.published,
                "card": inner.last_card.as_ref().map(Card::summary),
            },
            "notify": {
                "kinds": inner.cfg.notify,
                "unknown": inner.cfg.notify_unknown,
                "posted": inner.notices_posted,
                "dropped": inner.notices_dropped,
                "queued": inner.notices.len(),
            },
            "last_error": inner.last_error,
            "last_error_at": inner.last_error_at,
            "backoff_until": inner.backoff_until,
            "activity": inner.activity.iter().map(|(session, activity)| json!({
                "session": session,
                "tool": activity.tool,
                "ok": activity.ok,
                "at": activity.at,
            })).collect::<Vec<_>>(),
            "karucode": {
                "data_dir": inner.cfg.data_dir.display().to_string(),
                "db": karucode::db_path(&inner.cfg.data_dir).display().to_string(),
                "snapshot": {
                    "readable": snapshot.readable,
                    "running": snapshot.running.len(),
                    "approvals": snapshot.approvals.pending,
                    "agents_running": snapshot.agents_running,
                    "focus": snapshot.focus().map(|(session, run)| json!({
                        "session": session.id,
                        "title": session.title,
                        "project": session.project,
                        "running": run.is_some(),
                    })),
                    "problems": snapshot.problems,
                },
            },
            "settings": inner.cfg.public_json(),
        })
    }

    /// The inspector panel: a real view, built from memory only, so a 5 s poll
    /// costs nothing.
    pub fn panel_json(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let now = now_ms();
        let mut views: Vec<Value> = Vec::new();

        // The profile line first: it is the surface this plugin exists for.
        if !inner.cfg.presence_enabled() {
            views.push(json!({
                "view": "stat",
                "label": "Presence",
                "value": "off",
                "detail": "set an Application ID with discord_configure",
                "tone": "warn",
            }));
        } else if inner.presence_connected {
            views.push(json!({
                "view": "stat",
                "label": "Presence",
                "value": "live",
                "detail": format!(
                    "{} · {} sent",
                    inner.presence_socket.clone().unwrap_or_else(|| "discord".to_string()),
                    inner.presence_sent
                ),
                "tone": "ok",
            }));
        } else {
            views.push(json!({
                "view": "stat",
                "label": "Presence",
                "value": "waiting",
                "detail": inner
                    .presence_error
                    .clone()
                    .unwrap_or_else(|| "no Discord client found yet".to_string()),
                "tone": "bad",
            }));
        }

        // Then the optional channel card.
        let card_configured = inner.cfg.webhook_url.is_some();
        let (value, tone, detail) = match (&inner.last_error, card_configured) {
            (Some(error), _) => ("error".to_string(), "bad", error.clone()),
            (None, true) if inner.last_ok_at.is_some() => (
                "live".to_string(),
                "ok",
                format!(
                    "updated {} ago",
                    card::human_span(now - inner.last_ok_at.unwrap_or(now))
                ),
            ),
            (None, true) => ("ready".to_string(), "warn", "no publish yet".to_string()),
            (None, false) => (
                "off".to_string(),
                "warn",
                "optional: paste a webhook URL with discord_configure".to_string(),
            ),
        };
        views.push(json!({
            "view": "stat",
            "label": "Channel card",
            "value": value,
            "detail": detail,
            "tone": tone,
        }));

        if let Some(card) = inner.last_card.as_ref() {
            views.push(json!({
                "view": "stat",
                "label": "Now",
                "value": card.focus_project.clone().unwrap_or_else(|| card.state.clone()),
                "detail": card.focus_title.clone().unwrap_or_else(|| card.title.clone()),
                "tone": if card.state == "running" { "ok" } else { "warn" },
            }));
            let mut items: Vec<Value> = Vec::new();
            for (name, value) in card.fields.iter().take(6) {
                items.push(json!({
                    "text": card::plain(name),
                    "detail": card::plain(value),
                }));
            }
            if !items.is_empty() {
                views.push(json!({ "view": "list", "title": "Card", "items": items }));
            }
        } else if inner.cfg.presence_enabled() {
            // Without a card, show what the profile line says right now.
            let snapshot = karucode::snapshot(&inner.cfg.data_dir, now, inner.cfg.max_sessions);
            let activity = presence::activity_for(&snapshot, &inner.cfg);
            let mut items = vec![json!({ "text": "Details", "detail": card::plain(&activity.details) })];
            items.push(json!({ "text": "State", "detail": card::plain(&activity.state) }));
            if let Some(start) = activity.start {
                items.push(json!({
                    "text": "Elapsed",
                    "detail": card::human_span(now.saturating_sub(start)),
                }));
            }
            views.push(json!({ "view": "list", "title": "Profile", "items": items }));
        }

        if !inner.cfg.problems.is_empty() {
            views.push(json!({
                "view": "list",
                "title": "Settings",
                "items": inner.cfg.problems.iter().map(|p| json!({ "text": p })).collect::<Vec<_>>(),
            }));
        }
        if let Some(snapshot) = inner.last_snapshot.as_ref() {
            if !snapshot.problems.is_empty() {
                views.push(json!({
                    "view": "list",
                    "title": "Problems",
                    "items": snapshot.problems.iter().map(|p| json!({ "text": p })).collect::<Vec<_>>(),
                }));
            }
        }
        json!({
            "view": "stack",
            "views": views,
            "presence": {
                "application_id": inner.presence_app_id,
                "socket": inner.presence_socket,
            },
            "webhook": inner.cfg.redacted_webhook(),
            "notify": inner.cfg.notify,
            "refresh_secs": inner.cfg.refresh_secs,
        })
    }

    /// Stop the publisher, take the presence down and leave an honest card
    /// behind. Best effort: the host gives a plugin two seconds to shut down.
    pub fn shutdown(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.stopped = true;
        }
        self.wake.notify_all();
        // Let an in-flight publish finish so it cannot overwrite the offline card.
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while self.publishing.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let now = now_ms();
        // The profile line goes away with the plugin: "Playing karucode" forever
        // is worse than no presence at all.
        let presence = { self.inner.lock().unwrap().presence.take() };
        if let Some(mut presence) = presence {
            if presence.is_connected() {
                let _ = presence.clear(now);
            }
        }
        let (client, cfg, message_id) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.client.clone(),
                inner.cfg.clone(),
                inner.board_message_id.clone(),
            )
        };
        if let (Some(client), Some(id)) = (client, message_id) {
            if cfg.board_enabled() {
                let card = card::offline(&cfg, now);
                let _ = client.edit(&id, &card.payload(&cfg));
            }
        }
    }
}

fn build_presence(cfg: &Effective) -> Option<Presence> {
    if !cfg.presence_enabled() {
        return None;
    }
    let app_id = cfg.application_id.clone()?;
    Some(
        Presence::new(app_id)
            .with_socket(cfg.presence_socket.clone().map(PathBuf::from)),
    )
}

fn build_client(cfg: &Effective) -> (Option<Client>, Option<String>) {
    let Some(url) = cfg.webhook_url.as_deref() else {
        return (None, None);
    };
    match Webhook::parse_with_base(url, cfg.api_base.as_deref()) {
        Ok(webhook) => (Some(Client::new(webhook)), None),
        Err(error) => (None, Some(error.user_message())),
    }
}

fn persist_board(cfg: &Effective, message_id: &Option<String>, channel_id: &Option<String>) {
    let mut state = cfg.persisted();
    state.board_message_id = message_id.clone();
    state.board_channel_id = channel_id.clone();
    state.updated_at = Some(now_ms());
    if let Err(e) = cfg.save(&state) {
        eprintln!("discord-presence: cannot save the board message id: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod)]
#[path = "../tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use super::support::{FakeDiscord, FakeDiscordIpc};
    use super::*;
    use crate::karucode::tests::fixture_db;
    use serde_json::json;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "kc-discord-presence-worker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn app_with(fake: &FakeDiscord, dir: &std::path::Path) -> Arc<App> {
        let raw = format!(
            r#"{{"webhook_url":"{}","api_base":"{}","data_dir":"{}","refresh_secs":10}}"#,
            fake.url("123", "tok"),
            fake.base(),
            dir.display()
        );
        App::new(Effective::load_from(Some(&raw)))
    }

    #[test]
    fn a_publish_posts_once_then_edits_the_same_message() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        let first = app.publish_now().expect("first publish");
        assert_eq!(first["published"], true);
        let second = app.publish_now().expect("second publish");
        assert_eq!(first["card"]["message_id"], second["card"]["message_id"]);
        assert_eq!(first["card"]["message_id"], "m_default");

        let requests = fake.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert!(requests[0].query.contains("wait=true"));
        assert_eq!(requests[1].method, "PATCH");
        assert!(requests[1].route().ends_with("/messages/m_default"));
        // The card really carries the live session from the database.
        let embed = &requests[0].body.as_ref().unwrap()["embeds"][0];
        assert!(embed["title"].as_str().unwrap().contains("karucode-plugins"));
        assert_eq!(requests[0].body.as_ref().unwrap()["allowed_mentions"]["parse"], json!([]));
    }

    #[test]
    fn a_deleted_message_is_reposted_instead_of_editing_a_ghost() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        app.publish_now().expect("first publish");
        fake.reply(404, json!({"message": "Unknown Message", "code": 10008}));
        let result = app.publish_now().expect("repost after 404");
        assert_eq!(result["card"]["message_id"], "m_default");
        let methods: Vec<String> = fake.requests().iter().map(|r| r.method.clone()).collect();
        assert_eq!(methods, vec!["POST", "PATCH", "POST"]);
    }

    #[test]
    fn a_429_parks_the_next_publish_for_its_retry_after() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        fake.reply(429, json!({"message": "You are being rate limited.", "retry_after": 4.0, "global": false}));
        let error = app.publish_now().expect_err("a 429 is an error for this call");
        assert!(error.contains("rate limiting"), "{error}");
        let inner = app.inner.lock().unwrap();
        assert!(inner.backoff_until > now_ms(), "the publisher must wait");
        assert_eq!(inner.last_error.as_deref().map(|e| e.contains("rate limiting")), Some(true));
    }

    #[test]
    fn a_404_on_post_is_an_unknown_webhook_not_a_deleted_message() {
        // Discord answers 404 both for "this message is gone" and for "this
        // webhook does not exist". Only the second one is a refusal on a post,
        // and reporting it as a deleted message sent the operator hunting for the
        // wrong problem.
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        fake.reply(404, json!({"message": "Unknown Webhook", "code": 10015}));
        let error = app.publish_now().expect_err("an unknown webhook is an error");
        assert!(error.contains("Unknown Webhook"), "{error}");
        assert!(error.contains("10015"), "{error}");
        assert!(!error.contains("no longer exists"), "{error}");
    }

    #[test]
    fn a_429_with_only_a_retry_after_header_is_still_obeyed() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        fake.reply_with_headers(
            429,
            json!({"message": "You are being rate limited."}),
            &[("Retry-After", "3")],
        );
        let error = app.publish_now().expect_err("a 429 is an error for this call");
        assert!(error.contains("3.0s"), "{error}");
    }

    #[test]
    fn a_401_is_reported_with_discords_own_words() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        fake.reply(401, json!({"message": "Invalid Webhook Token", "code": 50027}));
        let error = app.publish_now().expect_err("a bad token is an error");
        assert!(error.contains("Invalid Webhook Token"), "{error}");
        assert!(error.contains("50027"), "{error}");
        assert!(!error.contains("tok"), "the token must not leak: {error}");
    }

    #[test]
    fn configure_persists_the_webhook_and_the_probe_becomes_the_card() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = App::new(Effective::load_from(Some(&format!(
            r#"{{"api_base":"{}","data_dir":"{}"}}"#,
            fake.base(),
            dir.display()
        ))));
        let result = app
            .configure(worker_configure(
                None,
                Some(&fake.url("77", "secret-token")),
                Some(30),
                Some("all"),
            ))
            .expect("configure");
        let card = &result["card"];
        assert_eq!(card["connected"], true);
        assert_eq!(card["message_id"], "m_default");
        assert_eq!(card["refresh_secs"], 30);
        assert!(!card["webhook"].as_str().unwrap().contains("secret-token"));

        let state = app.inner.lock().unwrap().cfg.persisted();
        assert_eq!(state.board_message_id.as_deref(), Some("m_default"));
        assert_eq!(
            state.webhook_url.as_deref(),
            Some(fake.url("77", "secret-token").as_str())
        );
        // The next publish edits the probe message rather than posting another.
        app.publish_now().expect("publish");
        assert_eq!(fake.last().unwrap().method, "PATCH");
    }

    #[test]
    fn configure_refuses_a_bad_url_without_writing_anything() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        let app = App::new(Effective::load_from(Some(&format!(
            r#"{{"api_base":"{}","data_dir":"{}"}}"#,
            fake.base(),
            dir.display()
        ))));
        let error = app
            .configure(worker_configure(None, Some("https://example.test/nope"), None, None))
            .unwrap_err();
        assert!(error.contains("webhooks"), "{error}");
        assert_eq!(fake.count(), 0);
        assert!(app.inner.lock().unwrap().cfg.persisted().webhook_url.is_none());
    }

    fn worker_configure(
        application_id: Option<&str>,
        webhook_url: Option<&str>,
        refresh_secs: Option<u64>,
        notify: Option<&str>,
    ) -> ConfigureRequest {
        ConfigureRequest {
            application_id: application_id.map(str::to_string),
            webhook_url: webhook_url.map(str::to_string),
            refresh_secs,
            notify: notify.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn configure_presence_stores_the_id_and_publishes_the_profile_line() {
        let ipc = FakeDiscordIpc::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = App::new(Effective::load_from(Some(&format!(
            r#"{{"data_dir":"{}"}}"#,
            dir.display()
        ))));
        let result = app
            .configure(ConfigureRequest {
                application_id: Some("123456789012345678".to_string()),
                presence_socket: Some(ipc.path().display().to_string()),
                ..Default::default()
            })
            .expect("configure");
        assert_eq!(result["presence"]["connected"], true);
        assert_eq!(result["presence"]["application_id"], "123456789012345678");
        assert_eq!(
            result["presence"]["activity"]["details"],
            "karucode-plugins"
        );
        // The fixture has a pending approval, and that is what the profile line
        // says: the thing that needs a human outranks the session title.
        assert_eq!(
            result["presence"]["activity"]["state"],
            "⏸ write_file docs/plan.md"
        );
        assert!(result["presence"]["activity"]["timestamps"]["start"]
            .as_i64()
            .unwrap()
            > 0);

        // The Application ID is persisted, so a restart keeps the presence.
        let state = app.inner.lock().unwrap().cfg.persisted();
        assert_eq!(
            state.application_id.as_deref(),
            Some("123456789012345678")
        );
        // And the client really received the handshake and the activity.
        let frames = ipc.frames();
        assert_eq!(frames[0].body["client_id"], "123456789012345678");
        assert_eq!(frames[1].body["cmd"], "SET_ACTIVITY");
    }

    #[test]
    fn configure_presence_without_a_client_still_stores_the_id() {
        let dir = tempdir();
        fixture_db(&dir);
        let app = App::new(Effective::load_from(Some(&format!(
            r#"{{"data_dir":"{}"}}"#,
            dir.display()
        ))));
        let result = app
            .configure(ConfigureRequest {
                application_id: Some("123456789012345678".to_string()),
                presence_socket: Some("/nonexistent/discord-ipc-0".to_string()),
                ..Default::default()
            })
            .expect("configure");
        assert_eq!(result["presence"]["connected"], false);
        assert!(result["presence"]["error"].as_str().unwrap().len() > 10);
        assert_eq!(
            app.inner.lock().unwrap().cfg.persisted().application_id.as_deref(),
            Some("123456789012345678"),
            "a closed Discord must not cost the operator the paste"
        );
        // The panel says it is waiting, not that it is live.
        let panel = app.panel_json();
        assert_eq!(panel["views"][0]["label"], "Presence");
        assert_eq!(panel["views"][0]["value"], "waiting");
    }

    #[test]
    fn configure_presence_refuses_a_client_secret_pasted_by_mistake() {
        let dir = tempdir();
        let app = App::new(Effective::load_from(Some(&format!(
            r#"{{"data_dir":"{}"}}"#,
            dir.display()
        ))));
        let error = app
            .configure(ConfigureRequest {
                application_id: Some("aBcD1234efGh5678iJkL9012mNoP3456".to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(error.contains("Application ID"), "{error}");
        assert!(error.contains("not the client secret"), "{error}");
    }

    #[test]
    fn shutdown_takes_the_presence_down() {
        let ipc = FakeDiscordIpc::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = App::new(Effective::load_from(Some(&format!(
            r#"{{"data_dir":"{}"}}"#,
            dir.display()
        ))));
        app.configure(ConfigureRequest {
            application_id: Some("123456789012345678".to_string()),
            presence_socket: Some(ipc.path().display().to_string()),
            ..Default::default()
        })
        .expect("configure");
        app.shutdown();
        let frames = ipc.frames();
        let activity = frames.iter().rfind(|f| f.opcode == 1)
            .expect("an activity frame");
        assert_eq!(activity.body["cmd"], "SET_ACTIVITY");
        assert_eq!(activity.body["args"]["activity"], json!(null));
        // ...and the connection is closed with the protocol's own goodbye.
        assert!(frames.iter().any(|f| f.opcode == 2));
    }

    #[test]
    fn notifications_are_filtered_deduped_and_posted_as_their_own_message() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        let payload = json!({
            "id": "approval:a_1", "kind": "needs_input", "title": "Approval needed",
            "body": "write_file docs/plan.md", "session_id": "s_1",
            "session_title": "Plan the work", "source": "runtime"
        });
        assert!(app.on_notify(&payload));
        assert!(!app.on_notify(&payload), "the same id must not post twice");
        let ignored = json!({"id": "run:1", "kind": "run_finished", "title": "done"});
        assert!(!app.on_notify(&ignored), "run_finished is not in the default set");

        app.tick();
        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        let embed = &requests[0].body.as_ref().unwrap()["embeds"][0];
        assert!(embed["title"].as_str().unwrap().contains("Approval needed"));
        assert_eq!(embed["color"], json!(crate::card::COLOR_APPROVAL));
    }

    #[test]
    fn a_tool_result_is_what_the_card_calls_activity() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        app.on_tool_result("s_live", "plugin__kg__raw", true);
        app.publish_now().expect("publish");
        let body = fake.last().unwrap().body.unwrap();
        let fields = body["embeds"][0]["fields"].as_array().unwrap();
        assert!(
            fields.iter().any(|f| f["name"] == "Activity"
                && f["value"].as_str().unwrap().contains("plugin__kg__raw")),
            "{fields:?}"
        );
    }

    #[test]
    fn status_and_panel_never_leak_the_token() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        app.publish_now().expect("publish");
        let status = app.status_json().to_string();
        assert!(!status.contains("\"tok\""), "{status}");
        assert!(status.contains("***"));
        let panel = app.panel_json().to_string();
        assert!(!panel.contains("\"tok\""), "{panel}");
        assert!(panel.contains("\"view\":\"stack\""));
    }

    #[test]
    fn an_unconfigured_plugin_says_what_to_do_instead_of_failing_silently() {
        let dir = tempdir();
        let app = App::new(Effective::load_from(Some(&format!(
            r#"{{"data_dir":"{}"}}"#,
            dir.display()
        ))));
        let error = app.publish_now().unwrap_err();
        assert!(error.contains("discord_configure"), "{error}");
        let panel = app.panel_json();
        assert_eq!(panel["views"][0]["value"], "off");
    }

    #[test]
    fn shutdown_leaves_an_offline_card() {
        let fake = FakeDiscord::start();
        let dir = tempdir();
        fixture_db(&dir);
        let app = app_with(&fake, &dir);
        app.publish_now().expect("publish");
        app.shutdown();
        let last = fake.last().unwrap();
        assert_eq!(last.method, "PATCH");
        let embed = &last.body.as_ref().unwrap()["embeds"][0];
        assert!(embed["title"].as_str().unwrap().contains("offline"));
    }
}
