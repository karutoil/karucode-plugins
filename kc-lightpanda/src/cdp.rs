//! Blocking Chrome DevTools Protocol transport for Lightpanda.
//!
//! Lightpanda serves one browser-level WebSocket per client. Pages are created
//! with `Target.createBrowserContext` + `Target.createTarget`, then bound to a
//! flattened `sessionId`. All later page commands on that socket carry the id.
//! Responses are matched by command id while unsolicited events are retained
//! for navigation and console readers. Reads and commands are explicitly timed.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/// Longest single CDP response we will buffer. Large text-rendered screenshots
/// are the big ones; beyond this the call fails explicitly rather
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
    pub session_id: Option<String>,
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
                Self::Closed("Lightpanda closed the DevTools socket (page or tab closed?)".into())
            }
            tungstenite::Error::Io(e) => Self::Io(e.to_string()),
            other => Self::WebSocket(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, TransportError>;

/// Resolve a configured host and share one absolute timeout across every
/// candidate address. Callers are expected to use a local/trusted endpoint;
/// operating-system DNS resolution itself does not expose a deadline.
fn connect_with_timeout(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    use std::net::SocketAddr;
    let normalized_host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses: Vec<SocketAddr> = (normalized_host, port)
        .to_socket_addrs()
        .map_err(|e| TransportError::Io(format!("could not resolve {host}: {e}")))?
        .collect();
    if addresses.is_empty() {
        return Err(TransportError::Io(format!(
            "{host}:{port} resolved to no addresses"
        )));
    }
    let deadline = Instant::now() + timeout;
    let mut last_error = "no address could be attempted".to_string();
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => return Ok(stream),
            Err(err) => last_error = err.to_string(),
        }
    }
    Err(TransportError::Io(format!(
        "could not connect to {host}:{port} within {}s: {last_error}",
        timeout.as_secs_f64()
    )))
}

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

/// A CDP error returned by Lightpanda (the `error` field of a response).
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
    /// Flattened CDP page session. `None` addresses browser-level Target methods.
    session_id: Option<String>,
    /// Set when the socket is known dead; every later call fails fast.
    dead: Option<String>,
}

