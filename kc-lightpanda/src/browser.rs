//! Lightpanda lifecycle and page-session API used by the plugin tools.
//!
//! Unlike Chromium, Lightpanda has no `/json/new` endpoint and allows one real
//! page target per CDP connection. Every `new_page` therefore gets a dedicated
//! WebSocket, browser context and flattened session. The plugin owns these
//! connections and the lazily-started `lightpanda serve` process, and tears all
//! of them down when the manager is dropped.

#![allow(dead_code)]

use crate::cdp::{self, BrowserInfo, PageSocket, Result, TransportError};
use crate::snapshot::{render_snapshot, ElementRef, Snapshot};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const DEFAULT_VIEWPORT_WIDTH: u32 = 1440;
pub const DEFAULT_VIEWPORT_HEIGHT: u32 = 900;
pub const MIN_VIEWPORT: u32 = 100;
pub const MAX_VIEWPORT: u32 = 10_000;

/// Operator settings delivered as `KARUCODE_PLUGIN_SETTINGS` JSON.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// Explicit `lightpanda` executable. PATH and standard local paths are used
    /// when this is absent; Chromium is never a fallback.
    pub browser_path: Option<String>,
    /// Attach to an existing `ws://host:port/` CDP server instead of spawning.
    /// Pages opened through this plugin still have isolated contexts.
    pub web_socket_url: Option<String>,
    pub viewport: (u32, u32),
    pub protocol_timeout_secs: u64,
    pub startup_timeout_secs: u64,
    pub screenshot_dir: Option<String>,
    pub max_snapshot_nodes: usize,
    pub extra_args: Vec<String>,
    pub obey_robots: bool,
    /// Disabled by default to avoid sending usage telemetry from an agent tool.
    pub telemetry: bool,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            browser_path: None,
            web_socket_url: None,
            viewport: (DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT),
            protocol_timeout_secs: 30,
            startup_timeout_secs: 30,
            screenshot_dir: None,
            max_snapshot_nodes: 2500,
            extra_args: Vec::new(),
            obey_robots: false,
            telemetry: false,
        }
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

