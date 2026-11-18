//! Browser lifecycle and the page-session API every tool is built on.
//!
//! # Ownership
//!
//! `BrowserManager` owns at most one browser process. It starts Chrome lazily on
//! the first tool that needs a page (a `list_pages` on a cold plugin must not
//! leave a browser running), tracks the page sessions it has attached to, and
//! shuts the browser down on `shutdown` so a stopped plugin does not leak a
//! Chromium per restart.
//!
//! # Pages
//!
//! A page is identified by the Chrome target id (`/json/list`). `ensure_page`
//! resolves the requested page id, or creates one, attaches the WebSocket, and
//! runs the one-time setup (`Page.enable`, `Runtime.enable`, console and
//! lifecycle listeners). Sessions are cached so repeated tool calls reuse the
//! same socket instead of re-attaching.
//!
//! # Time
//!
//! CDP calls are blocking socket round-trips. Tool calls arrive on the plugin's
//! stdin thread, so a blocked call also blocks the queue; every call therefore
//! carries an explicit timeout and fails loudly instead of hanging the harness.

#![allow(dead_code)]

use crate::cdp::{
    self, parse_target_list, PageSocket, Result, TargetInfo, TransportError,
};
use crate::heap::HeapSnapshot;
use crate::snapshot::{render_snapshot, Snapshot};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Configuration, read from the plugin's `settings` table plus environment.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// Run headless (default true: agents rarely have a display).
    pub headless: bool,
    /// Use a throwaway `--user-data-dir` per plugin process. Isolates cookies
    /// and storage from the user's real profile; disabling it lets the agent
    /// use an already-signed-in profile via `user_data_dir`.
    pub isolated: bool,
    /// Explicit browser binary.
    pub chrome_path: Option<String>,
    /// Persistent profile directory (only with `isolated = false`).
    pub user_data_dir: Option<String>,
    /// Default viewport `(width, height)`.
    pub viewport: Option<(u32, u32)>,
    /// Per-CDP-command timeout.
    pub protocol_timeout_secs: u64,
    /// How long to wait for Chrome's DevTools endpoint after spawn.
    pub startup_timeout_secs: u64,
    /// Extra arguments appended to the Chrome command line.
    pub extra_args: Vec<String>,
    /// Directory for heap snapshots (defaults to the system temp dir).
    pub snapshot_dir: Option<String>,
    /// Cap on rendered snapshot nodes.
    pub max_snapshot_nodes: usize,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            headless: true,
            isolated: true,
            chrome_path: None,
            user_data_dir: None,
            // Headless Chromium defaults to a small 800x600 surface, which makes
            // responsiveness testing meaningless. Default to a real desktop
            // viewport; `viewport` and the resize_page tool override it.
            viewport: Some((DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT)),
            protocol_timeout_secs: 30,
            startup_timeout_secs: 30,
            extra_args: Vec::new(),
            snapshot_dir: None,
            max_snapshot_nodes: 2500,
        }
    }
}

/// Default browser size when neither settings nor a tool call choose one.
pub const DEFAULT_VIEWPORT_WIDTH: u32 = 1440;
pub const DEFAULT_VIEWPORT_HEIGHT: u32 = 900;

/// Bounds for a requested viewport. Chrome accepts absurd sizes but renders a
/// blank surface or exhausts GPU memory, so clamp to something usable.
pub const MIN_VIEWPORT: u32 = 100;
pub const MAX_VIEWPORT: u32 = 10_000;

fn json_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0 && *f <= u64::MAX as f64)
            .map(|f| f.round() as u64)
    })
}

fn non_empty_str(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl BrowserConfig {
    /// Build from `plugin/settings/get` merged over `KARUCODE_PLUGIN_SETTINGS`.
    pub fn from_settings(settings: &Value) -> Self {
        let mut config = Self::default();
        if let Some(value) = settings.get("headless").and_then(|v| v.as_bool()) {
            config.headless = value;
        }
        if let Some(value) = settings.get("isolated").and_then(|v| v.as_bool()) {
            config.isolated = value;
        }
        if let Some(value) = settings.get("chrome_path").and_then(non_empty_str) {
            config.chrome_path = Some(value);
        }
        if let Some(value) = settings.get("user_data_dir").and_then(non_empty_str) {
            config.user_data_dir = Some(value);
            config.isolated = false;
        }
        if let Some(viewport) = settings.get("viewport") {
            let width = viewport.get("width").and_then(json_u64).unwrap_or(0) as u32;
            let height = viewport.get("height").and_then(json_u64).unwrap_or(0) as u32;
            if width > 0 && height > 0 {
                config.viewport = Some((
                    width.clamp(MIN_VIEWPORT, MAX_VIEWPORT),
                    height.clamp(MIN_VIEWPORT, MAX_VIEWPORT),
                ));
            }
        }
        let has_viewport_obj = settings.get("viewport").is_some_and(|v| v.is_object());
        let (mut vw, mut vh) = config
            .viewport
            .unwrap_or((DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT));
        if let Some(w) = settings.get("viewport_width").and_then(json_u64) {
            let w = (w as u32).clamp(MIN_VIEWPORT, MAX_VIEWPORT);
            if !has_viewport_obj || w != DEFAULT_VIEWPORT_WIDTH {
                vw = w;
            }
        }
        if let Some(h) = settings.get("viewport_height").and_then(json_u64) {
            let h = (h as u32).clamp(MIN_VIEWPORT, MAX_VIEWPORT);
            if !has_viewport_obj || h != DEFAULT_VIEWPORT_HEIGHT {
                vh = h;
            }
        }
        config.viewport = Some((vw, vh));
        if let Some(value) = settings.get("protocol_timeout_secs").and_then(json_u64) {
            config.protocol_timeout_secs = value.clamp(1, 600);
        }
        if let Some(value) = settings.get("startup_timeout_secs").and_then(json_u64) {
            config.startup_timeout_secs = value.clamp(1, 600);
        }
        if let Some(extra) = settings.get("extra_args") {
            if let Some(list) = extra.as_array() {
                config.extra_args = list
                    .iter()
                    .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
                    .map(str::to_string)
                    .collect();
            } else if let Some(text) = extra.as_str() {
                config.extra_args = text.split_whitespace().map(str::to_string).collect();
            }
        }
        if let Some(value) = settings.get("snapshot_dir").and_then(non_empty_str) {
            config.snapshot_dir = Some(value);
        }
        if let Some(value) = settings.get("max_snapshot_nodes").and_then(json_u64) {
            config.max_snapshot_nodes = (value as usize).clamp(50, 100_000);
        }
        config
    }
}

/// An attached page: the Chrome target plus its live socket.
pub struct PageSession {
    pub target: TargetInfo,
    pub socket: PageSocket,
    /// Frame id of the main frame, learned from `Page.getFrameTree`.
    pub main_frame_id: String,
    /// When the socket was attached.
    pub attached_at: Instant,
}

impl PageSession {
    /// Numeric page id shown to the model: a stable short hash of the target id
    /// so ids survive restarts of the plugin and look like the upstream tool's
    /// `pageId`.
    pub fn page_id(&self) -> i64 {
        page_id_for(&self.target.id)
    }

    pub fn describe(&self) -> String {
        format!(
            "pageId={} url={} title={}",
            self.page_id(),
            if self.target.url.is_empty() {
                "about:blank"
            } else {
                &self.target.url
            },
            if self.target.title.is_empty() {
                "(untitled)"
            } else {
                &self.target.title
            }
        )
    }
}

/// Deterministic page id from a Chrome target id.
///
/// FNV-1a over the target id, masked to 31 bits and kept in `1..=2_000_000_000`
/// so it fits the `pageId` integer shape the tools accept.
pub fn page_id_for(target_id: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in target_id.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % 2_000_000_000) as i64 + 1
}