impl PageSocket {
    /// Attach to a plain-WS Lightpanda endpoint with bounded TCP connect and
    /// WebSocket upgrade phases. TLS support is intentionally not compiled in;
    /// use an operator-controlled loopback socket or a trusted TLS proxy.
    pub fn connect(ws_url: &str, timeout: Duration) -> Result<Self> {
        let uri: tungstenite::http::Uri = ws_url.parse().map_err(|err| {
            TransportError::Protocol(format!("invalid WebSocket URL `{ws_url}`: {err}"))
        })?;
        if uri.scheme_str() != Some("ws") {
            return Err(TransportError::Protocol(format!(
                "web_socket_url `{ws_url}` must use ws://; TLS/wss is not compiled in"
            )));
        }
        let host = uri
            .host()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| TransportError::Protocol("WebSocket URL has no host".into()))?;
        let port = uri.port_u16().unwrap_or(80);
        let started = Instant::now();
        let tcp = connect_with_timeout(host, port, timeout)?;
        let remaining = timeout
            .saturating_sub(started.elapsed())
            .max(Duration::from_millis(1));
        tcp.set_read_timeout(Some(remaining))
            .map_err(|e| TransportError::Io(format!("could not bound the handshake read: {e}")))?;
        tcp.set_write_timeout(Some(remaining))
            .map_err(|e| TransportError::Io(format!("could not bound the handshake write: {e}")))?;
        let transport = tungstenite::stream::MaybeTlsStream::Plain(tcp);
        let (ws, _response) = tungstenite::client::client(ws_url, transport).map_err(|err| {
            TransportError::WebSocket(format!(
                "could not attach to {ws_url} within {}s: {err}",
                timeout.as_secs_f64()
            ))
        })?;
        let tungstenite::stream::MaybeTlsStream::Plain(tcp) = ws.get_ref() else {
            return Err(TransportError::Protocol(
                "plain ws:// connection unexpectedly used a TLS transport".into(),
            ));
        };
        let _ = tcp.set_nodelay(true);
        tcp.set_read_timeout(Some(READ_POLL_INTERVAL))
            .map_err(|e| TransportError::Io(format!("could not bound CDP reads: {e}")))?;
        tcp.set_write_timeout(Some(timeout))
            .map_err(|e| TransportError::Io(format!("could not bound CDP writes: {e}")))?;
        Ok(Self {
            ws,
            next_id: 1,
            events: VecDeque::new(),
            max_events: 20_000,
            timeout,
            session_id: None,
            dead: None,
        })
    }

    pub fn is_dead(&self) -> bool {
        self.dead.is_some()
    }

    pub fn dead_reason(&self) -> Option<&str> {
        self.dead.as_deref()
    }

    /// Route subsequent commands to an attached flattened page session.
    pub fn use_session(&mut self, session_id: String) {
        self.session_id = Some(session_id);
    }

    /// Send a browser-level command even after a session has been selected.
    /// The previous binding is restored on both success and failure.
    pub fn send_root(&mut self, method: &str, params: Value) -> Result<Value> {
        let saved = self.session_id.take();
        let result = self.send(method, params);
        self.session_id = saved;
        result
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
        let deadline = Instant::now() + timeout;
        let id = self.next_id;
        self.next_id += 1;
        if Instant::now() >= deadline {
            return Err(TransportError::Timeout(format!(
                "`{method}` exhausted its timeout before the write"
            )));
        }
        self.write_command(id, method, &params, deadline)?;

        loop {
            if Instant::now() > deadline {
                return Err(TransportError::Timeout(format!(
                    "`{method}` did not answer within {}s. Lightpanda may be busy or the \
                     operation needs a longer timeout.",
                    timeout.as_secs()
                )));
            }
            let Some(value) = self.read_frame()? else {
                continue;
            };
            if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if cfg!(debug_assertions)
                    && std::env::var_os("KC_LIGHTPANDA_DEBUG_CDP").is_some()
                    && (method == "Runtime.evaluate"
                        || method == "Page.navigate"
                        || method.starts_with("Target."))
                {
                    eprintln!("CDP response method={method} id={id} frame={value}");
                }
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
                        "Lightpanda detached the page ({reason})"
                    )));
                }
                self.push_event(CapturedEvent {
                    method: event_method.to_string(),
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                    session_id: value
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    at: Instant::now(),
                });
            }
        }
    }

    fn write_command(
        &mut self,
        id: u64,
        method: &str,
        params: &Value,
        deadline: Instant,
    ) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let tungstenite::stream::MaybeTlsStream::Plain(tcp) = self.ws.get_ref() else {
            return Err(TransportError::Protocol(
                "unexpected TLS transport for a plain ws:// page".into(),
            ));
        };
        tcp.set_write_timeout(Some(remaining.max(Duration::from_millis(1))))
            .map_err(|e| {
                TransportError::Io(format!("could not update the command-write timeout: {e}"))
            })?;
        let mut frame = json!({"id": id, "method": method, "params": params});
        if let Some(session_id) = &self.session_id {
            frame["sessionId"] = json!(session_id);
        }
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
                        "Lightpanda sent a {} byte frame, over the {} byte cap",
                        text.len(),
                        MAX_FRAME_BYTES
                    )));
                }
                let value: Value = serde_json::from_str(&text).map_err(|e| {
                    TransportError::Protocol(format!("Lightpanda sent a non-JSON frame: {e}"))
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
                                session_id: value
                                    .get("sessionId")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string),
                                at: Instant::now(),
                            });
                            count += 1;
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    self.dead = Some("Lightpanda closed the DevTools socket".into());
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
        if cfg!(debug_assertions) && std::env::var_os("KC_LIGHTPANDA_DEBUG_CDP").is_some() {
            eprintln!(
                "CDP event session={} method={} params={}",
                event.session_id.as_deref().unwrap_or("browser"),
                event.method,
                event.params
            );
        }
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