fn non_empty_str(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl BrowserConfig {
    pub fn from_settings(settings: &Value) -> Self {
        let mut config = Self::default();
        if let Some(value) = settings.get("browser_path").and_then(non_empty_str) {
            config.browser_path = Some(value);
        }
        if let Some(value) = settings.get("web_socket_url").and_then(non_empty_str) {
            config.web_socket_url = Some(value);
        }
        if let Some(viewport) = settings.get("viewport") {
            let width = viewport.get("width").and_then(json_u64).unwrap_or(0) as u32;
            let height = viewport.get("height").and_then(json_u64).unwrap_or(0) as u32;
            if width > 0 && height > 0 {
                config.viewport = (
                    width.clamp(MIN_VIEWPORT, MAX_VIEWPORT),
                    height.clamp(MIN_VIEWPORT, MAX_VIEWPORT),
                );
            }
        }
        let has_viewport_obj = settings.get("viewport").is_some_and(|v| v.is_object());
        if let Some(w) = settings.get("viewport_width").and_then(json_u64) {
            let w = (w as u32).clamp(MIN_VIEWPORT, MAX_VIEWPORT);
            if !has_viewport_obj || w != DEFAULT_VIEWPORT_WIDTH {
                config.viewport.0 = w;
            }
        }
        if let Some(h) = settings.get("viewport_height").and_then(json_u64) {
            let h = (h as u32).clamp(MIN_VIEWPORT, MAX_VIEWPORT);
            if !has_viewport_obj || h != DEFAULT_VIEWPORT_HEIGHT {
                config.viewport.1 = h;
            }
        }
        if let Some(value) = settings.get("protocol_timeout_secs").and_then(json_u64) {
            config.protocol_timeout_secs = value.clamp(1, 600);
        }
        if let Some(value) = settings.get("startup_timeout_secs").and_then(json_u64) {
            config.startup_timeout_secs = value.clamp(1, 600);
        }
        if let Some(value) = settings.get("screenshot_dir").and_then(non_empty_str) {
            config.screenshot_dir = Some(value);
        }
        if let Some(value) = settings.get("max_snapshot_nodes").and_then(json_u64) {
            config.max_snapshot_nodes = value.clamp(1, 100_000) as usize;
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
        if let Some(value) = settings.get("obey_robots").and_then(|v| v.as_bool()) {
            config.obey_robots = value;
        }
        if let Some(value) = settings.get("telemetry").and_then(|v| v.as_bool()) {
            config.telemetry = value;
        }
        config
    }
}

#[derive(Debug, Clone)]
pub struct PageInfo {
    pub id: i64,
    pub url: String,
    pub title: String,
}

/// One page and its independent CDP connection.
pub struct PageSession {
    pub target_id: String,
    pub session_id: String,
    pub socket: PageSocket,
    pub attached_at: Instant,
    /// Loader id returned by the in-flight Page.navigate. Lightpanda's
    /// lifecycle load event is bound to this id, preventing an about:blank or
    /// previous-document load from releasing a new navigation too early.
    navigation_loader_id: Option<String>,
    /// The last snapshot returned to the model. Element uids resolve against
    /// this exact mapping rather than an independently regenerated tree.
    pub last_snapshot: Option<Snapshot>,
}

pub struct BrowserManager {
    pub config: BrowserConfig,
    process: Option<std::process::Child>,
    port: u16,
    ws_url: String,
    browser_info: Option<BrowserInfo>,
    pages: BTreeMap<i64, PageSession>,
    last_active: Option<i64>,
    next_page_id: i64,
}

impl BrowserManager {
    pub fn new(config: BrowserConfig) -> Self {
        Self {
            ws_url: config.web_socket_url.clone().unwrap_or_default(),
            config,
            process: None,
            port: 0,
            browser_info: None,
            pages: BTreeMap::new(),
            last_active: None,
            next_page_id: 1,
        }
    }

    pub fn is_running(&self) -> bool {
        self.process.is_some() || self.config.web_socket_url.is_some()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Owned browser pid, for diagnostics and lifecycle integration tests.
    /// Remote endpoints deliberately return `None`.
    pub fn browser_pid(&self) -> Option<u32> {
        self.process.as_ref().map(|child| child.id())
    }

    pub fn browser_info(&self) -> Option<&BrowserInfo> {
        self.browser_info.as_ref()
    }

    pub fn screenshot_dir(&self) -> PathBuf {
        self.config
            .screenshot_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.protocol_timeout_secs)
    }

    /// Start `lightpanda serve` only when needed. Existing remote endpoints do
    /// not cause a process launch and are never killed by `shutdown`.
    pub fn ensure_browser(&mut self) -> Result<()> {
        if let Some(child) = &mut self.process {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.close_pages();
                    self.process = None;
                    self.ws_url.clear();
                    self.port = 0;
                    return Err(TransportError::Closed(format!(
                        "Lightpanda exited with status {status}; the next call will restart it"
                    )));
                }
                Ok(None) => return Ok(()),
                Err(e) => {
                    return Err(TransportError::Io(format!(
                        "could not check the Lightpanda process: {e}"
                    )))
                }
            }
        }
        if self.config.web_socket_url.is_some() {
            if !self.ws_url.starts_with("ws://") {
                return Err(TransportError::Protocol(
                    "web_socket_url must start with ws://; this build has no TLS support for wss://"
                        .into(),
                ));
            }
            return Ok(());
        }

        let binary =
            cdp::find_lightpanda_binary(self.config.browser_path.as_deref()).ok_or_else(|| {
                TransportError::Io(
                    "no Lightpanda binary found. Install it from \
                 https://github.com/lightpanda-io/browser or set `browser_path` in the \
                 lightpanda plugin settings."
                        .into(),
                )
            })?;
        let port = cdp::free_port()?;
        let mut args = vec![
            "serve".into(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            port.to_string(),
            "--disable-metrics".into(),
            "--log-level".into(),
            "err".into(),
        ];
        if self.config.obey_robots {
            args.push("--obey-robots".into());
        }
        args.extend(self.config.extra_args.iter().cloned());
        let mut child = cdp::spawn_lightpanda(&binary, &args, self.config.telemetry)?;
        let info = match cdp::wait_for_browser(
            port,
            Duration::from_secs(self.config.startup_timeout_secs),
        ) {
            Ok(info) => info,
            Err(err) => {
                cdp::kill_process_group(&mut child);
                return Err(err);
            }
        };
        self.ws_url = if info.web_socket_debugger_url.is_empty() {
            format!("ws://127.0.0.1:{port}/")
        } else {
            info.web_socket_debugger_url.clone()
        };
        self.browser_info = Some(info);
        self.port = port;
        self.process = Some(child);
        Ok(())
    }

    pub fn browser_version(&mut self) -> Result<BrowserInfo> {
        self.ensure_browser()?;
        self.browser_info.clone().ok_or_else(|| {
            TransportError::Protocol(
                "the remote endpoint was supplied directly; Browser.getVersion is per page".into(),
            )
        })
    }

    fn close_pages(&mut self) {
        for (_, page) in std::mem::take(&mut self.pages) {
            page.socket.close();
        }
        self.last_active = None;
    }

    fn prune_disconnected_pages(&mut self) {
        let dead: Vec<i64> = self
            .pages
            .iter()
            .filter(|(_, page)| page.socket.is_dead())
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            if let Some(page) = self.pages.remove(&id) {
                page.socket.close();
            }
            if self.last_active == Some(id) {
                self.last_active = None;
            }
        }
    }

    /// Create a page on a fresh CDP connection. Per-connection target ids may
    /// repeat in Lightpanda, so tool-facing page ids come from our monotonic
    /// counter rather than from `FID-0000000001`.
    pub fn create_page(&mut self, url: &str, foreground: bool) -> Result<PageInfo> {
        self.ensure_browser()?;
        if url.trim().is_empty() {
            return Err(TransportError::Protocol("new_page needs a url".into()));
        }
        let id = self.next_page_id;
        self.next_page_id = self
            .next_page_id
            .checked_add(1)
            .ok_or_else(|| TransportError::Protocol("page id space exhausted".into()))?;
        let previous_active = self.last_active;
        let mut socket = PageSocket::connect(&self.ws_url, self.timeout())?;

        // These three commands must be root-level. Lightpanda's startup session
        // has no page; attaching this flattened session changes that.
        let context = socket.send_root("Target.createBrowserContext", json!({}))?;
        let context_id = context
            .get("browserContextId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TransportError::Protocol("Target.createBrowserContext returned no id".into())
            })?;
        let created = socket.send_root(
            "Target.createTarget",
            json!({"url": "about:blank", "browserContextId": context_id}),
        )?;
        let target_id = created
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TransportError::Protocol("Target.createTarget returned no id".into()))?
            .to_string();
        let attached = socket.send_root(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
        )?;
        let session_id = attached
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TransportError::Protocol("Target.attachToTarget returned no session id".into())
            })?
            .to_string();
        socket.use_session(session_id.clone());

        socket.send("Page.enable", json!({}))?;
        socket.send("Runtime.enable", json!({}))?;
        socket.send("Network.enable", json!({}))?;
        socket.send("Log.enable", json!({}))?;
        socket.send("DOM.enable", json!({}))?;
        socket.send("Accessibility.enable", json!({}))?;
        socket.send("Page.setLifecycleEventsEnabled", json!({"enabled": true}))?;
        // Lightpanda reuses a virgin about:blank frame in place. That reuses
        // LID-0000000001 from its initial load, which can confuse a load waiter.
        // A harmless main-world evaluation selects the ordinary root-navigation
        // path (Frame.zig's canNavigateInPlace) with a fresh document/loader and
        // consistent DOM visibility for subsequent Runtime commands.
        socket.send(
            "Runtime.evaluate",
            json!({"expression": "undefined", "returnByValue": true}),
        )?;
        if foreground {
            socket.send_root("Target.activateTarget", json!({"targetId": target_id}))?;
        }
        let (width, height) = self.config.viewport;
        socket.send(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": width, "height": height, "deviceScaleFactor": 1, "mobile": false}),
        )?;
        self.pages.insert(
            id,
            PageSession {
                target_id: target_id.clone(),
                session_id,
                socket,
                attached_at: Instant::now(),
                navigation_loader_id: None,
                last_snapshot: None,
            },
        );
        self.last_active = Some(id);

        let info = if url == "about:blank" {
            PageInfo {
                id,
                url: url.into(),
                title: String::new(),
            }
        } else {
            match self.navigate(Some(id), url, self.timeout().as_millis() as u64) {
                Ok(url_now) => {
                    let title = self.current_title(Some(id)).unwrap_or_default();
                    PageInfo {
                        id,
                        url: url_now,
                        title,
                    }
                }
                Err(err) => {
                    if let Some(page) = self.pages.remove(&id) {
                        page.socket.close();
                    }
                    self.last_active = previous_active;
                    return Err(err);
                }
            }
        };
        self.last_active = if foreground {
            Some(id)
        } else {
            previous_active
        };
        Ok(info)
    }

    /// All pages owned by this plugin process, newest/oldest ordering fixed by id.
    /// Other CDP clients connected to a shared remote endpoint are intentionally
    /// not included: they are separate isolated Lightpanda sessions.
    pub fn page_targets(&mut self) -> Result<Vec<PageInfo>> {
        self.ensure_browser()?;
        self.prune_disconnected_pages();
        let ids: Vec<i64> = self.pages.keys().copied().collect();
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let value = {
                let page = self.pages.get_mut(&id).ok_or_else(|| {
                    TransportError::Protocol(format!("page {id} disappeared while listing pages"))
                })?;
                page.socket.send(
                    "Runtime.evaluate",
                    json!({"expression": "({url: location.href, title: document.title})", "returnByValue": true}),
                )
            };
            let value = match value {
                Ok(value) => value,
                Err(err) => {
                    if matches!(
                        err,
                        TransportError::Closed(_)
                            | TransportError::WebSocket(_)
                            | TransportError::Io(_)
                    ) {
                        // A closed page is gone, not a reason to lose discovery of
                        // every remaining healthy page forever.
                        self.prune_disconnected_pages();
                        continue;
                    }
                    return Err(err);
                }
            };
            let remote = value.get("result").cloned().unwrap_or(Value::Null);
            let value = describe_remote_object(&remote);
            result.push(PageInfo {
                id,
                url: value
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("about:blank")
                    .into(),
                title: value
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
            });
        }
        Ok(result)
    }

    pub fn session(&self, page_id: i64) -> Option<&PageSession> {
        self.pages.get(&page_id)
    }

    pub fn target_id_for(&self, page_id: i64) -> Option<String> {
        self.pages.get(&page_id).map(|p| p.target_id.clone())
    }

    pub fn ensure_page(&mut self, page_id: Option<i64>) -> Result<i64> {
        self.ensure_browser()?;
        self.prune_disconnected_pages();
        let id = match page_id {
            Some(id) if self.pages.contains_key(&id) => id,
            Some(id) => {
                return Err(TransportError::Protocol(format!(
                    "no page with pageId {id}. Call list_pages to see the open pages."
                )))
            }
            None => self
                .last_active
                .filter(|id| self.pages.contains_key(id))
                .or_else(|| self.pages.keys().next_back().copied())
                .map(Ok)
                .unwrap_or_else(|| self.create_page("about:blank", true).map(|p| p.id))?,
        };
        self.last_active = Some(id);
        Ok(id)
    }

    fn session_mut(&mut self, page_id: Option<i64>) -> Result<&mut PageSession> {
        let id = self.ensure_page(page_id)?;
        self.pages
            .get_mut(&id)
            .ok_or_else(|| TransportError::Protocol(format!("page {id} is no longer attached")))
    }

    pub fn navigate(&mut self, page_id: Option<i64>, url: &str, timeout_ms: u64) -> Result<String> {
        let url = url.trim();
        if url.is_empty() {
            return Err(TransportError::Protocol("navigate_page needs a url".into()));
        }
        let id = self.ensure_page(page_id)?;
        {
            let session = self.session_mut(Some(id))?;
            session.last_snapshot = None;
            session.socket.retain_events(|event| {
                event.method != "Page.loadEventFired"
                    && event.method != "Page.frameStoppedLoading"
                    && event.method != "Page.lifecycleEvent"
            });
            let value = session.socket.send_with_timeout(
                "Page.navigate",
                json!({"url": url}),
                Duration::from_millis(timeout_ms.max(1000)),
            )?;
            session.navigation_loader_id = value
                .get("loaderId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if let Some(error) = value
                .get("errorText")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                return Err(TransportError::Protocol(format!(
                    "navigation failed: {error}"
                )));
            }
        }
        self.wait_for_load(Some(id), timeout_ms)?;
        self.current_url(Some(id))
    }

    /// Wait for this navigation's loaded-and-settled lifecycle event. The
    /// response to Page.navigate means navigation commit, not document load.
    /// `settled` is emitted by Lightpanda after load once tracked work is done;
    /// its loader id prevents an earlier about:blank event from winning a race.
    pub fn wait_for_load(&mut self, page_id: Option<i64>, timeout_ms: u64) -> Result<()> {
        let id = self.ensure_page(page_id)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(500));
        while Instant::now() < deadline {
            let session = self.pages.get_mut(&id).ok_or_else(|| {
                TransportError::Closed("the page disappeared during navigation".into())
            })?;
            session.socket.pump_events(Duration::from_millis(40));
            if session.socket.is_dead() {
                return Err(TransportError::Closed(format!(
                    "the page disconnected during navigation: {}",
                    session.socket.dead_reason().unwrap_or("connection closed")
                )));
            }
            let loader_id = session.navigation_loader_id.clone();
            let saw_load = session.socket.events().any(|event| {
                if let Some(loader_id) = loader_id.as_deref() {
                    event.method == "Page.lifecycleEvent"
                        && event.params.get("name").and_then(|v| v.as_str()) == Some("settled")
                        && event.params.get("loaderId").and_then(|v| v.as_str()) == Some(loader_id)
                } else {
                    event.method == "Page.loadEventFired"
                        || event.method == "Page.frameStoppedLoading"
                }
            });
            if saw_load {
                // Confirm the lifecycle event with the new document's own
                // Runtime result before exposing the page to subsequent tools.
                let ready = session.socket.send(
                    "Runtime.evaluate",
                    json!({"expression": "document.readyState", "returnByValue": true}),
                )?;
                let complete = ready
                    .get("result")
                    .and_then(|r| r.get("value"))
                    .and_then(|v| v.as_str())
                    == Some("complete");
                if !complete {
                    continue;
                }
                session.socket.retain_events(|event| {
                    event.method != "Page.loadEventFired"
                        && event.method != "Page.frameStoppedLoading"
                        && !(event.method == "Page.lifecycleEvent"
                            && event.params.get("name").and_then(|v| v.as_str()) == Some("settled")
                            && event.params.get("loaderId").and_then(|v| v.as_str())
                                == loader_id.as_deref())
                });
                session.navigation_loader_id = None;
                return Ok(());
            }
        }
        Err(TransportError::Timeout(format!(
            "the page did not finish loading within {timeout_ms}ms"
        )))
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

    /// Drain console events since the previous call, oldest first.
    pub fn take_console(&mut self, page_id: Option<i64>) -> Result<Vec<String>> {
        let session = self.session_mut(page_id)?;
        session.socket.pump_events(Duration::from_millis(120));
        if session.socket.is_dead() {
            return Err(TransportError::Closed(format!(
                "the page disconnected while reading the console: {}",
                session.socket.dead_reason().unwrap_or("connection closed")
            )));
        }
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
                    let level = entry
                        .get("level")
                        .and_then(|v| v.as_str())
                        .unwrap_or("info");
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
        session.socket.retain_events(|event| {
            event.method != "Runtime.consoleAPICalled" && event.method != "Log.entryAdded"
        });
        Ok(lines)
    }

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
        self.session_mut(page_id)?.socket.send(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": device_scale_factor.unwrap_or(1.0),
                "mobile": mobile,
            }),
        )?;
        self.viewport_size(page_id)
    }

    pub fn clear_viewport(&mut self, page_id: Option<i64>) -> Result<()> {
        self.session_mut(page_id)?
            .socket
            .send("Emulation.clearDeviceMetricsOverride", json!({}))?;
        Ok(())
    }

    pub fn viewport_size(&mut self, page_id: Option<i64>) -> Result<(u32, u32)> {
        let value = self.evaluate(
            page_id,
            "({w: window.innerWidth, h: window.innerHeight})",
            false,
        )?;
        Ok((
            value.get("w").and_then(|v| v.as_f64()).unwrap_or(0.0) as u32,
            value.get("h").and_then(|v| v.as_f64()).unwrap_or(0.0) as u32,
        ))
    }

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
            }),
        )?;
        check_exception(&result)?;
        Ok(describe_remote_object(
            result.get("result").unwrap_or(&Value::Null),
        ))
    }

    pub fn evaluate_json(&mut self, page_id: Option<i64>, expression: &str) -> Result<Value> {
        self.evaluate(page_id, expression, true)
    }

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
        let snapshot = render_snapshot(&nodes, max_nodes.max(1));
        session.last_snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }

    /// Resolve a uid from the most recent snapshot this tool actually returned.
    fn resolve_uid(&mut self, page_id: Option<i64>, uid: &str) -> Result<(i64, String)> {
        let session = self.session_mut(page_id)?;
        let element: &ElementRef = session
            .last_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.element(uid))
            .ok_or_else(|| {
                TransportError::Protocol(format!(
                    "no element with uid `{uid}` in the last returned snapshot. Call \
                     take_snapshot on this page first; uids are page- and snapshot-specific."
                ))
            })?;
        let backend_node_id = element.backend_node_id;
        if backend_node_id <= 0 {
            return Err(TransportError::Protocol(format!(
                "uid `{uid}` ({} \"{}\") has no DOM node to act on",
                element.role, element.name
            )));
        }
        let resolved = session
            .socket
            .send("DOM.resolveNode", json!({"backendNodeId": backend_node_id}))?;
        let object_id = resolved
            .get("object")
            .and_then(|o| o.get("objectId"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TransportError::Protocol(format!(
                    "Lightpanda could not resolve uid `{uid}`; the page may have changed. \
                     Take a fresh snapshot."
                ))
            })?
            .to_string();
        Ok((backend_node_id, object_id))
    }

    pub fn click(&mut self, page_id: Option<i64>, uid: &str, double: bool) -> Result<()> {
        let (backend_node_id, object_id) = self.resolve_uid(page_id, uid)?;
        let session = self.session_mut(page_id)?;
        let _ = session.socket.send(
            "DOM.scrollIntoViewIfNeeded",
            json!({"backendNodeId": backend_node_id}),
        );
        let box_model = session
            .socket
            .send("DOM.getBoxModel", json!({"backendNodeId": backend_node_id}))
            .ok()
            .and_then(|m| m.get("model").cloned());
        let (x, y) = match box_model.as_ref().and_then(quads_center) {
            Some(point) => point,
            None => {
                // Accessibility nodes need not correspond to a laid-out element.
                // A DOM click is still meaningful and avoids claiming a random
                // coordinate click succeeded.
                let count = if double { 2 } else { 1 };
                for _ in 0..count {
                    let result = session.socket.send(
                        "Runtime.callFunctionOn",
                        json!({
                            "objectId": object_id,
                            "functionDeclaration": "function(){ this.click(); return true; }",
                            "returnByValue": true,
                        }),
                    )?;
                    check_exception(&result)?;
                }
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

    pub fn fill(&mut self, page_id: Option<i64>, uid: &str, value: &str) -> Result<()> {
        let (_, object_id) = self.resolve_uid(page_id, uid)?;
        let result = self.session_mut(page_id)?.socket.send(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": FILL_FUNCTION,
                "arguments": [{"value": value}],
                "returnByValue": true,
            }),
        )?;
        check_exception(&result)?;
        Ok(())
    }

    pub fn hover(&mut self, page_id: Option<i64>, uid: &str) -> Result<()> {
        let (_, object_id) = self.resolve_uid(page_id, uid)?;
        let result = self.session_mut(page_id)?.socket.send(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": HOVER_FUNCTION,
                "returnByValue": true,
            }),
        )?;
        check_exception(&result)?;
        Ok(())
    }

    /// Lightpanda currently renders textual content, not Chromium pixels. PNG
    /// is the only supported CDP screenshot format at the tested version.
    pub fn screenshot(
        &mut self,
        page_id: Option<i64>,
        format: &str,
        full_page: bool,
    ) -> Result<Vec<u8>> {
        if format != "png" {
            return Err(TransportError::Protocol(format!(
                "Lightpanda supports only PNG screenshots here; `{format}` is unavailable"
            )));
        }
        let result = self.session_mut(page_id)?.socket.send(
            "Page.captureScreenshot",
            json!({"format": "png", "captureBeyondViewport": full_page}),
        )?;
        let data = result.get("data").and_then(|d| d.as_str()).ok_or_else(|| {
            TransportError::Protocol("Page.captureScreenshot returned no data".into())
        })?;
        base64_decode(data)
    }

    /// Pixel dimensions of the generated PNG (or a JPEG, for utility tests).
    pub fn screenshot_size(bytes: &[u8]) -> Option<(u32, u32)> {
        if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
            let read = |offset: usize| {
                u32::from_be_bytes([
                    bytes[offset],
                    bytes[offset + 1],
                    bytes[offset + 2],
                    bytes[offset + 3],
                ])
            };
            return Some((read(16), read(20)));
        }
        if bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
            let mut i = 2usize;
            while i + 9 < bytes.len() {
                if bytes[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = bytes[i + 1];
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

    pub fn shutdown(&mut self) {
        self.close_pages();
        if let Some(mut child) = self.process.take() {
            cdp::kill_process_group(&mut child);
        }
        self.port = 0;
        self.browser_info = None;
        self.ws_url = self.config.web_socket_url.clone().unwrap_or_default();
    }
}

impl Drop for BrowserManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn check_exception(result: &Value) -> Result<()> {
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
    Ok(())
}

const FILL_FUNCTION: &str = r#"function(value) {
  const el = this;
  el.focus();
  const tag = el.tagName ? el.tagName.toLowerCase() : '';
  if (tag === 'select') {
    el.value = value;
  } else if (tag === 'input' || tag === 'textarea') {
    if (el.type === 'checkbox' || el.type === 'radio') {
      const want = String(value).toLowerCase() === 'true';
      if (el.checked !== want) { el.click(); }
      return true;
    }
    const proto = Object.getPrototypeOf(el);
    const desc = Object.getOwnPropertyDescriptor(proto, 'value');
    if (desc && desc.set) { desc.set.call(el, value); } else { el.value = value; }
  } else if (el.isContentEditable) {
    el.textContent = value;
  } else {
    throw new TypeError('fill supports inputs, textareas, selects and contenteditable elements');
  }
  el.dispatchEvent(new Event('input', {bubbles: true}));
  el.dispatchEvent(new Event('change', {bubbles: true}));
  return true;
}"#;