/// A running browser plus its page sessions.
pub struct BrowserManager {
    pub config: BrowserConfig,
    process: Option<std::process::Child>,
    port: u16,
    user_data_dir: Option<PathBuf>,
    pages: HashMap<String, PageSession>, // key: Chrome target id
    /// Page ids create order; used for "the active page" fallback.
    last_active: Option<String>,
    /// Heap snapshots taken this session, keyed by a short id.
    snapshots: HashMap<String, PathBuf>,
    next_snapshot_id: u64,
    /// Most recent trace file, when tracing was used.
    pub last_trace: Option<PathBuf>,
}

impl BrowserManager {
    pub fn new(config: BrowserConfig) -> Self {
        Self {
            config,
            process: None,
            port: 0,
            user_data_dir: None,
            pages: HashMap::new(),
            last_active: None,
            snapshots: HashMap::new(),
            next_snapshot_id: 1,
            last_trace: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.process.is_some()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The throwaway profile directory, when isolation is on.
    pub fn profile_dir(&self) -> Option<&std::path::Path> {
        self.user_data_dir.as_deref()
    }

    /// Where screenshots are written: `snapshot_dir` when configured, else the
    /// system temp directory.
    pub fn screenshot_dir(&self) -> PathBuf {
        self.config
            .snapshot_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.protocol_timeout_secs)
    }

    /// Start Chrome if it is not running, and wait for its DevTools endpoint.
    pub fn ensure_browser(&mut self) -> Result<()> {
        if self.is_running() {
            // A dead child leaves `start_all`'s process handle in place; reap it
            // so the next spawn starts clean.
            if let Some(child) = &mut self.process {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".into());
                        self.process = None;
                        self.pages.clear();
                        return Err(TransportError::Closed(format!(
                            "the browser exited (status {code}); it will be restarted on the \
                             next call"
                        )));
                    }
                    Ok(None) => return Ok(()),
                    Err(e) => {
                        return Err(TransportError::Io(format!(
                            "could not check the browser process: {e}"
                        )))
                    }
                }
            }
            return Ok(());
        }

        let binary = cdp::find_chrome_binary(self.config.chrome_path.as_deref()).ok_or_else(|| {
            TransportError::Io(
                "no Chrome or Chromium binary found. Install Chrome, or set \
                 `chrome_path` in the plugin settings to the executable."
                    .into(),
            )
        })?;

        let port = cdp::free_port()?;
        let user_data_dir = if self.config.isolated {
            let dir = std::env::temp_dir().join(format!(
                "kc-chrome-devtools-{}-{}",
                std::process::id(),
                port
            ));
            std::fs::create_dir_all(&dir).map_err(|e| {
                TransportError::Io(format!("could not create {}: {e}", dir.display()))
            })?;
            Some(dir)
        } else {
            self.config.user_data_dir.clone().map(PathBuf::from)
        };

        let mut args: Vec<String> = vec![
            format!("--remote-debugging-port={port}"),
            "--remote-debugging-address=127.0.0.1".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-background-networking".into(),
            "--disable-background-timer-throttling".into(),
            "--disable-backgrounding-occluded-windows".into(),
            "--disable-renderer-backgrounding".into(),
            "--disable-features=Translate".into(),
            // A local profile must not show the "restore pages?" bubble.
            "--hide-crash-restore-bubble".into(),
        ];
        if self.config.headless {
            args.push("--headless=new".into());
            args.push("--hide-scrollbars".into());
            args.push("--mute-audio".into());
        }
        if let Some(dir) = &user_data_dir {
            args.push(format!("--user-data-dir={}", dir.display()));
        }
        // Always pass a size. Without `--window-size` headless Chromium picks an
        // arbitrary small surface (780x493 here), which is exactly the "stuck at
        // 780p" symptom: the window never gets the size anyone asked for.
        let (width, height) = self
            .config
            .viewport
            .unwrap_or((DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT));
        args.push(format!(
            "--window-size={},{}",
            width.clamp(MIN_VIEWPORT, MAX_VIEWPORT),
            height.clamp(MIN_VIEWPORT, MAX_VIEWPORT)
        ));
        args.extend(self.config.extra_args.iter().cloned());
        args.push("about:blank".into());

        let child = cdp::spawn_detached(&binary, &args)?;
        self.process = Some(child);
        self.port = port;
        self.user_data_dir = user_data_dir;

