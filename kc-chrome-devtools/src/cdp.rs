//! Transport: the target-attached CDP WebSocket and a tiny HTTP client for
//! Chrome's DevTools endpoints.
//!
//! ## Why a flat session
//!
//! Chrome exposes a browser-level WebSocket (`/devtools/browser/<id>`) on which
//! commands must be wrapped in `Target.attachToTarget` sessions. Attaching
//! through the *page* WebSocket instead (`/devtools/page/<id>` from
//! `GET /json/list`) gives the same protocol without session ids, which keeps
//! every `send` a single flat command. Page-level sockets do not emit
//! `Target.targetCrashed`; the transport notices death through socket errors and
//! `Inspector.detached` instead, and re-attaches on the next page tool.
//!
//! ## Request/response framing
//!
//! `send` registers its id, writes the frame, then reads until that id comes
//! back, stashing every other message as an event. That is why the reader also
//! runs a "drain" path: events accumulate while we wait, and tools that care
//! about event history (`list_network_requests`, console messages) consume the
//! stash instead of asking Chrome to replay it.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/// Longest single CDP response we will buffer. Heap snapshots and large
/// screenshots are the big ones; beyond this the call fails explicitly rather
/// than growing without bound.
const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

/// How long a single socket read may block before we loop and re-check the
/// deadline. Short enough that command timeouts and event draining stay
/// responsive, long enough that an idle page costs almost nothing.
const READ_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// An event captured while waiting for a response.
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    pub method: String,
    pub params: Value,
    pub at: Instant,
}