/// Minimal HTTP/1.1 GET client for Lightpanda's local DevTools endpoints.
///
/// Lightpanda's `/json/*` endpoints are plain HTTP on localhost and we already have
/// `std::net`, so pulling in an HTTP stack for two GETs is not worth it. This
/// speaks exactly enough HTTP/1.1: GET, headers, `Content-Length` and
/// chunked bodies, no redirects, no TLS.
///
/// Reading stops as soon as the declared body is complete: Lightpanda ignores
/// `Connection: close` and keeps the socket open, so `read_to_end` would block
/// until the read timeout and discard a perfectly good response.
pub fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<String> {
    let addr = format!("{host}:{port}");
    let deadline = Instant::now() + timeout;
    let mut stream = connect_with_timeout(host, port, timeout)?;
    stream
        .set_read_timeout(Some(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
        ))
        .map_err(|e| TransportError::Io(format!("could not bound the HTTP read: {e}")))?;
    stream
        .set_write_timeout(Some(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
        ))
        .map_err(|e| TransportError::Io(format!("could not bound the HTTP write: {e}")))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| TransportError::Io(format!("could not write the request: {e}")))?;

    // Accumulate until the response is complete, EOF, or the socket goes quiet.
    let mut raw: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::Timeout(format!(
                "GET {path} on {addr} exceeded {}s",
                timeout.as_secs_f64()
            )));
        }
        stream
            .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
            .map_err(|e| {
                TransportError::Io(format!("could not update the HTTP read timeout: {e}"))
            })?;
        match stream.read(&mut buffer) {
            Ok(0) => break, // peer closed: response is as complete as it gets
            Ok(n) => {
                if raw.len() + n > 1024 * 1024 {
                    return Err(TransportError::Protocol(
                        "HTTP readiness response exceeded 1 MiB".into(),
                    ));
                }
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
        // Parse actual chunk boundaries. A zero chunk's bytes might occur in a
        // larger chunk's payload and must not make that payload look finished.
        return decode_chunked_prefix(body).map(|option| option.is_some());
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

/// Decode as much as is present: `None` means incomplete input, not corruption.
fn decode_chunked_prefix(body: &[u8]) -> Result<Option<(Vec<u8>, usize)>> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    loop {
        let Some(remaining) = body.get(cursor..) else {
            return Ok(None);
        };
        let Some(relative_end) = find_subslice(remaining, b"\r\n") else {
            return Ok(None);
        };
        let line_end = cursor + relative_end;
        let size_line = String::from_utf8_lossy(&body[cursor..line_end]);
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_| TransportError::Protocol(format!("invalid chunk size `{size_line}`")))?;
        let data_start = line_end.checked_add(2).ok_or_else(|| {
            TransportError::Protocol("chunk size overflowed the body offset".into())
        })?;
        let data_end = data_start.checked_add(size).ok_or_else(|| {
            TransportError::Protocol("chunk size overflowed the body length".into())
        })?;
        let terminator_end = data_end.checked_add(2).ok_or_else(|| {
            TransportError::Protocol("chunk terminator overflowed the body length".into())
        })?;
        if terminator_end > body.len() {
            return Ok(None);
        }
        out.extend_from_slice(&body[data_start..data_end]);
        if &body[data_end..terminator_end] != b"\r\n" {
            return Err(TransportError::Protocol(
                "chunk data is not followed by CRLF".into(),
            ));
        }
        cursor = terminator_end;
        if size == 0 {
            return Ok(Some((out, cursor)));
        }
    }
}

fn decode_chunked(body: &[u8]) -> Result<Vec<u8>> {
    match decode_chunked_prefix(body)? {
        Some((decoded, _consumed)) => Ok(decoded),
        None => Err(TransportError::Protocol("truncated chunked body".into())),
    }
}

/// Version payload and WebSocket endpoint returned by Lightpanda.
#[derive(Debug, Clone, Default)]
pub struct BrowserInfo {
    pub browser: String,
    pub web_socket_debugger_url: String,
    pub lightpanda_version: String,
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
        lightpanda_version: value
            .get("Lightpanda-Version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Locate Lightpanda without searching for or falling back to Chromium.
pub fn find_lightpanda_binary(explicit: Option<&str>) -> Option<String> {
    if let Some(path) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(path.to_string());
    }
    let mut candidates = vec![
        "/usr/local/bin/lightpanda".to_string(),
        "/usr/bin/lightpanda".to_string(),
        "/opt/homebrew/bin/lightpanda".to_string(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.insert(
            0,
            std::path::PathBuf::from(home)
                .join(".local/bin/lightpanda")
                .to_string_lossy()
                .into_owned(),
        );
    }
    candidates
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .or_else(|| which("lightpanda"))
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

/// Reserve an ephemeral loopback port. The bind/reuse race is acknowledged in
/// README; startup reports an error and the next tool call can retry.
pub fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| TransportError::Io(format!("could not reserve a port: {e}")))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| TransportError::Io(e.to_string()))
}