        match cdp::wait_for_browser(port, Duration::from_secs(self.config.startup_timeout_secs)) {
            Ok(_) => {}
            Err(err) => {
                // Do not leave a half-started browser behind.
                if let Some(mut child) = self.process.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                self.port = 0;
                self.cleanup_profile();
                return Err(err);
            }
        }
        Ok(())
    }

    /// Remove the throwaway profile directory.
    ///
    /// Chrome's zygote/GPU children outlive the main process by a few
    /// milliseconds and keep writing into the profile, so a single
    /// `remove_dir_all` can fail with `ENOTEMPTY`/`EBUSY` — or, worse, succeed
    /// and then have a dying child recreate `Default/Cache` behind us. So we
    /// remove, then confirm the directory is still gone, retrying until it is.
    fn cleanup_profile(&mut self) {
        let Some(dir) = self.user_data_dir.take() else {
            return;
        };
        if !self.config.isolated {
            return;
        }
        for attempt in 0..40 {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => {}
            }
            // Give a dying child a moment, then check it did not come back.
            std::thread::sleep(Duration::from_millis(50));
            if !dir.exists() {
                return;
            }
            if attempt == 39 {
                // Not fatal: the browser is gone and the path is in /tmp.
                eprintln!(
                    "kc-chrome-devtools: could not remove the profile {}",
                    dir.display()
                );
            }
        }
    }

    /// Targets Chrome currently knows about.
    pub fn targets(&self) -> Result<Vec<TargetInfo>> {
        if self.port == 0 {
            return Ok(Vec::new());
        }
        let body = cdp::http_get("127.0.0.1", self.port, "/json/list")?;
        parse_target_list(&body)
    }

    /// Page targets that have a debugger socket, excluding devtools/extension
    /// surfaces the model did not ask for.
    pub fn page_targets(&self) -> Result<Vec<TargetInfo>> {
        Ok(self
            .targets()?
            .into_iter()
            .filter(|t| t.target_type == "page" && !t.web_socket_debugger_url.is_empty())
            .collect())
    }

    pub fn browser_version(&self) -> Result<cdp::BrowserInfo> {
        if self.port == 0 {
            return Err(TransportError::Closed("the browser is not running".into()));
        }
        let body = cdp::http_get("127.0.0.1", self.port, "/json/version")?;
        cdp::parse_version(&body)
    }

    /// Resolve a `pageId` to an attached session, returning the resolved page id.
    ///
    /// `None` means "the active page", creating one when no page exists.
    /// Borrowing the session out of the manager would conflict with the
    /// `&mut self` command methods below, so callers get an id and pass it back.
    pub fn ensure_page(&mut self, page_id: Option<i64>) -> Result<i64> {
        self.ensure_browser()?;
        let targets = self.page_targets()?;

        let target = match page_id {
            Some(id) => targets
                .into_iter()
                .find(|t| page_id_for(&t.id) == id)
                .ok_or_else(|| {
                    TransportError::Protocol(format!(
                        "no page with pageId {id}. Call list_pages to see the open pages."
                    ))
                })?,
            None => {
                // Prefer the last active page, then the first real page, then a
                // brand-new one.
                let existing = self
                    .last_active
                    .as_ref()
                    .and_then(|id| targets.iter().find(|t| &t.id == id).cloned())
                    .or_else(|| targets.first().cloned());
                match existing {
                    Some(target) => target,
                    None => self.create_page("about:blank", true)?,
                }
            }
        };

        self.last_active = Some(target.id.clone());
        let id = page_id_for(&target.id);
        self.attach(target.id)?;
        Ok(id)
    }

    /// The cached session for a page id, if it is attached.
    pub fn session(&self, page_id: i64) -> Option<&PageSession> {
        self.pages.values().find(|s| s.page_id() == page_id)
    }

    /// Target id for a page id, if attached.
    pub fn target_id_for(&self, page_id: i64) -> Option<String> {
        self.pages
            .values()
            .find(|s| s.page_id() == page_id)
            .map(|s| s.target.id.clone())
    }

    /// Attach (or reuse) a session and hand back a mutable reference to it.
    fn attach_and_get(&mut self, target_id: &str) -> Result<&mut PageSession> {
        self.attach(target_id.to_string())?;
        self.pages
            .get_mut(target_id)
            .ok_or_else(|| TransportError::Protocol("page session disappeared".into()))
    }

    fn attach(&mut self, target_id: String) -> Result<()> {
        // Reuse a healthy session; drop a dead one and re-attach.
        if let Some(existing) = self.pages.get(&target_id) {
            if !existing.socket.is_dead() {
                return Ok(());
            }
        }
        self.pages.remove(&target_id);

        let targets = self.page_targets()?;
        let target = targets
            .into_iter()
            .find(|t| t.id == target_id)
            .ok_or_else(|| {
                TransportError::Protocol(format!(
                    "page target `{target_id}` disappeared before it could be attached"
                ))
            })?;

        let mut socket = PageSocket::connect(&target.web_socket_debugger_url, self.timeout())?;
        // Enable the domains the tools depend on. Failures here are worth
        // surfacing: without Runtime.enable, evaluate_script cannot work.
        socket.send("Page.enable", json!({}))?;
        socket.send("Runtime.enable", json!({}))?;
        // Best effort: not all builds support these, and the tools that need
        // them enable them on demand.
        let _ = socket.send("Network.enable", json!({}));
        let _ = socket.send("Log.enable", json!({}));
        let _ = socket.send("DOM.enable", json!({}));
        let _ = socket.send("Page.setInterceptFileChooserDialog", json!({"enabled": false}));

        let frame_tree = socket.send("Page.getFrameTree", json!({}))?;
        let main_frame_id = frame_tree
            .get("frameTree")
            .and_then(|f| f.get("frame"))
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // Console messages are delivered as events; the ones emitted before we
        // attached are gone. Say so once per session instead of pretending the
        // console is complete.
        let session = PageSession {
            target,
            socket,
            main_frame_id,
            attached_at: Instant::now(),
        };
        self.pages.insert(target_id, session);
        Ok(())
    }

    /// Open a new tab and attach it.
    pub fn create_page(&mut self, url: &str, foreground: bool) -> Result<TargetInfo> {
        self.ensure_browser()?;
        let before: Vec<String> = self.page_targets()?.into_iter().map(|t| t.id).collect();
        // `PUT /json/new?url=` opens a tab in the running browser. The endpoint
        // only accepts PUT in current Chrome versions.
        let encoded = url_encode(url);
        let path = format!("/json/new?{encoded}");
        let body = cdp::http_put("127.0.0.1", self.port, &path).or_else(|_| {
            // Older builds answer GET; try it so we work on both.
            cdp::http_get("127.0.0.1", self.port, &path)
        })?;
        let created: Value = serde_json::from_str(&body).map_err(|e| {
            TransportError::Protocol(format!("/json/new returned non-JSON: {e}"))
        })?;
        let _ = foreground; // Chrome focuses new tabs by default.

        let target_id = created
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // Prefer the explicit reply; fall back to "the target that appeared".
        let target = match target_id {
            Some(id) => self.page_targets()?.into_iter().find(|t| t.id == id),
            None => self
                .page_targets()?
                .into_iter()
                .find(|t| !before.contains(&t.id)),
        }
        .ok_or_else(|| {
            TransportError::Protocol(
                "the browser accepted /json/new but no new page target appeared".into(),
            )
        })?;

        self.last_active = Some(target.id.clone());
        Ok(target)
    }

    /// Close a page target by id.
    pub fn close_page_by_id(&mut self, target_id: &str) -> Result<()> {
        let path = format!("/json/close/{target_id}");
        cdp::http_get("127.0.0.1", self.port, &path)?;
        if let Some(session) = self.pages.remove(target_id) {
            session.socket.close();
        }
        if self.last_active.as_deref() == Some(target_id) {
            self.last_active = None;
        }
        Ok(())
    }

    /// Resolve a page id (or the active page) to its live session.
    fn session_mut(&mut self, page_id: Option<i64>) -> Result<&mut PageSession> {
        let id = self.ensure_page(page_id)?;
        let target_id = self.target_id_for(id).ok_or_else(|| {
            TransportError::Protocol(format!("page {id} is no longer attached"))
        })?;
        self.pages
            .get_mut(&target_id)
            .ok_or_else(|| TransportError::Protocol(format!("page {id} is no longer attached")))
    }

    /// Navigate and wait for the load event.
    pub fn navigate(&mut self, page_id: Option<i64>, url: &str, timeout_ms: u64) -> Result<String> {
        let url = url.trim().to_string();
        if url.is_empty() {
            return Err(TransportError::Protocol("navigate_page needs a url".into()));
        }
        let id = self.ensure_page(page_id)?;
        self.session_mut(Some(id))?
            .socket
            .send_with_timeout(
                "Page.navigate",
                json!({"url": url}),
                Duration::from_millis(timeout_ms.max(1000)),
            )?;
        self.wait_for_load(Some(id), timeout_ms)?;
        let url_now = self.current_url(Some(id))?;
        // Update the cached description so list_pages is fresh without a poll.
        if let Some(target_id) = self.target_id_for(id) {
            if let Some(session) = self.pages.get_mut(&target_id) {
                session.target.url = url_now.clone();
            }
        }
        Ok(url_now)
    }

    /// Wait for a `Page.loadEventFired`, tolerating pages that never fire one.
    pub fn wait_for_load(&mut self, page_id: Option<i64>, timeout_ms: u64) -> Result<()> {
        let id = self.ensure_page(page_id)?;
        // Resolve the target once: re-running `ensure_page` inside the poll loop
        // would do an HTTP round-trip every 40ms.
        let Some(target_id) = self.target_id_for(id) else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(500));
        while Instant::now() < deadline {
            let Some(session) = self.pages.get_mut(&target_id) else {
                break;
            };
            session.socket.pump_events(Duration::from_millis(40));
            let saw_load = session
                .socket
                .events()
                .any(|event| event.method == "Page.loadEventFired");
            if saw_load {
                // Consume the load event so a later wait does not see this one.
                session
                    .socket
                    .retain_events(|event| event.method != "Page.loadEventFired");
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // A timeout here is not an error: a page can stay busy (long-polling,
        // video) forever. The caller decides whether that matters.
        Ok(())
    }

    pub fn current_url(&mut self, page_id: Option<i64>) -> Result<String> {
        let session = self.session_mut(page_id)?;
        let result = session.socket.send(
            "Runtime.evaluate",
            json!({"expression": "location.href", "returnByValue": true}),
        )?;
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    pub fn current_title(&mut self, page_id: Option<i64>) -> Result<String> {
        let session = self.session_mut(page_id)?;
        let result = session.socket.send(
            "Runtime.evaluate",
            json!({"expression": "document.title", "returnByValue": true}),
        )?;
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// Drain the console messages captured since the last call.
    ///
    /// `Runtime.consoleAPICalled` and `Log.entryAdded` arrive as events while
    /// other commands run, so this pumps the socket first to pick up anything
    /// still in flight, then returns the formatted lines oldest-first.
    pub fn take_console(&mut self, page_id: Option<i64>) -> Result<Vec<String>> {
        let id = self.ensure_page(page_id)?;
        let Some(target_id) = self.target_id_for(id) else {
            return Ok(Vec::new());
        };
        let Some(session) = self.pages.get_mut(&target_id) else {
            return Ok(Vec::new());
        };
        // A short pump: console output is usually already queued by the time a
        // tool asks for it, and we do not want to stall for a full timeout.
        session.socket.pump_events(Duration::from_millis(120));
        let mut lines = Vec::new();
        for event in session.socket.events() {
            match event.method.as_str() {
                "Runtime.consoleAPICalled" => {
                    let level = event
                        .params
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("log");
                    let text = event
                        .params
                        .get("args")
                        .and_then(|a| a.as_array())
                        .map(|args| {
                            args.iter()
                                .map(describe_remote_object)
                                .map(|v| match v {
                                    Value::String(s) => s,
                                    other => other.to_string(),
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    lines.push(format!("[{level}] {text}"));
                }
                "Log.entryAdded" => {
                    let entry = &event.params["entry"];
                    let level = entry.get("level").and_then(|v| v.as_str()).unwrap_or("info");
                    let text = entry.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let url = entry.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    if url.is_empty() {
                        lines.push(format!("[{level}] {text}"));
                    } else {
                        lines.push(format!("[{level}] {text} ({url})"));
                    }
                }
                _ => {}
            }
        }
        // Consume what we reported so a later call does not repeat it.
        session.socket.retain_events(|event| {
            event.method != "Runtime.consoleAPICalled" && event.method != "Log.entryAdded"
        });
        Ok(lines)
    }

    /// Resize the page viewport at runtime.
    ///
    /// `Emulation.setDeviceMetricsOverride` is what actually changes the layout
    /// viewport; `--window-size` only sets the initial window.
    ///
    /// Mobile emulation has a wrinkle worth knowing: without a
    /// `<meta name="viewport">` tag, Chrome falls back to its legacy 980px
    /// layout viewport, so `innerWidth` will not match the requested width.
    /// That is correct browser behaviour, not a bug, so the returned size is
    /// what the page *actually* reports.
    pub fn resize_page(
        &mut self,
        page_id: Option<i64>,
        width: u32,
        height: u32,
        device_scale_factor: Option<f64>,
        mobile: bool,
    ) -> Result<(u32, u32)> {
        let width = width.clamp(MIN_VIEWPORT, MAX_VIEWPORT);
        let height = height.clamp(MIN_VIEWPORT, MAX_VIEWPORT);
        {
            let session = self.session_mut(page_id)?;
            session.socket.send(
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": width,
                    "height": height,
                    "deviceScaleFactor": device_scale_factor.unwrap_or(0.0),
                    "mobile": mobile,
                }),
            )?;
        }
        // Report what the page sees, so a 980px legacy viewport is visible in
        // the result instead of silently contradicting the request.
        self.viewport_size(page_id).or(Ok((width, height)))
    }

    /// Drop any runtime viewport override, returning to the window's own size.
    pub fn clear_viewport(&mut self, page_id: Option<i64>) -> Result<()> {
        let session = self.session_mut(page_id)?;
        session
            .socket
            .send("Emulation.clearDeviceMetricsOverride", json!({}))?;
        Ok(())
    }

    /// The layout viewport currently in effect, as the page sees it.
    pub fn viewport_size(&mut self, page_id: Option<i64>) -> Result<(u32, u32)> {
        let value = self.evaluate(
            page_id,
            "({w: window.innerWidth, h: window.innerHeight})",
            false,
        )?;
        let width = value.get("w").and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
        let height = value.get("h").and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
        Ok((width, height))
    }

    /// Evaluate an expression in the page's main world.
    ///
    /// Returns the value (with `returnByValue`), or the remote object's
    /// description for non-serialisable results.
    pub fn evaluate(
        &mut self,
        page_id: Option<i64>,
        expression: &str,
        await_promise: bool,
    ) -> Result<Value> {
        let session = self.session_mut(page_id)?;
        let result = session.socket.send(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": await_promise,
                "userGesture": true,
                "replMode": false,
            }),
        )?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|d| d.as_str())
                .or_else(|| exception.get("text").and_then(|t| t.as_str()))
                .unwrap_or("script threw");
            return Err(TransportError::Protocol(format!(
                "the script threw: {text}"
            )));
        }
        let remote = result.get("result").cloned().unwrap_or(Value::Null);
        Ok(describe_remote_object(&remote))
    }

    /// Evaluate and require the result to be a plain JSON value (used by tools
    /// that post-process the output themselves).
    pub fn evaluate_json(&mut self, page_id: Option<i64>, expression: &str) -> Result<Value> {
        self.evaluate(page_id, expression, true)
    }

    /// Take an accessibility snapshot.
    pub fn snapshot(&mut self, page_id: Option<i64>, max_nodes: usize) -> Result<Snapshot> {
        let session = self.session_mut(page_id)?;
        let tree = session
            .socket
            .send("Accessibility.getFullAXTree", json!({}))?;
        let nodes = tree
            .get("nodes")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(render_snapshot(&nodes, max_nodes))
    }

    /// Resolve a snapshot uid to a `(BackendNodeId, RemoteObjectId)` pair for
    /// DOM operations.
    fn resolve_uid(&mut self, page_id: Option<i64>, uid: &str) -> Result<(i64, String)> {
        let snapshot = self.snapshot(page_id, 50_000)?;
        let element = snapshot.element(uid).ok_or_else(|| {
            TransportError::Protocol(format!(
                "no element with uid `{uid}` in the current snapshot. Take a fresh \
                 snapshot with take_snapshot; uids are only valid for the snapshot \
                 they came from."
            ))
        })?;
        let backend_node_id = element.backend_node_id;
        if backend_node_id <= 0 {
            return Err(TransportError::Protocol(format!(
                "uid `{uid}` ({} \"{}\") has no DOM node to act on",
                element.role, element.name
            )));
        }
        let session = self.session_mut(page_id)?;
        let resolved = session
            .socket
            .send("DOM.resolveNode", json!({"backendNodeId": backend_node_id}))?;
        let object_id = resolved
            .get("object")
            .and_then(|o| o.get("objectId"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TransportError::Protocol(format!(
                    "Chrome could not resolve uid `{uid}` to a DOM node (it may be detached)"
                ))
            })?
            .to_string();
        Ok((backend_node_id, object_id))
    }

    /// Click an element by snapshot uid.
    pub fn click(&mut self, page_id: Option<i64>, uid: &str, double: bool) -> Result<()> {
        let (backend_node_id, object_id) = self.resolve_uid(page_id, uid)?;
        let session = self.session_mut(page_id)?;
        // Scroll into view first: Chrome's Input events are viewport-relative,
        // so an off-screen element would get a click in the wrong place.
        let _ = session.socket.send(
            "DOM.scrollIntoViewIfNeeded",
            json!({"backendNodeId": backend_node_id}),
        );
        let box_model = session
            .socket
            .send("DOM.getBoxModel", json!({"backendNodeId": backend_node_id}))
            .ok()
            .and_then(|m| m.get("model").cloned());
        let (x, y) = match box_model.and_then(|m| quads_center(&m)) {
            Some(point) => point,
            None => {
                // No layout box (e.g. an element inside a shadow root or a
                // display:contents wrapper): fall back to a DOM click.
                session.socket.send(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object_id,
                        "functionDeclaration": "function(){ this.click(); return true; }",
                        "returnByValue": true,
                    }),
                )?;
                return Ok(());
            }
        };
        let click_count = if double { 2 } else { 1 };
        for i in 1..=click_count {
            session.socket.send(
                "Input.dispatchMouseEvent",
                json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": i}),
            )?;
            session.socket.send(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": i}),
            )?;
        }
        Ok(())
    }

    /// Focus an element and set its value.
    pub fn fill(&mut self, page_id: Option<i64>, uid: &str, value: &str) -> Result<()> {
        let (_, object_id) = self.resolve_uid(page_id, uid)?;
        let session = self.session_mut(page_id)?;
        // Use the element's own focus + value semantics so <select>, checkbox
        // and contenteditable all behave like a user action.
        session.socket.send(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": FILL_FUNCTION,
                "arguments": [{"value": value}],
                "returnByValue": true,
            }),
        )?;
        Ok(())
    }

    /// Hover an element by uid.
    pub fn hover(&mut self, page_id: Option<i64>, uid: &str) -> Result<()> {
        let (_, object_id) = self.resolve_uid(page_id, uid)?;
        let session = self.session_mut(page_id)?;
        session.socket.send(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": HOVER_FUNCTION,
                "returnByValue": true,
            }),
        )?;
        Ok(())
    }

    /// Drag from one element to another.
    pub fn drag(&mut self, page_id: Option<i64>, from_uid: &str, to_uid: &str) -> Result<()> {
        let (from_backend, _) = self.resolve_uid(page_id, from_uid)?;
        let (to_backend, _) = self.resolve_uid(page_id, to_uid)?;
        let session = self.session_mut(page_id)?;
        let _ = session
            .socket
            .send("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": from_backend}));
        let from_box = session
            .socket
            .send("DOM.getBoxModel", json!({"backendNodeId": from_backend}))
            .ok()
            .and_then(|m| m.get("model").cloned())
            .and_then(|m| quads_center(&m));
        let to_box = session
            .socket
            .send("DOM.getBoxModel", json!({"backendNodeId": to_backend}))
            .ok()
            .and_then(|m| m.get("model").cloned())
            .and_then(|m| quads_center(&m));
        let (Some((fx, fy)), Some((tx, ty))) = (from_box, to_box) else {
            return Err(TransportError::Protocol(
                "could not compute a drag path: one of the elements has no layout box".into(),
            ));
        };
        session.socket.send(
            "Input.dispatchMouseEvent",
            json!({"type": "mousePressed", "x": fx, "y": fy, "button": "left", "clickCount": 1}),
        )?;
        // Intermediate moves matter: HTML5 drag controllers ignore a single jump.
        for step in 1..=8 {
            let ratio = step as f64 / 8.0;
            let x = fx + (tx - fx) * ratio;
            let y = fy + (ty - fy) * ratio;
            session.socket.send(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseMoved", "x": x, "y": y, "button": "left", "buttons": 1}),
            )?;
        }
        session.socket.send(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseReleased", "x": tx, "y": ty, "button": "left", "clickCount": 1}),
        )?;
        Ok(())
    }

    /// Type text into whatever is focused.
    pub fn type_text(&mut self, page_id: Option<i64>, text: &str) -> Result<()> {
        let session = self.session_mut(page_id)?;
        session.socket.send("Input.insertText", json!({"text": text}))?;
        Ok(())
    }

    /// Press a key or chord (`Control+Shift+R`).
    pub fn press_key(&mut self, page_id: Option<i64>, key: &str) -> Result<()> {
        let session = self.session_mut(page_id)?;
        let parts: Vec<&str> = key.split('+').map(|p| p.trim()).collect();
        let (modifiers, key_part) = split_modifiers(&parts);
        let (key_code, code, text) = key_descriptor(key_part);
        if key_code.is_empty() {
            return Err(TransportError::Protocol(format!(
                "unknown key `{key_part}`. Use a DOM key name such as `Enter`, `Escape`, \
                 `Tab`, a single character, or a chord like `Control+a`."
            )));
        }
        let vk: i64 = key_code.parse().unwrap_or(0);
        session.socket.send(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyDown",
                "modifiers": modifiers,
                "key": key_part,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
                "text": text,
            }),
        )?;
        session.socket.send(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "modifiers": modifiers,
                "key": key_part,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
            }),
        )?;
        Ok(())
    }

    /// Take a screenshot of the viewport or the full page.
    ///
    /// `captureBeyondViewport` captures the whole document without touching the
    /// device-metrics override, so a `resize_page` viewport survives a
    /// full-page screenshot instead of being silently cleared.
    pub fn screenshot(
        &mut self,
        page_id: Option<i64>,
        format: &str,
        full_page: bool,
    ) -> Result<Vec<u8>> {
        let session = self.session_mut(page_id)?;
        let mut params = json!({"format": format});
        if full_page {
            params["captureBeyondViewport"] = json!(true);
        }
        let result = session.socket.send("Page.captureScreenshot", params)?;
        let data = result
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| {
                TransportError::Protocol("Page.captureScreenshot returned no data".into())
            })?;
        base64_decode(data)
    }

    /// Pixel dimensions of a PNG or JPEG image, read from its header.
    pub fn screenshot_size(bytes: &[u8]) -> Option<(u32, u32)> {
        // PNG: 8-byte signature, then IHDR with width/height as big-endian u32.
        if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
            let read = |offset: usize| -> u32 {
                u32::from_be_bytes([
                    bytes[offset],
                    bytes[offset + 1],
                    bytes[offset + 2],
                    bytes[offset + 3],
                ])
            };
            return Some((read(16), read(20)));
        }
        // JPEG: walk the markers to the first SOFn segment.
        if bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
            let mut i = 2usize;
            while i + 9 < bytes.len() {
                if bytes[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = bytes[i + 1];
                // SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC).
                if (0xC0..=0xCF).contains(&marker)
                    && marker != 0xC4
                    && marker != 0xC8
                    && marker != 0xCC
                {
                    let height = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                    let width = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                    return Some((width, height));
                }
                let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
                if length < 2 {
                    break;
                }
                i += 2 + length;
            }
        }
        None
    }

    /// Capture a JPEG preview of the viewport, used by `screencast_start`.
    pub fn screencast_frame(&mut self, page_id: Option<i64>) -> Result<Vec<u8>> {
        let session = self.session_mut(page_id)?;
        let result = session.socket.send(
            "Page.captureScreenshot",
            json!({"format": "jpeg", "quality": 60}),
        )?;
        let data = result
            .get("data")
            .and_then(|d| d.as_str())
            .unwrap_or_default();
        base64_decode(data)
    }

    /// Take a V8 heap snapshot and return its id and path.
    pub fn take_heap_snapshot(&mut self, page_id: Option<i64>) -> Result<(String, PathBuf)> {
        let dir = self
            .config
            .snapshot_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| TransportError::Io(format!("could not create {}: {e}", dir.display())))?;
        let id = format!("snap{}", self.next_snapshot_id);
        self.next_snapshot_id += 1;
        let path = dir.join(format!("{id}.heapsnapshot"));

        {
            let session = self.session_mut(page_id)?;
            let mut file = std::fs::File::create(&path).map_err(|e| {
                TransportError::Io(format!("could not create {}: {e}", path.display()))
            })?;
            let _ = session.socket.send("HeapProfiler.enable", json!({}));
            take_snapshot_streaming(&mut session.socket, &mut file)?;
            let _ = session.socket.send("HeapProfiler.disable", json!({}));
        }
        self.snapshots.insert(id.clone(), path.clone());
        Ok((id, path))
    }

    /// Load a snapshot written earlier (by id) or an explicit path.
    pub fn load_snapshot(&self, id_or_path: &str) -> Result<HeapSnapshot> {
        let path = self
            .snapshots
            .get(id_or_path)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| {
                let candidate = PathBuf::from(id_or_path);
                if candidate.is_file() {
                    Ok(candidate)
                } else {
                    Err(TransportError::Protocol(format!(
                        "no heap snapshot `{id_or_path}`. Take one with take_heapsnapshot, or \
                         pass a path to a .heapsnapshot file."
                    )))
                }
            })?;
        let text = std::fs::read_to_string(&path).map_err(|e| {
            TransportError::Io(format!("could not read {}: {e}", path.display()))
        })?;
        let doc: Value = serde_json::from_str(&text).map_err(|e| {
            TransportError::Protocol(format!("{} is not a JSON heap snapshot: {e}", path.display()))
        })?;
        HeapSnapshot::parse(&doc).map_err(TransportError::Protocol)
    }

    pub fn snapshot_path(&self, id: &str) -> Option<&PathBuf> {
        self.snapshots.get(id)
    }

    pub fn known_snapshots(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = self
            .snapshots
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Shut the browser down and remove the throwaway profile.
    pub fn shutdown(&mut self) {
        for (_, session) in self.pages.drain() {
            session.socket.close();
        }
        if let Some(mut child) = self.process.take() {
            // Signal the whole process group: Chrome's helpers outlive the main
            // process and keep writing into the profile.
            cdp::kill_process_group(&mut child);
        }
        self.port = 0;
        self.cleanup_profile();
    }
}