/// Transport error. Everything is flattened into a message for the model.
#[derive(Debug)]
pub enum TransportError {
    Io(String),
    WebSocket(String),
    Closed(String),
    Protocol(String),
    Timeout(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "transport I/O error: {m}"),
            Self::WebSocket(m) => write!(f, "websocket error: {m}"),
            Self::Closed(m) => write!(f, "connection closed: {m}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
            Self::Timeout(m) => write!(f, "timeout: {m}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<tungstenite::Error> for TransportError {
    fn from(err: tungstenite::Error) -> Self {
        match err {
            tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
                Self::Closed("Chrome closed the DevTools socket (page or tab closed?)".into())
            }
            tungstenite::Error::Io(e) => Self::Io(e.to_string()),
            other => Self::WebSocket(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, TransportError>;

/// Turn a response frame into a `result`, or the CDP error it carries.
fn ok_or_cdp_error(value: &Value, method: &str) -> Result<Value> {
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string();
        let data = error
            .get("data")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string());
        return Err(TransportError::Protocol(format!(
            "{} (command `{method}`)",
            CdpError {
                code,
                message,
                data
            }
        )));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// A CDP error returned by Chrome (the `error` field of a response).
#[derive(Debug, Clone)]
pub struct CdpError {
    pub code: i64,
    pub message: String,
    pub data: Option<String>,
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CDP error {}: {}", self.code, self.message)?;
        if let Some(data) = &self.data {
            write!(f, " ({data})")?;
        }
        Ok(())
    }
}

/// One attached page connection.
pub struct PageSocket {
    ws: WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    next_id: u64,
    /// Events seen since the last `take_events`, oldest first.
    events: VecDeque<CapturedEvent>,
    /// Hard cap on the event stash so a chatty page cannot grow us forever.
    max_events: usize,
    /// Default per-command timeout.
    pub timeout: Duration,
    /// Set when the socket is known dead; every later call fails fast.
    dead: Option<String>,
}

impl PageSocket {
    /// Attach to a page WebSocket URL.
    pub fn connect(ws_url: &str, timeout: Duration) -> Result<Self> {
        let (ws, _response) = tungstenite::connect(ws_url).map_err(|err| {
            TransportError::WebSocket(format!("could not attach to {ws_url}: {err}"))
        })?;
        // A read timeout is what makes the `WouldBlock` handling in `send_raw`
        // and `pump_events` real: without it `ws.read()` blocks forever on a
        // quiet page, so a command timeout could never fire and event draining
        // would hang instead of returning. It also bounds how long a dead peer
        // can stall us.
        if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = ws.get_ref() {
            let _ = tcp.set_nodelay(true);
            let _ = tcp.set_read_timeout(Some(READ_POLL_INTERVAL));
        }
        Ok(Self {
            ws,
            next_id: 1,
            events: VecDeque::new(),
            max_events: 20_000,
            timeout,
            dead: None,
        })
    }

    pub fn is_dead(&self) -> bool {
        self.dead.is_some()
    }

    pub fn dead_reason(&self) -> Option<&str> {
        self.dead.as_deref()
    }

    /// Send a command and wait for its response. Returns the `result` object.
    pub fn send(&mut self, method: &str, params: Value) -> Result<Value> {
        self.send_with_timeout(method, params, self.timeout)
    }

    /// Send a command with an explicit timeout (long navigations, tracing).
    pub fn send_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        if let Some(reason) = &self.dead {
            return Err(TransportError::Closed(reason.clone()));
        }
        self.send_raw(method, params, timeout).map_err(|err| {
            // Any transport-level failure leaves the socket unusable; remember
            // it so the next call does not block on a dead pipe.
            if matches!(
                err,
                TransportError::Closed(_) | TransportError::WebSocket(_) | TransportError::Io(_)
            ) {
                self.dead = Some(err.to_string());
            }
            err
        })
    }

    fn send_raw(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_command(id, method, &params)?;

        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() > deadline {
                return Err(TransportError::Timeout(format!(
                    "`{method}` did not answer within {}s. Chrome may be busy or the \
                     operation needs a longer timeout.",
                    timeout.as_secs()
                )));
            }
            let Some(value) = self.read_frame()? else {
                continue;
            };
            if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return ok_or_cdp_error(&value, method);
            }
            // Not our response: an event (or a stale reply we skip).
            if let Some(event_method) = value.get("method").and_then(|m| m.as_str()) {
                if event_method == "Inspector.detached" {
                    let reason = value
                        .get("params")
                        .and_then(|p| p.get("reason"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("detached")
                        .to_string();
                    return Err(TransportError::Closed(format!(
                        "Chrome detached the page ({reason})"
                    )));
                }
                self.push_event(CapturedEvent {
                    method: event_method.to_string(),
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                    at: Instant::now(),
                });
            }
        }
    }

    /// Write a command frame and return its id without waiting for the answer.
    ///
    /// Used for commands whose output arrives as events before the response
    /// (`HeapProfiler.takeHeapSnapshot` streams its snapshot in chunks).
    pub fn start_command(&mut self, method: &str, params: Value) -> Result<u64> {
        if let Some(reason) = &self.dead {
            return Err(TransportError::Closed(reason.clone()));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.write_command(id, method, &params)?;
        Ok(id)
    }

    fn write_command(&mut self, id: u64, method: &str, params: &Value) -> Result<()> {
        let frame = json!({"id": id, "method": method, "params": params});
        let text = serde_json::to_string(&frame)
            .map_err(|e| TransportError::Protocol(format!("could not encode `{method}`: {e}")))?;
        self.ws.send(Message::Text(text.into()))?;
        Ok(())
    }

    /// Read one JSON frame, skipping binary/pong frames.
    ///
    /// `Ok(None)` means "no JSON frame arrived yet" (a control frame, or the
    /// read timeout fired); callers loop and re-check their own deadline.
    pub fn read_frame(&mut self) -> Result<Option<Value>> {
        let message = match self.ws.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        match message {
            Message::Text(text) => {
                if text.len() > MAX_FRAME_BYTES {
                    return Err(TransportError::Protocol(format!(
                        "Chrome sent a {} byte frame, over the {} byte cap",
                        text.len(),
                        MAX_FRAME_BYTES
                    )));
                }
                let value: Value = serde_json::from_str(&text).map_err(|e| {
                    TransportError::Protocol(format!(
                        "Chrome sent a non-JSON frame: {e}"
                    ))
                })?;
                Ok(Some(value))
            }
            Message::Close(frame) => {
                let reason = frame
                    .map(|f| f.reason.to_string())
                    .unwrap_or_else(|| "no reason given".into());
                Err(TransportError::Closed(reason))
            }
            Message::Ping(payload) => {
                self.ws.send(Message::Pong(payload))?;
                Ok(None)
            }
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
        }
    }

    /// Drain any queued events without issuing a command.
    pub fn pump_events(&mut self, budget: Duration) -> usize {
        if self.dead.is_some() {
            return 0;
        }
        let deadline = Instant::now() + budget;
        let mut count = 0;
        while Instant::now() < deadline {
            match self.ws.read() {
                Ok(Message::Text(text)) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
                            self.push_event(CapturedEvent {
                                method: method.to_string(),
                                params: value.get("params").cloned().unwrap_or(Value::Null),
                                at: Instant::now(),
                            });
                            count += 1;
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    self.dead = Some("Chrome closed the DevTools socket".into());
                    break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    // Nothing to read yet: the read timeout fired. Not an error.
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => {
                    self.dead = Some("DevTools socket error while draining events".into());
                    break;
                }
            }
        }
        count
    }

    fn push_event(&mut self, event: CapturedEvent) {
        if self.events.len() >= self.max_events {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Take every stashed event, oldest first.
    pub fn take_events(&mut self) -> Vec<CapturedEvent> {
        self.events.drain(..).collect()
    }

    /// Look at the stash without consuming it.
    pub fn events(&self) -> impl Iterator<Item = &CapturedEvent> {
        self.events.iter()
    }

    /// Drop stashed events matching a predicate (rows already accounted for).
    pub fn retain_events(&mut self, mut keep: impl FnMut(&CapturedEvent) -> bool) {
        self.events.retain(|e| keep(e));
    }

    /// Close the socket politely. Errors are ignored: callers close on the way
    /// out and nothing good comes from surfacing a close failure.
    pub fn close(mut self) {
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }
}

/// Minimal HTTP/1.1 GET client for Chrome's local DevTools endpoints.
///
/// Chrome's `/json/*` endpoints are plain HTTP on localhost and we already have
/// `std::net`, so pulling in an HTTP stack for two GETs is not worth it. This
/// speaks exactly enough HTTP/1.1: GET/PUT, headers, `Content-Length` and
/// chunked bodies, no redirects, no TLS.
///
/// Reading stops as soon as the declared body is complete: Chrome ignores
/// `Connection: close` and keeps the socket open, so `read_to_end` would block
/// until the read timeout and discard a perfectly good response.
pub fn http_get(host: &str, port: u16, path: &str) -> Result<String> {
    http_request("GET", host, port, path)
}

/// `/json/new` needs `PUT` on current Chrome versions.
pub fn http_put(host: &str, port: u16, path: &str) -> Result<String> {
    http_request("PUT", host, port, path)
}

fn http_request(method: &str, host: &str, port: u16, path: &str) -> Result<String> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| TransportError::Io(format!("could not connect to {addr}: {e}")))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| TransportError::Io(e.to_string()))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| TransportError::Io(e.to_string()))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| TransportError::Io(format!("could not write the request: {e}")))?;

    // Accumulate until the response is complete, EOF, or the socket goes quiet.
    let mut raw: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break, // peer closed: response is as complete as it gets
            Ok(n) => {
                raw.extend_from_slice(&buffer[..n]);
                if response_is_complete(&raw)? {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Quiet socket. If the headers and body are already complete we
                // are done; otherwise the request genuinely stalled.
                if response_is_complete(&raw)? {
                    break;
                }
                return Err(TransportError::Io(format!(
                    "timed out waiting for a response from {addr}{path}"
                )));
            }
            Err(e) => {
                return Err(TransportError::Io(format!(
                    "could not read the response: {e}"
                )))
            }
        }
    }

