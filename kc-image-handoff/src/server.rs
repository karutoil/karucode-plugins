//! A karucode API client, plain HTTP to the loopback server that spawned this
//! plugin - the same shape kc-goals uses.
//!
//! The host clears a plugin's environment, so the server address and a token
//! come from settings (the operator's `[[plugins]]` entry). The token is the
//! one credential this plugin needs: every model call happens through
//! karucode's own runtime, so no provider API keys are involved at all.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-read timeout for JSON requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle read timeout on the SSE stream: a vision run can sit quiet between
/// provider deltas, but not for minutes.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum HttpError {
    BadUrl(String),
    Io(String),
    Status(u16, String),
    Malformed(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::BadUrl(m) => write!(f, "{m}"),
            HttpError::Io(m) => write!(f, "{m}"),
            HttpError::Status(code, body) if body.is_empty() => write!(f, "HTTP {code}"),
            HttpError::Status(code, body) => write!(f, "HTTP {code}: {body}"),
            HttpError::Malformed(m) => write!(f, "{m}"),
        }
    }
}

/// A parsed `http://host[:port]` base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn parse(base: &str) -> Result<Self, HttpError> {
        let rest = base.strip_prefix("http://").ok_or_else(|| {
            HttpError::BadUrl(format!(
                "`{base}` is not an http:// URL. The image-handoff plugin speaks plain HTTP to \
                 the local karucode server; an https:// base would need TLS, which this build \
                 does not link."
            ))
        })?;
        let authority = rest.split('/').next().unwrap_or(rest);
        if authority.is_empty() {
            return Err(HttpError::BadUrl(format!("`{base}` has no host")));
        }
        match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port = port.parse::<u16>().map_err(|_| {
                    HttpError::BadUrl(format!("`{port}` is not a port number in `{base}`"))
                })?;
                Ok(Endpoint {
                    host: host.to_string(),
                    port,
                })
            }
            None => Ok(Endpoint {
                host: authority.to_string(),
                port: 80,
            }),
        }
    }

    fn connect(&self) -> Result<TcpStream, HttpError> {
        let addr = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| HttpError::Io(format!("cannot resolve {}: {e}", self.host)))?
            .next()
            .ok_or_else(|| HttpError::Io(format!("{} resolved to no address", self.host)))?;
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| HttpError::Io(format!("cannot connect to {addr}: {e}")))?;
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }

    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Send one JSON request and read one JSON response body.
pub fn request_json(
    endpoint: &Endpoint,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), HttpError> {
    let payload = body
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| HttpError::Io(format!("cannot serialise the request body: {e}")))?;
    let (status, text) = send(
        endpoint,
        token,
        method,
        path,
        payload.as_deref(),
        "application/json",
    )?;
    let value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).map_err(|e| {
            HttpError::Malformed(format!(
                "the server's answer is not JSON ({e}): {}",
                truncate(&text, 200)
            ))
        })?
    };
    Ok((status, value))
}

/// Send one raw-bytes request (the image upload) and read the JSON response.
pub fn request_raw(
    endpoint: &Endpoint,
    token: &str,
    method: &str,
    path: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<(u16, Value), HttpError> {
    let (status, text) = send_raw(endpoint, token, method, path, content_type, bytes)?;
    let value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text)
            .map_err(|e| HttpError::Malformed(format!("the server's answer is not JSON ({e})")))?
    };
    Ok((status, value))
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Send one request and hand back status plus the body text. Non-2xx bodies
/// are returned with their status; the caller decides what an error is.
fn send(
    endpoint: &Endpoint,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    content_type: &str,
) -> Result<(u16, String), HttpError> {
    let bytes = body.map(str::as_bytes);
    send_bytes(endpoint, token, method, path, Some(content_type), bytes)
}

fn send_raw(
    endpoint: &Endpoint,
    token: &str,
    method: &str,
    path: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<(u16, String), HttpError> {
    send_bytes(
        endpoint,
        token,
        method,
        path,
        Some(content_type),
        Some(bytes),
    )
}

fn send_bytes(
    endpoint: &Endpoint,
    token: &str,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    bytes: Option<&[u8]>,
) -> Result<(u16, String), HttpError> {
    let stream = endpoint.connect()?;
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .map_err(|e| HttpError::Io(e.to_string()))?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\n\
         Connection: close\r\nX-Karucode-Client: web\r\n",
        host = endpoint.authority(),
    );
    if !token.is_empty() {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(content_type) = content_type {
        request.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if let Some(bytes) = bytes {
        request.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
    }
    request.push_str("\r\n");

    let mut writer = stream
        .try_clone()
        .map_err(|e| HttpError::Io(e.to_string()))?;
    writer
        .write_all(request.as_bytes())
        .map_err(|e| HttpError::Io(format!("cannot send the request: {e}")))?;
    if let Some(bytes) = bytes {
        writer
            .write_all(bytes)
            .map_err(|e| HttpError::Io(format!("cannot send the request body: {e}")))?;
    }
    writer
        .flush()
        .map_err(|e| HttpError::Io(format!("cannot flush the request: {e}")))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| HttpError::Io(format!("no response: {e}")))?;
    let status = parse_status(&status_line)?;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| HttpError::Io(format!("truncated headers: {e}")))?;
        if read == 0 {
            return Err(HttpError::Malformed(
                "the response ended inside its headers".into(),
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.to_ascii_lowercase();
            let value = value.trim();
            if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            } else if name == "content-length" {
                content_length = value.parse().ok();
            }
        }
    }
    let text = if chunked {
        read_chunked(&mut reader)?
    } else if content_length.map(|len| len > 0).unwrap_or(false) {
        let mut buf = vec![0u8; content_length.unwrap()];
        reader
            .read_exact(&mut buf)
            .map_err(|e| HttpError::Io(format!("cannot read the body: {e}")))?;
        String::from_utf8_lossy(&buf).to_string()
    } else {
        // No length, no chunking: read to EOF (Connection: close).
        let mut text = String::new();
        reader
            .read_to_string(&mut text)
            .map_err(|e| HttpError::Io(format!("cannot read the body: {e}")))?;
        text
    };
    Ok((status, text))
}