/// Spawn Lightpanda without ever inheriting the plugin's protocol stdout.
pub fn spawn_lightpanda(
    program: &str,
    args: &[String],
    telemetry: bool,
) -> Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Browser core dumps can contain complete credentials and page data.
        .env("LIGHTPANDA_DISABLE_CORE_DUMP", "1");
    if !telemetry {
        command.env("LIGHTPANDA_DISABLE_TELEMETRY", "true");
    } else {
        command.env_remove("LIGHTPANDA_DISABLE_TELEMETRY");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // A host that predates process-group teardown may kill only the plugin
        // pid. Ask the kernel to terminate the browser when that parent dies too.
        // SAFETY: pre_exec runs in the child and only calls async-signal-safe
        // prctl(2); no Rust allocation or shared runtime state is touched.
        unsafe {
            command.pre_exec(|| {
                const PR_SET_PDEATHSIG: i32 = 1;
                const SIGTERM: i32 = 15;
                extern "C" {
                    fn prctl(option: i32, ...) -> i32;
                }
                if prctl(PR_SET_PDEATHSIG, SIGTERM) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    command
        .spawn()
        .map_err(|e| TransportError::Io(format!("could not start `{program}`: {e}")))
}

/// Stop the browser process group and reap the child. SIGKILL and kill(2)
/// errors are intentionally best-effort during teardown; fatal start/runtime
/// errors are returned by the operation that encountered them.
pub fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // SAFETY: this is the process group created by spawn_lightpanda.
        unsafe { libc_kill(-pid, 15) };
        for _ in 0..40 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // SAFETY: bounded escalation after graceful termination did not work.
        unsafe { libc_kill(-pid, 9) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, signal: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let _ = kill(pid, signal);
}

/// Wait for the local server, and refuse to attach if it is not Lightpanda.
pub fn wait_for_browser(port: u16, timeout: Duration) -> Result<BrowserInfo> {
    let deadline = Instant::now() + timeout;
    let mut last_error = "Lightpanda did not expose its CDP endpoint".to_string();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match http_get("127.0.0.1", port, "/json/version", remaining) {
            Ok(body) => match parse_version(&body) {
                Ok(info) if info.browser.starts_with("Lightpanda/") => return Ok(info),
                Ok(info) => {
                    last_error = format!(
                        "expected Lightpanda, but /json/version identified {:?}",
                        info.browser
                    )
                }
                Err(err) => last_error = err.to_string(),
            },
            Err(err) => last_error = err.to_string(),
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Err(TransportError::Timeout(format!(
        "Lightpanda did not open its CDP endpoint on 127.0.0.1:{port} within {}s: {last_error}",
        timeout.as_secs()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_chunked_body() {
        let input = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(input).unwrap(), b"Wikipedia");
    }

    #[test]
    fn chunked_body_truncation_is_an_error() {
        assert!(decode_chunked(b"10\r\nshort").is_err());
    }

    #[test]
    fn detects_complete_responses_by_content_length() {
        assert!(response_is_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").unwrap());
        assert!(!response_is_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}").unwrap());
    }

    #[test]
    fn parses_lightpanda_version() {
        let info = parse_version(
            r#"{"Browser":"Lightpanda/1.0","Lightpanda-Version":"1.1.0-test","webSocketDebuggerUrl":"ws://127.0.0.1:9222/"}"#,
        )
        .unwrap();
        assert_eq!(info.browser, "Lightpanda/1.0");
        assert_eq!(info.lightpanda_version, "1.1.0-test");
        assert_eq!(info.web_socket_debugger_url, "ws://127.0.0.1:9222/");
    }

    #[test]
    fn invalid_version_is_not_silently_accepted() {
        assert!(parse_version("not JSON").is_err());
    }

    #[test]
    fn no_chrome_binary_fallback_exists() {
        let explicit = find_lightpanda_binary(Some("custom-panda"));
        assert_eq!(explicit.as_deref(), Some("custom-panda"));
    }

    #[test]
    fn free_port_returns_something_bindable() {
        let port = free_port().unwrap();
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn chunk_payload_masquerading_as_its_terminator_is_not_special() {
        let payload = b"A\r\n0\r\n\r\nB";
        let input = b"9\r\nA\r\n0\r\n\r\nB\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(input).unwrap(), payload);
        let response = [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
            input.as_slice(),
        ]
        .concat();
        assert!(response_is_complete(&response).unwrap());
    }

    #[test]
    fn malformed_chunks_never_panic_or_walk_out_of_bounds() {
        assert!(decode_chunked(b"7\r\n\r\n0\r\n\r\n").is_err());
        assert!(decode_chunked(b"1\r\naXX0\r\n\r\n").is_err());
        let valid = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        for end in 0..=valid.len() {
            let truncated = &valid[..end];
            assert!(
                std::panic::catch_unwind(|| decode_chunked(truncated)).is_ok(),
                "truncation at {end} panicked"
            );
        }
    }

    #[test]
    fn every_chunk_prefix_is_safe_to_poll_for_completeness() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nA\r\n0\r\n\r\nB\r\n0\r\n\r\n";
        for end in 0..=raw.len() {
            let partial = &raw[..end];
            assert!(
                std::panic::catch_unwind(|| response_is_complete(partial)).is_ok(),
                "prefix at {end} panicked"
            );
        }
    }

    #[test]
    fn invalid_and_tls_websocket_urls_are_rejected_immediately() {
        for bad in [
            "wss://127.0.0.1:9222/",
            "http://127.0.0.1:9222/",
            "not a url",
        ] {
            let error = match PageSocket::connect(bad, Duration::from_millis(10)) {
                Ok(_) => panic!("invalid URL {bad} unexpectedly connected"),
                Err(error) => error,
            };
            assert!(
                matches!(error, TransportError::Protocol(_)),
                "unexpected error for {bad}: {error}"
            );
        }
    }
}