    let split = find_subslice(&raw, b"\r\n\r\n")
        .ok_or_else(|| TransportError::Protocol("HTTP response has no header terminator".into()))?;
    let (head, body) = raw.split_at(split + 4);
    let head = String::from_utf8_lossy(head).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    if !status_line.contains(" 200") {
        return Err(TransportError::Protocol(format!(
            "GET {path} on {addr} failed: {}",
            status_line.trim()
        )));
    }
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
        if lower.starts_with("content-length:") {
            content_length = line
                .split_once(':')
                .and_then(|(_, v)| v.trim().parse::<usize>().ok());
        }
    }

    let body: Vec<u8> = if chunked {
        decode_chunked(body)?
    } else if let Some(len) = content_length {
        body.iter().take(len).copied().collect()
    } else {
        body.to_vec()
    };
    String::from_utf8(body)
        .map_err(|e| TransportError::Protocol(format!("HTTP body was not UTF-8: {e}")))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// True when `raw` holds a complete HTTP response (headers plus full body).
fn response_is_complete(raw: &[u8]) -> Result<bool> {
    let Some(split) = find_subslice(raw, b"\r\n\r\n") else {
        return Ok(false);
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let body = &raw[split + 4..];
    if head.contains("transfer-encoding:") && head.contains("chunked") {
        // A terminating zero-length chunk ends the body.
        return Ok(find_subslice(body, b"\r\n0\r\n\r\n").is_some() || body.starts_with(b"0\r\n\r\n"));
    }
    for line in head.lines() {
        if let Some(value) = line.strip_prefix("content-length:") {
            if let Ok(len) = value.trim().parse::<usize>() {
                return Ok(body.len() >= len);
            }
        }
    }
    // No length and no chunking: only EOF can tell us the body ended.
    Ok(false)
}

fn decode_chunked(body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    loop {
        let line_end = body[cursor..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .map(|p| cursor + p)
            .ok_or_else(|| {
                TransportError::Protocol("truncated chunked body (no size line)".into())
            })?;
        let size_line = String::from_utf8_lossy(&body[cursor..line_end]).to_string();
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_| {
                TransportError::Protocol(format!("invalid chunk size `{size_line}`"))
            })?;
        cursor = line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        if cursor + size > body.len() {
            return Err(TransportError::Protocol(
                "truncated chunked body (chunk overruns the buffer)".into(),
            ));
        }
        out.extend_from_slice(&body[cursor..cursor + size]);
        cursor += size + 2; // skip the trailing CRLF
    }
}

/// A target as listed by `GET /json/list`.
#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub id: String,
    pub title: String,
    pub url: String,
    pub web_socket_debugger_url: String,
    pub target_type: String,
}