impl Drop for BrowserManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// JS used by `fill`: works for inputs, textareas, selects, checkboxes and
/// contenteditable, and fires the events frameworks listen for.
const FILL_FUNCTION: &str = r#"function(value) {
  const el = this;
  el.focus();
  const tag = el.tagName ? el.tagName.toLowerCase() : '';
  if (tag === 'select') {
    el.value = value;
  } else if (el.type === 'checkbox' || el.type === 'radio') {
    const want = String(value).toLowerCase() === 'true';
    if (el.checked !== want) { el.click(); }
    return true;
  } else if (el.isContentEditable) {
    el.textContent = value;
  } else {
    const proto = Object.getPrototypeOf(el);
    const desc = Object.getOwnPropertyDescriptor(proto, 'value');
    if (desc && desc.set) { desc.set.call(el, value); } else { el.value = value; }
  }
  el.dispatchEvent(new Event('input', {bubbles: true}));
  el.dispatchEvent(new Event('change', {bubbles: true}));
  return true;
}"#;

const HOVER_FUNCTION: &str = r#"function() {
  const el = this;
  const rect = el.getBoundingClientRect();
  const options = {bubbles: true, clientX: rect.left + rect.width / 2,
                   clientY: rect.top + rect.height / 2};
  el.dispatchEvent(new MouseEvent('mouseover', options));
  el.dispatchEvent(new MouseEvent('mouseenter', options));
  el.dispatchEvent(new MouseEvent('mousemove', options));
  return true;
}"#;