const HOVER_FUNCTION: &str = r#"function() {
  const el = this;
  if (typeof el.getBoundingClientRect !== 'function' && el.parentElement) {
    el.parentElement.dispatchEvent(new MouseEvent('mouseover', {bubbles: true}));
    return true;
  }
  const rect = el.getBoundingClientRect();
  const options = {bubbles: true, clientX: rect.left + rect.width / 2,
                   clientY: rect.top + rect.height / 2};
  el.dispatchEvent(new MouseEvent('mouseover', options));
  el.dispatchEvent(new MouseEvent('mouseenter', options));
  el.dispatchEvent(new MouseEvent('mousemove', options));
  return true;
}"#;

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
    Some((
        xs.iter().sum::<f64>() / xs.len() as f64,
        ys.iter().sum::<f64>() / ys.len() as f64,
    ))
}

/// Convert a CDP RemoteObject into JSON without losing unusual return values.
pub fn describe_remote_object(remote: &Value) -> Value {
    if let Some(value) = remote.get("value") {
        if !value.is_null() {
            return value.clone();
        }
    }
    if remote.get("unserializableValue").is_some() {
        return remote["unserializableValue"].clone();
    }
    if remote.get("subtype").and_then(|v| v.as_str()) == Some("null") {
        return Value::Null;
    }
    if let Some(description) = remote
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return json!(description);
    }
    if remote.get("type").and_then(|v| v.as_str()) == Some("undefined") {
        return Value::Null;
    }
    remote.clone()
}