/// Parse `GET /json/list` into targets we can attach to.
pub fn parse_target_list(body: &str) -> Result<Vec<TargetInfo>> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| TransportError::Protocol(format!("/json/list was not JSON: {e}")))?;
    let array = value.as_array().ok_or_else(|| {
        TransportError::Protocol("/json/list did not return a JSON array".into())
    })?;
    Ok(array
        .iter()
        .filter_map(|entry| {
            Some(TargetInfo {
                id: entry.get("id")?.as_str()?.to_string(),
                title: entry
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: entry
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                web_socket_debugger_url: entry
                    .get("webSocketDebuggerUrl")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                target_type: entry
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("page")
                    .to_string(),
            })
        })
        .collect())
}

/// Parse `GET /json/version`.
#[derive(Debug, Clone, Default)]
pub struct BrowserInfo {
    pub browser: String,
    pub web_socket_debugger_url: String,
    pub user_agent: String,
}

pub fn parse_version(body: &str) -> Result<BrowserInfo> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| TransportError::Protocol(format!("/json/version was not JSON: {e}")))?;
    Ok(BrowserInfo {
        browser: value
            .get("Browser")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        web_socket_debugger_url: value
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        user_agent: value
            .get("User-Agent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Find a usable Chrome/Chromium binary.
pub fn find_chrome_binary(explicit: Option<&str>) -> Option<String> {
    if let Some(path) = explicit {
        if !path.trim().is_empty() {
            return Some(path.to_string());
        }
    }
    let candidates = [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/opt/google/chrome/chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
        "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    ];
    for candidate in candidates {
        if std::path::Path::new(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }
    // Last resort: whatever PATH offers.
    for name in ["google-chrome", "chromium", "chromium-browser", "chrome"] {
        if let Some(path) = which(name) {
            return Some(path);
        }
    }
    None
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

/// Allocate a free TCP port by binding and dropping the listener.
///
/// There is an unavoidable race between the drop and Chrome's bind; it is small
/// and retried by the caller through [`spawn_retry`].
pub fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| TransportError::Io(format!("could not reserve a port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| TransportError::Io(e.to_string()))?
        .port();
    drop(listener);
    Ok(port)
}

/// Spawn a command without inheriting our stdin/stdout: a plugin's stdout is
/// the karucode protocol, so a child must never inherit it.
pub fn spawn_detached(
    program: &str,
    args: &[String],
) -> Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Put Chrome in its own process group so shutdown can signal the whole
    // tree. Killing only the main process leaves zygotes and GPU helpers alive
    // for a moment, and they keep writing into the profile directory.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|e| TransportError::Io(format!("could not start `{program}`: {e}")))?;
    Ok(child)
}

/// Terminate a browser process and every child in its process group.
///
/// Chromium re-execs itself, so the process group leader is not necessarily the
/// pid we spawned: `child.id()` can already be gone while the browser (and its
/// zygotes/GPU helpers) live on in a group led by a descendant. Signalling
/// `-child_pid` therefore misses them. We resolve the group id from the living
/// descendants instead, and fall back to walking the tree.
pub fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let spawned = child.id() as i32;
        // Collect every pid still referencing this browser, by profile dir or by
        // ancestry, then signal each group they belong to.
        let groups = process_groups_for(spawned);
        for pgid in &groups {
            // SAFETY: negative pid signals the process group; ESRCH is harmless.
            unsafe {
                libc_kill(-*pgid, 15);
            }
        }
        // Give them a moment to exit before forcing the issue.
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) && groups.is_empty() {
                break;
            }
            if !any_alive(spawned) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        for pgid in &groups {
            // SAFETY: as above; SIGKILL whatever is still standing.
            unsafe {
                libc_kill(-*pgid, 9);
            }
        }
        // The spawned pid itself may lead a group too; cover it either way.
        // SAFETY: stale pids yield ESRCH and are ignored.
        unsafe {
            libc_kill(-spawned, 9);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Process groups that contain this browser or its descendants.
///
/// Reads `/proc` directly: a plugin must not depend on `libc`'s procfs helpers,
/// and this is the only portable-enough way to see a re-exec'd tree.
#[cfg(target_os = "linux")]
fn process_groups_for(spawned: i32) -> Vec<i32> {
    let (tree, groups) = scan_process_tree(spawned);
    let _ = tree;
    groups
}

/// Scan `/proc` once and return `(pid -> (ppid, pgid), groups of the subtree)`.
#[cfg(target_os = "linux")]
fn scan_process_tree(spawned: i32) -> (HashMap<i32, (i32, i32)>, Vec<i32>) {
    use std::collections::HashSet;

    // pid -> (ppid, pgid)
    let mut tree: HashMap<i32, (i32, i32)> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return (tree, Vec::new());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `stat` is `pid (comm) state ppid pgrp ...`; comm may itself contain
        // spaces or parentheses, so split on the *last* ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let ppid = fields.next().and_then(|v| v.parse::<i32>().ok());
        let pgid = fields.next().and_then(|v| v.parse::<i32>().ok());
        if let (Some(ppid), Some(pgid)) = (ppid, pgid) {
            tree.insert(pid, (ppid, pgid));
        }
    }

    // Every pid descended from `spawned`.
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for (pid, (ppid, _)) in &tree {
        children.entry(*ppid).or_default().push(*pid);
    }
    let mut groups: HashSet<i32> = HashSet::new();
    let mut seen: HashSet<i32> = HashSet::new();
    let mut stack = vec![spawned];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some((_, pgid)) = tree.get(&pid) {
            groups.insert(*pgid);
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    if let Some((_, pgid)) = tree.get(&spawned) {
        groups.insert(*pgid);
    }
    let groups: Vec<i32> = groups.into_iter().filter(|g| *g > 0).collect();
    (tree, groups)
}

#[cfg(not(target_os = "linux"))]
fn process_groups_for(spawned: i32) -> Vec<i32> {
    vec![spawned]
}

/// True when any process descended from `spawned` is still running.
#[cfg(target_os = "linux")]
fn any_alive(spawned: i32) -> bool {
    let (tree, _) = scan_process_tree(spawned);
    // A failed scan yields an empty tree; assume alive so the caller still
    // force-kills rather than leaving a browser behind.
    if tree.is_empty() {
        return true;
    }
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for (pid, (ppid, _)) in &tree {
        children.entry(*ppid).or_default().push(*pid);
    }
    let mut stack = vec![spawned];
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn any_alive(_spawned: i32) -> bool {
    false
}

/// `kill(2)` without pulling in the `libc` crate.
///
/// # Safety
/// `pid` must be a valid process or process-group id owned by this user.
#[cfg(unix)]
unsafe fn libc_kill(pid: i32, signal: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let _ = kill(pid, signal);
}

/// Wait for `GET /json/version` to answer, up to `timeout`.
pub fn wait_for_browser(port: u16, timeout: Duration) -> Result<BrowserInfo> {
    let deadline = Instant::now() + timeout;
    let mut last_error = String::from("Chrome did not expose its DevTools endpoint");
    while Instant::now() < deadline {
        match http_get("127.0.0.1", port, "/json/version") {
            Ok(body) => match parse_version(&body) {
                Ok(info) => return Ok(info),
                Err(err) => last_error = err.to_string(),
            },
            Err(err) => last_error = err.to_string(),
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    Err(TransportError::Timeout(format!(
        "Chrome did not open its DevTools endpoint on 127.0.0.1:{port} within {}s: {last_error}",
        timeout.as_secs()
    )))
}

/// Convenience: a `HashMap<String, Value>` view of a JSON object, for building
/// command params without `json!` boilerplate at every call site.
pub fn object(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        if !value.is_null() {
            map.insert(key.to_string(), value);
        }
    }
    Value::Object(map)
}

/// Pull an `i64` from a JSON value, accepting numeric strings too.
pub fn as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_chunked_body() {
        let body = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(body).unwrap(), b"hello world".to_vec());
    }

    #[test]
    fn chunked_body_truncation_is_an_error() {
        let body = b"5\r\nhel";
        let err = decode_chunked(body).expect_err("must fail");
        assert!(format!("{err}").contains("truncated"));
    }

    #[test]
    fn detects_complete_responses_by_content_length() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        let mut partial = head.to_vec();
        assert!(!response_is_complete(&partial).unwrap());
        partial.extend_from_slice(b"he");
        assert!(!response_is_complete(&partial).unwrap());
        partial.extend_from_slice(b"llo");
        assert!(response_is_complete(&partial).unwrap());
        // Extra bytes beyond the declared length are fine.
        partial.extend_from_slice(b"trailing");
        assert!(response_is_complete(&partial).unwrap());
    }

    #[test]
    fn detects_complete_chunked_responses() {
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        assert!(!response_is_complete(&raw).unwrap());
        raw.extend_from_slice(b"5\r\nhello\r\n");
        assert!(!response_is_complete(&raw).unwrap());
        raw.extend_from_slice(b"0\r\n\r\n");
        assert!(response_is_complete(&raw).unwrap());
    }

    #[test]
    fn a_response_without_a_length_is_only_complete_at_eof() {
        let raw = b"HTTP/1.1 200 OK\r\nServer: x\r\n\r\nbody";
        assert!(!response_is_complete(raw).unwrap());
        assert!(!response_is_complete(b"HTTP/1.1 200 OK\r\n").unwrap());
    }

    #[test]
    fn parses_a_target_list() {
        let body = r#"[
          {"id":"A1","title":"Example","url":"https://example.com",
           "type":"page","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/A1"},
          {"id":"B2","title":"Extension","url":"chrome-extension://x","type":"background_page"}
        ]"#;
        let targets = parse_target_list(body).unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, "A1");
        assert!(targets[0].web_socket_debugger_url.ends_with("/devtools/page/A1"));
        assert_eq!(targets[1].target_type, "background_page");
        // A target with no socket URL is kept but not attachable; the caller
        // filters on the empty string.
        assert!(targets[1].web_socket_debugger_url.is_empty());
    }

    #[test]
    fn malformed_target_list_is_a_protocol_error() {
        let err = parse_target_list("not json").expect_err("must fail");
        assert!(format!("{err}").contains("not JSON"));
        let err = parse_target_list("{}").expect_err("must fail");
        assert!(format!("{err}").contains("JSON array"));
    }

    #[test]
    fn parses_a_version_object() {
        let body = r#"{"Browser":"Chrome/156.0","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/browser/x","User-Agent":"Mozilla/5.0"}"#;
        let info = parse_version(body).unwrap();
        assert_eq!(info.browser, "Chrome/156.0");
        assert!(info.web_socket_debugger_url.contains("/devtools/browser/"));
    }

    #[test]
    fn object_helper_drops_null_fields() {
        let value = object(vec![
            ("url", json!("https://example.com")),
            ("timeout", Value::Null),
        ]);
        assert_eq!(value["url"], json!("https://example.com"));
        assert!(value.get("timeout").is_none());
    }

    #[test]
    fn as_i64_accepts_strings() {
        assert_eq!(as_i64(&json!(42)), Some(42));
        assert_eq!(as_i64(&json!("42")), Some(42));
        assert_eq!(as_i64(&json!(null)), None);
    }

    #[test]
    fn free_port_returns_something_bindable() {
        let port = free_port().unwrap();
        assert!(port > 0);
        let listener = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(listener.is_ok(), "free_port handed out a busy port");
    }

    #[test]
    fn chrome_binary_discovery_prefers_an_explicit_path() {
        assert_eq!(
            find_chrome_binary(Some("/custom/chrome")).as_deref(),
            Some("/custom/chrome")
        );
        // An empty override falls through to autodetection.
        let found = find_chrome_binary(Some("  "));
        if let Some(path) = found {
            assert!(!path.trim().is_empty());
        }
    }
}