/// Split `Control+Shift+R` into CDP modifier bits and the key.
fn split_modifiers<'a>(parts: &'a [&'a str]) -> (i64, &'a str) {
    let mut modifiers = 0i64;
    let mut key = "";
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "alt" => modifiers |= 1,
            "control" | "ctrl" => modifiers |= 2,
            "meta" | "cmd" | "command" => modifiers |= 4,
            "shift" => modifiers |= 8,
            _ => key = part,
        }
    }
    (modifiers, key)
}

/// Map a DOM key name to `(virtual key code, code, text)`.
fn key_descriptor(key: &str) -> (String, String, String) {
    let named: &[(&str, i64, &str)] = &[
        ("Enter", 13, "Enter"),
        ("Tab", 9, "Tab"),
        ("Backspace", 8, "Backspace"),
        ("Escape", 27, "Escape"),
        ("Delete", 46, "Delete"),
        ("ArrowLeft", 37, "ArrowLeft"),
        ("ArrowUp", 38, "ArrowUp"),
        ("ArrowRight", 39, "ArrowRight"),
        ("ArrowDown", 40, "ArrowDown"),
        ("Home", 36, "Home"),
        ("End", 35, "End"),
        ("PageUp", 33, "PageUp"),
        ("PageDown", 34, "PageDown"),
        (" ", 32, "Space"),
        ("Space", 32, "Space"),
    ];
    for (name, code, dom_code) in named {
        if name.eq_ignore_ascii_case(key) {
            return (code.to_string(), dom_code.to_string(), String::new());
        }
    }
    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        let upper = ch.to_ascii_uppercase();
        let code = upper as i64;
        let text = if ch.is_ascii_alphabetic() { ch.to_string() } else { String::new() };
        return (code.to_string(), format!("Key{upper}"), text);
    }
    (String::new(), String::new(), String::new())
}