/// Minimal RFC 4648 decoder for CDP's base64 screenshot payload.
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
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let Some(six) = value(byte) else {
            let ch = char::from(byte);
            return Err(TransportError::Protocol(format!(
                "invalid base64 character `{ch}` in screenshot data"
            )));
        };
        buffer = (buffer << 6) | u32::from(six);
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
    fn config_defaults_are_safe_and_desktop_sized() {
        let config = BrowserConfig::from_settings(&Value::Null);
        assert_eq!(config.viewport, (1440, 900));
        assert!(!config.telemetry);
        assert!(!config.obey_robots);
        assert!(config.browser_path.is_none());
        assert!(config.web_socket_url.is_none());
    }

    #[test]
    fn config_reads_lightpanda_settings() {
        let config = BrowserConfig::from_settings(&json!({
            "browser_path": "/opt/panda",
            "web_socket_url": " ws://127.0.0.1:9222/ ",
            "viewport": {"width": 800, "height": 600},
            "protocol_timeout_secs": 0,
            "startup_timeout_secs": 4,
            "screenshot_dir": "/tmp/shots",
            "max_snapshot_nodes": 123,
            "extra_args": ["--block-private-networks"],
            "obey_robots": true,
            "telemetry": true
        }));
        assert_eq!(config.browser_path.as_deref(), Some("/opt/panda"));
        assert_eq!(
            config.web_socket_url.as_deref(),
            Some("ws://127.0.0.1:9222/")
        );
        assert_eq!(config.viewport, (800, 600));
        assert_eq!(config.protocol_timeout_secs, 1);
        assert_eq!(config.startup_timeout_secs, 4);
        assert_eq!(config.max_snapshot_nodes, 123);
        assert!(config.telemetry);
        assert!(config.obey_robots);
    }

    #[test]
    fn invalid_viewport_does_not_replace_defaults() {
        let config =
            BrowserConfig::from_settings(&json!({"viewport": {"width": 0, "height": 700}}));
        assert_eq!(config.viewport, (1440, 900));
    }

    #[test]
    fn remote_manager_does_not_claim_ownership_of_a_process() {
        let manager = BrowserManager::new(BrowserConfig {
            web_socket_url: Some("ws://127.0.0.1:9/".into()),
            ..Default::default()
        });
        assert!(manager.is_running());
        assert_eq!(manager.port(), 0);
    }

    #[test]
    fn png_dimensions_are_read_from_header() {
        let mut bytes = vec![0; 24];
        bytes[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]);
        bytes[16..20].copy_from_slice(&390u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&844u32.to_be_bytes());
        assert_eq!(BrowserManager::screenshot_size(&bytes), Some((390, 844)));
    }

    #[test]
    fn unknown_image_bytes_have_no_dimensions() {
        assert_eq!(BrowserManager::screenshot_size(b"not a PNG"), None);
    }

    #[test]
    fn box_model_centre_is_computed() {
        let model = json!({"content": [0, 10, 20, 10, 20, 30, 0, 30]});
        assert_eq!(quads_center(&model), Some((10.0, 20.0)));
        assert_eq!(quads_center(&json!({"content": [0]})), None);
    }

    #[test]
    fn remote_objects_are_described_usefully() {
        assert_eq!(
            describe_remote_object(&json!({"value": "hello"})),
            json!("hello")
        );
        assert_eq!(
            describe_remote_object(&json!({"subtype": "null"})),
            Value::Null
        );
        assert_eq!(
            describe_remote_object(&json!({"type": "undefined"})),
            Value::Null
        );
        assert_eq!(
            describe_remote_object(&json!({"description": "Window"})),
            json!("Window")
        );
        assert_eq!(
            describe_remote_object(&json!({"unserializableValue": "NaN"})),
            json!("NaN")
        );
    }

    #[test]
    fn base64_decodes_known_vectors() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        assert!(base64_decode("not*base64").is_err());
    }

    #[test]
    fn evaluation_exceptions_are_not_successes() {
        let error = json!({"exceptionDetails": {"text": "ReferenceError"}});
        assert!(check_exception(&error)
            .unwrap_err()
            .to_string()
            .contains("ReferenceError"));
        assert!(check_exception(&json!({"result": {"value": 1}})).is_ok());
    }
}