fn parse_status(line: &str) -> Result<u16, HttpError> {
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(HttpError::Malformed(format!(
            "not an HTTP response: {line:?}"
        )));
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Malformed(format!("no status code in {line:?}")))
}

fn read_chunked(reader: &mut BufReader<TcpStream>) -> Result<String, HttpError> {
    let mut out = String::new();
    loop {
        let mut size_line = String::new();
        let read = reader
            .read_line(&mut size_line)
            .map_err(|e| HttpError::Io(e.to_string()))?;
        if read == 0 {
            break;
        }
        let size_hex = size_line.trim();
        if size_hex.is_empty() {
            continue;
        }
        let size = usize::from_str_radix(size_hex.split(';').next().unwrap_or("0"), 16)
            .map_err(|_| HttpError::Malformed(format!("bad chunk size {size_hex:?}")))?;
        if size == 0 {
            break;
        }
        let mut buf = vec![0u8; size];
        reader
            .read_exact(&mut buf)
            .map_err(|e| HttpError::Io(format!("truncated chunk: {e}")))?;
        out.push_str(&String::from_utf8_lossy(&buf));
        // The CRLF that terminates the chunk.
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf);
    }
    Ok(out)
}

/// One SSE frame from `GET /api/events`.
#[derive(Debug, Clone, PartialEq)]
pub struct SseFrame {
    pub id: u64,
    pub data: String,
}

/// The live event stream of one connection: `data: {json}` per frame.
pub struct EventStream {
    reader: BufReader<TcpStream>,
}

/// Open the server's SSE stream. Token may be empty when the server accepts
/// unauthenticated event streams; the bus retries with no token first.
pub fn open_event_stream(endpoint: &Endpoint, token: &str) -> Result<EventStream, HttpError> {
    EventStream::connect(endpoint, token)
}

impl EventStream {
    /// Read the next frame, or `Ok(None)` when the server closed the stream.
    /// An idle timeout surfaces as [`HttpError::Io`] whose message contains
    /// `idle`, so a caller can distinguish a quiet stream from a dead one.
    pub fn read_frame(&mut self) -> Result<Option<SseFrame>, HttpError> {
        self.next_frame()
    }

    /// Open the stream. Returns before the first frame arrives.
    pub fn connect(endpoint: &Endpoint, token: &str) -> Result<Self, HttpError> {
        let stream = endpoint.connect()?;
        stream
            .set_read_timeout(Some(SSE_IDLE_TIMEOUT))
            .map_err(|e| HttpError::Io(e.to_string()))?;
        let mut request = format!(
            "GET /api/events HTTP/1.1\r\nHost: {host}\r\nAccept: text/event-stream\r\n\
             Connection: close\r\nX-Karucode-Client: web\r\n",
            host = endpoint.authority(),
        );
        if !token.is_empty() {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        request.push_str("\r\n");
        let mut writer = stream
            .try_clone()
            .map_err(|e| HttpError::Io(e.to_string()))?;
        writer
            .write_all(request.as_bytes())
            .map_err(|e| HttpError::Io(format!("cannot request the event stream: {e}")))?;
        writer
            .flush()
            .map_err(|e| HttpError::Io(format!("cannot flush: {e}")))?;
        let mut reader = BufReader::new(stream);
        // Consume the response headers so the first `read_frame` sees data.
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| HttpError::Io(format!("no event stream response: {e}")))?;
        let status = parse_status(&status_line)?;
        if status != 200 {
            return Err(HttpError::Status(
                status,
                "the event stream was refused".into(),
            ));
        }
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .map_err(|e| HttpError::Io(format!("truncated event headers: {e}")))?;
            if read == 0 || line.trim_end_matches(['\r', '\n']).is_empty() {
                break;
            }
        }
        Ok(Self { reader })
    }

    fn next_frame(&mut self) -> Result<Option<SseFrame>, HttpError> {
        loop {
            // Frames are `id: N\ndata: {...}\n\n`; the data line is what we want.
            let mut line = String::new();
            let read = match self.reader.read_line(&mut line) {
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Idle, not dead: the caller decides what an idle stream means.
                    return Err(HttpError::Io(
                        "idle: no frame within the read timeout".into(),
                    ));
                }
                Err(e) => return Err(HttpError::Io(format!("the event stream died: {e}"))),
            };
            if read == 0 {
                return Ok(None);
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = trimmed.strip_prefix("data: ") {
                return Ok(Some(SseFrame {
                    id: 0,
                    data: data.to_string(),
                }));
            }
            if trimmed.is_empty() {
                continue;
            }
            // Keep-alive comments and `event: karucode.transport` frames are
            // skipped: only `data:` frames carry envelopes.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing_covers_ports_and_defaults() {
        let e = Endpoint::parse("http://127.0.0.1:8787").unwrap();
        assert_eq!((e.host.as_str(), e.port), ("127.0.0.1", 8787));
        let e = Endpoint::parse("http://localhost").unwrap();
        assert_eq!(e.port, 80);
        assert!(Endpoint::parse("https://x").is_err());
        assert!(Endpoint::parse("http://").is_err());
        assert!(Endpoint::parse("http://host:notaport").is_err());
    }
}