/// Centre point of a `DOM.getBoxModel` quad.
fn quads_center(model: &Value) -> Option<(f64, f64)> {
    let content = model.get("content").and_then(|c| c.as_array())?;
    if content.len() < 8 {
        return None;
    }
    let mut xs = Vec::with_capacity(4);
    let mut ys = Vec::with_capacity(4);
    for pair in content.chunks(2) {
        xs.push(pair[0].as_f64()?);
        ys.push(pair[1].as_f64()?);
    }
    let x = xs.iter().sum::<f64>() / xs.len() as f64;
    let y = ys.iter().sum::<f64>() / ys.len() as f64;
    Some((x, y))
}

/// Turn a `RemoteObject` into something JSON-serialisable for the model.
pub fn describe_remote_object(remote: &Value) -> Value {
    if let Some(value) = remote.get("value") {
        if !value.is_null() {
            return value.clone();
        }
    }
    if remote.get("unserializableValue").is_some() {
        return remote["unserializableValue"].clone();
    }
    if remote
        .get("subtype")
        .and_then(|s| s.as_str())
        .map(|s| s == "null")
        .unwrap_or(false)
    {
        return Value::Null;
    }
    let description = remote
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    if !description.is_empty() {
        return json!(description);
    }
    if remote.get("type").and_then(|t| t.as_str()) == Some("undefined") {
        return Value::Null;
    }
    // Last resort: hand back the raw remote object so nothing is lost silently.
    remote.clone()
}

/// Stream `HeapProfiler.takeHeapSnapshot` chunks into `file`.
///
/// The command itself does not answer until the snapshot is complete; the data
/// arrives as `HeapProfiler.addHeapSnapshotChunk` events with our id pending.
fn take_snapshot_streaming(socket: &mut PageSocket, file: &mut std::fs::File) -> Result<()> {
    use std::io::Write as _;
    let id_sent = socket.start_command(
        "HeapProfiler.takeHeapSnapshot",
        json!({"reportProgress": false, "captureNumericValue": true}),
    )?;
    loop {
        let message = socket.read_frame()?;
        let Some(value) = message else { continue };
        if value.get("method").and_then(|m| m.as_str())
            == Some("HeapProfiler.addHeapSnapshotChunk")
        {
            if let Some(chunk) = value
                .get("params")
                .and_then(|p| p.get("chunk"))
                .and_then(|c| c.as_str())
            {
                file.write_all(chunk.as_bytes())
                    .map_err(|e| TransportError::Io(format!("could not write the snapshot: {e}")))?;
            }
            continue;
        }
        if value.get("id").and_then(|v| v.as_u64()) == Some(id_sent) {
            if let Some(error) = value.get("error") {
                return Err(TransportError::Protocol(format!(
                    "HeapProfiler.takeHeapSnapshot failed: {}",
                    error.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")
                )));
            }
            return Ok(());
        }
    }
}

/// Percent-encode a query string value for `/json/new`.
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Minimal base64 decoder (RFC 4648, `=` padding, no line breaks expected but
/// tolerated) so screenshots and chunked bodies need no extra dependency.
pub fn base64_decode(input: &str) -> Result<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let Some(six) = value(byte) else {
            return Err(TransportError::Protocol(format!(
                "invalid base64 character `{}` in screenshot data",
                byte as char
            )));
        };
        buffer = (buffer << 6) | six as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_defaults_to_a_real_desktop_size() {
        let config = BrowserConfig::default();
        assert_eq!(
            config.viewport,
            Some((DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT))
        );
        // The old default (None) let headless Chromium fall back to 800x600.
        assert_ne!(config.viewport, None);
    }

    #[test]
    fn viewport_settings_override_the_default() {
        let config = BrowserConfig::from_settings(&json!({
            "viewport": {"width": 390, "height": 844}
        }));
        assert_eq!(config.viewport, Some((390, 844)));
        // A partial or zero viewport keeps the default rather than breaking it.
        let config = BrowserConfig::from_settings(&json!({
            "viewport": {"width": 0, "height": 844}
        }));
        assert_eq!(
            config.viewport,
            Some((DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT))
        );
    }

    #[test]
    fn png_dimensions_are_read_from_the_header() {
        // Minimal PNG header: signature + IHDR length/type + width/height.
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&1920u32.to_be_bytes());
        bytes.extend_from_slice(&1080u32.to_be_bytes());
        assert_eq!(BrowserManager::screenshot_size(&bytes), Some((1920, 1080)));
    }

    #[test]
    fn jpeg_dimensions_are_read_from_the_sof_marker() {
        let mut bytes = vec![0xFF, 0xD8]; // SOI
        // An APP0 segment that must be skipped.
        bytes.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        // SOF0: marker, length, precision, height, width
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&844u16.to_be_bytes());
        bytes.extend_from_slice(&390u16.to_be_bytes());
        bytes.extend_from_slice(&[0x03, 0x01, 0x11, 0x00]);
        assert_eq!(BrowserManager::screenshot_size(&bytes), Some((390, 844)));
    }

    #[test]
    fn unknown_image_bytes_yield_no_dimensions() {
        assert_eq!(BrowserManager::screenshot_size(&[]), None);
        assert_eq!(BrowserManager::screenshot_size(b"not an image"), None);
        // Truncated PNG signature without a header.
        assert_eq!(
            BrowserManager::screenshot_size(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            None
        );
    }

    #[test]
    fn page_ids_are_stable_and_in_range() {
        let a = page_id_for("ABC123");
        assert_eq!(a, page_id_for("ABC123"));
        assert!((1..=2_000_000_001).contains(&a));
        assert_ne!(a, page_id_for("ABC124"));
    }

    #[test]
    fn config_reads_settings_and_env_style_values() {
        let settings = json!({
            "headless": false,
            "chrome_path": "/opt/chrome",
            "viewport": {"width": 1280, "height": 720},
            "protocol_timeout_secs": 0,
            "max_snapshot_nodes": 10,
            "extra_args": ["--foo", 7],
        });
        let config = BrowserConfig::from_settings(&settings);
        assert!(!config.headless);
        assert_eq!(config.chrome_path.as_deref(), Some("/opt/chrome"));
        assert_eq!(config.viewport, Some((1280, 720)));
        // Values are clamped rather than accepted verbatim.
        assert_eq!(config.protocol_timeout_secs, 1);
        assert_eq!(config.max_snapshot_nodes, 50);
        assert_eq!(config.extra_args, vec!["--foo".to_string()]);
    }

    #[test]
    fn setting_a_user_data_dir_disables_isolation() {
        let config = BrowserConfig::from_settings(&json!({"user_data_dir": "/home/me/.config/x"}));
        assert!(!config.isolated);
        assert_eq!(config.user_data_dir.as_deref(), Some("/home/me/.config/x"));
    }

    #[test]
    fn empty_string_defaults_keep_optional_paths_unset_and_isolated() {
        let config = BrowserConfig::from_settings(&json!({
            "chrome_path": "",
            "user_data_dir": "   ",
            "snapshot_dir": "",
            "extra_args": "",
        }));
        assert!(config.isolated);
        assert_eq!(config.chrome_path, None);
        assert_eq!(config.user_data_dir, None);
        assert_eq!(config.snapshot_dir, None);
        assert!(config.extra_args.is_empty());
    }

    #[test]
    fn flat_viewport_extra_args_string_and_float_numbers_work() {
        let config = BrowserConfig::from_settings(&json!({
            "viewport_width": 1280.0,
            "viewport_height": 720.0,
            "protocol_timeout_secs": 45.0,
            "extra_args": "--no-sandbox --disable-gpu",
        }));
        assert_eq!(config.viewport, Some((1280, 720)));
        assert_eq!(config.protocol_timeout_secs, 45);
        assert_eq!(config.extra_args, vec!["--no-sandbox", "--disable-gpu"]);
    }

    #[test]
    fn modifiers_and_keys_map_to_cdp_values() {
        let parts = ["Control", "Shift", "R"];
        let (modifiers, key) = split_modifiers(&parts);
        assert_eq!(modifiers, 2 | 8);
        assert_eq!(key, "R");
        let (code, dom_code, text) = key_descriptor("R");
        assert_eq!(code, "82");
        assert_eq!(dom_code, "KeyR");
        assert_eq!(text, "R");
        let (code, _, _) = key_descriptor("Enter");
        assert_eq!(code, "13");
        let (code, _, _) = key_descriptor("nonsense-key");
        assert!(code.is_empty());
    }

    #[test]
    fn box_model_centre_is_computed() {
        let model = json!({"content": [10.0, 10.0, 30.0, 10.0, 30.0, 50.0, 10.0, 50.0]});
        let (x, y) = quads_center(&model).unwrap();
        assert_eq!(x, 20.0);
        assert_eq!(y, 30.0);
        assert!(quads_center(&json!({"content": [1.0]})).is_none());
    }

    #[test]
    fn remote_objects_are_described_usefully() {
        assert_eq!(
            describe_remote_object(&json!({"type": "string", "value": "hi"})),
            json!("hi")
        );
        assert_eq!(
            describe_remote_object(&json!({"type": "undefined"})),
            Value::Null
        );
        assert_eq!(
            describe_remote_object(&json!({"type": "object", "subtype": "null", "value": null})),
            Value::Null
        );
        assert_eq!(
            describe_remote_object(&json!({"type": "function", "description": "function f(){}"})),
            json!("function f(){}")
        );
        assert_eq!(
            describe_remote_object(&json!({"type": "number", "unserializableValue": "NaN"})),
            json!("NaN")
        );
    }

    #[test]
    fn base64_round_trips_known_vectors() {
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello".to_vec());
        assert_eq!(base64_decode("aGVsbG8gd29ybGQ=").unwrap(), b"hello world".to_vec());
        // Newlines inside base64 (Chrome does not emit them, but be safe).
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello".to_vec());
        let err = base64_decode("!!!!").expect_err("must fail");
        assert!(format!("{err}").contains("invalid base64"));
    }

    #[test]
    fn url_encoding_escapes_query_characters() {
        assert_eq!(url_encode("https://a.example/x?y=1"), "https%3A%2F%2Fa.example%2Fx%3Fy%3D1");
        assert_eq!(url_encode("abc-._~"), "abc-._~");
    }

    #[test]
    fn lifetime_guard_clears_the_browser_on_drop() {
        let mut manager = BrowserManager::new(BrowserConfig::default());
        // Nothing started: shutdown must be a no-op, not a panic.
        manager.shutdown();
        assert!(!manager.is_running());
        assert_eq!(manager.port(), 0);
    }
}