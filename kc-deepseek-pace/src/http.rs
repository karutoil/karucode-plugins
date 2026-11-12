//! A minimal HTTP/1.1 client, just enough for two jobs.
//!
//! A plugin cannot add a dependency without shipping it, and the two things this
//! plugin needs from HTTP are small and fixed:
//!
//! 1. `GET /api/events` as a long-lived `text/event-stream`, read incrementally.
//! 2. Plain JSON requests against the same loopback server.
//!
//! So this is written by hand rather than pulled in as `ureq`: no TLS, no
//! redirects, no connection pooling, no proxies. It handles what karucode's
//! server actually sends - `Transfer-Encoding: chunked` on the event stream,
//! `Content-Length` on the JSON routes - and refuses anything else loudly instead
//! of misparsing it.
//!
//! The parsing is split from the socket on purpose: [`SseParser`] is a pure
//! byte-in, event-out state machine, so the framing rules (split frames,
//! keepalive comments, multi-line `data:`) are unit-tested without a server. Only
//! [`EventStream`] touches a `TcpStream`.
//!
//! **Plain HTTP only.** The plugin talks to the server that spawned it, on
//! loopback. An `https://` server URL is a configuration error this build reports
//! rather than silently downgrading.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// How long a connect may take before the watcher gives up on this attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle read timeout for a JSON request. The event stream sets its own, longer
/// one: it is *supposed* to sit silent between model deltas.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest frame we will buffer before giving up, so a server that never sends a
/// blank line cannot grow this process without bound.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub enum HttpError {
    /// The URL is not `http://host[:port]`.
    BadUrl(String),
    Io(String),
    /// The server answered with a non-2xx status.
    Status(u16, String),
    /// The response was well-formed HTTP but not what this client can read.
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

/// A parsed `http://host:port` base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn parse(base: &str) -> Result<Self, HttpError> {
        let rest = base.strip_prefix("http://").ok_or_else(|| {
            HttpError::BadUrl(format!(
                "`{base}` is not an http:// URL. This build speaks plain HTTP to the local \
                 server only; an https:// endpoint would need TLS, which the plugin does not \
                 link."
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

/// Send one request and hand back the status, a reader over the body, and whether
/// that body is chunk-framed.
fn send(
    endpoint: &Endpoint,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> Result<(u16, BufReader<TcpStream>, bool), HttpError> {
    let stream = endpoint.connect()?;
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .map_err(|e| HttpError::Io(e.to_string()))?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json, text/event-stream\r\n\
         Connection: close\r\nX-Karucode-Client: web\r\n",
        host = endpoint.authority(),
    );
    if !token.is_empty() {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(body) = body {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }

    let mut writer = stream
        .try_clone()
        .map_err(|e| HttpError::Io(e.to_string()))?;
    writer
        .write_all(request.as_bytes())
        .map_err(|e| HttpError::Io(format!("cannot send the request: {e}")))?;
    writer
        .flush()
        .map_err(|e| HttpError::Io(format!("cannot flush the request: {e}")))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| HttpError::Io(format!("no response: {e}")))?;
    let status = parse_status(&status_line)?;

    // Headers: we only need to know whether the body is chunked.
    let mut chunked = false;
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
            if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    Ok((status, reader, chunked))
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

/// One JSON request/response.
pub fn request(
    endpoint: &Endpoint,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> Result<String, HttpError> {
    let (status, mut reader, chunked) = send(endpoint, method, path, token, body)?;
    let text = if chunked {
        read_chunked(&mut reader)?
    } else {
        let mut text = String::new();
        reader
            .read_to_string(&mut text)
            .map_err(|e| HttpError::Io(format!("cannot read the body: {e}")))?;
        text
    };
    if !(200..300).contains(&status) {
        return Err(HttpError::Status(status, text.trim().to_string()));
    }
    Ok(text)
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
        let mut crlf = String::new();
        let _ = reader.read_line(&mut crlf);
    }
    Ok(out)
}

/// Server-sent-event framing: bytes in, `data:` payloads out.
///
/// Split from the socket so the rules that actually bite are testable without a
/// network: a frame split across two reads, a keepalive comment between frames, a
/// multi-line `data:` payload, and a frame that never completes.
#[derive(Debug, Default)]
pub struct SseParser {
    pending: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes as they arrive off the wire.
    ///
    /// CRLF is normalised to LF here rather than at every comparison. The
    /// event-stream spec allows CRLF, LF or CR as a line ending, and a frame
    /// separator of `\r\n\r\n` does not contain `\n\n`, so without this an
    /// otherwise-valid stream would buffer forever and look like a dead socket.
    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.contains(&b'\r') {
            let text = String::from_utf8_lossy(bytes);
            self.pending
                .extend_from_slice(text.replace("\r\n", "\n").as_bytes());
        } else {
            self.pending.extend_from_slice(bytes);
        }
    }

    /// How many bytes are buffered but not yet a complete frame.
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }

    /// Pull out one complete event, if a blank line has terminated one.
    ///
    /// Returns the value of the frame's `data:` lines, joined with `\n` as the
    /// event-stream format specifies. `None` means "not yet" - call again after
    /// pushing more bytes.
    pub fn next_event(&mut self) -> Option<String> {
        loop {
            let end = find(&self.pending, b"\n\n")?;
            let frame: Vec<u8> = self.pending.drain(..=end + 1).collect();
            let text = String::from_utf8_lossy(&frame);
            let mut data: Vec<&str> = Vec::new();
            for line in text.lines() {
                let line = line.trim_end_matches('\r');
                if line.is_empty() || line.starts_with(':') {
                    // Blank padding, or a `: keepalive` comment the server sends
                    // every 15 s to keep a half-open socket detectable.
                    continue;
                }
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push(rest.strip_prefix(' ').unwrap_or(rest));
                }
                // `id:`/`event:`/`retry:` carry nothing this plugin needs.
            }
            if data.is_empty() {
                // A frame with only `id:`/`event:` lines: not something to hand
                // out. Keep looking for the next frame.
                continue;
            }
            return Some(data.join("\n"));
        }
    }

    /// True when the buffer has grown past anything a sane server would send,
    /// which means the framing is broken rather than the frame being large.
    pub fn overflowed(&self) -> bool {
        self.pending.len() > MAX_FRAME_BYTES
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A live `text/event-stream` over a socket.
pub struct EventStream {
    reader: BufReader<TcpStream>,
    chunked: bool,
    /// Bytes still owed for the chunk being read (chunked encoding only).
    chunk_left: usize,
    parser: SseParser,
    eof: bool,
}

impl EventStream {
    /// Open `GET /api/events`.
    ///
    /// `idle` bounds how long a single read may block. The server sends a
    /// `: keepalive` every 15 s, so anything beyond ~45 s means a half-open
    /// socket, which never reports an error by itself.
    pub fn open(endpoint: &Endpoint, token: &str, idle: Duration) -> Result<Self, HttpError> {
        let (status, reader, chunked) = send(endpoint, "GET", "/api/events", token, None)?;
        if status != 200 {
            return Err(HttpError::Status(status, String::new()));
        }
        let _ = reader.get_ref().set_read_timeout(Some(idle));
        Ok(EventStream {
            reader,
            chunked,
            chunk_left: 0,
            parser: SseParser::new(),
            eof: false,
        })
    }

    /// Block for the next event. `None` means the stream ended or idled out, and
    /// the caller should reconnect.
    pub fn next_event(&mut self) -> Option<String> {
        loop {
            if let Some(event) = self.parser.next_event() {
                return Some(event);
            }
            if self.eof || self.parser.overflowed() {
                return None;
            }
            if !self.fill() {
                return None;
            }
        }
    }

    /// Pull more bytes from the socket into the parser. False on EOF/idle/error.
    fn fill(&mut self) -> bool {
        if self.chunked && self.chunk_left == 0 && !self.read_chunk_header() {
            return false;
        }

        let want = if self.chunked {
            self.chunk_left.min(8192)
        } else {
            8192
        };
        let mut buf = vec![0u8; want];
        match self.reader.read(&mut buf) {
            Ok(0) => {
                self.eof = true;
                false
            }
            Ok(n) => {
                self.parser.push(&buf[..n]);
                if self.chunked {
                    self.chunk_left -= n;
                    if self.chunk_left == 0 {
                        let mut crlf = String::new();
                        let _ = self.reader.read_line(&mut crlf);
                    }
                }
                true
            }
            // An idle or WouldBlock read is the watchdog's business, not an
            // error: surface it as "the stream is over" and let the caller decide.
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                self.eof = true;
                false
            }
            Err(_) => {
                self.eof = true;
                false
            }
        }
    }

    /// Read one chunk-size line. False means the body is over.
    fn read_chunk_header(&mut self) -> bool {
        loop {
            let mut header = String::new();
            match self.reader.read_line(&mut header) {
                Ok(0) => {
                    self.eof = true;
                    return false;
                }
                Ok(_) => {}
                Err(_) => {
                    self.eof = true;
                    return false;
                }
            }
            let size_hex = header.trim();
            if size_hex.is_empty() {
                continue; // a stray CRLF between chunks
            }
            match usize::from_str_radix(size_hex.split(';').next().unwrap_or("0"), 16) {
                Ok(0) => {
                    self.eof = true;
                    return false;
                }
                Ok(size) => {
                    self.chunk_left = size;
                    return true;
                }
                Err(_) => {
                    self.eof = true;
                    return false;
                }
            }
        }
    }
}

/// Percent-encode anything that is not unreserved, for a query value.
pub fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser_with(bytes: &[u8]) -> SseParser {
        let mut p = SseParser::new();
        p.push(bytes);
        p
    }

    #[test]
    fn a_plain_base_parses_with_the_default_port() {
        let e = Endpoint::parse("http://127.0.0.1:8787").unwrap();
        assert_eq!(e.host, "127.0.0.1");
        assert_eq!(e.port, 8787);
    }

    #[test]
    fn a_base_with_a_path_and_trailing_slash_still_parses() {
        let e = Endpoint::parse("http://localhost:9000/api/v1/").unwrap();
        assert_eq!(e.host, "localhost");
        assert_eq!(e.port, 9000);
    }

    #[test]
    fn https_is_refused_with_an_explanation_not_downgraded() {
        let message = Endpoint::parse("https://example.com")
            .unwrap_err()
            .to_string();
        assert!(message.contains("https"), "{message}");
        assert!(message.contains("TLS"), "{message}");
    }

    #[test]
    fn a_missing_port_defaults_to_80() {
        assert_eq!(Endpoint::parse("http://box").unwrap().port, 80);
    }

    #[test]
    fn a_non_numeric_port_is_a_configuration_error() {
        assert!(Endpoint::parse("http://box:http").is_err());
    }

    #[test]
    fn http_status_lines_parse_and_garbage_does_not() {
        assert_eq!(parse_status("HTTP/1.1 200 OK\r\n").unwrap(), 200);
        assert_eq!(parse_status("HTTP/1.1 409 Conflict\r\n").unwrap(), 409);
        assert!(parse_status("nonsense\r\n").is_err());
        assert!(parse_status("HTTP/1.1\r\n").is_err());
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(encode_query("s_abc-123"), "s_abc-123");
        assert_eq!(encode_query("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn an_sse_frame_is_extracted_from_its_data_line() {
        let mut p = parser_with(b"id: 7\ndata: {\"type\":\"text_delta\"}\n\n");
        assert_eq!(p.next_event().as_deref(), Some("{\"type\":\"text_delta\"}"));
        assert_eq!(p.next_event(), None);
    }

    #[test]
    fn a_keepalive_comment_is_skipped_and_the_next_event_returned() {
        let mut p = parser_with(b": keepalive\n\ndata: {\"type\":\"usage\"}\n\n");
        assert_eq!(p.next_event().as_deref(), Some("{\"type\":\"usage\"}"));
    }

    #[test]
    fn an_incomplete_frame_is_not_returned() {
        let mut p = parser_with(b"data: {\"type\":\"half\"");
        assert_eq!(p.next_event(), None);
        assert!(p.buffered() > 0);
    }

    #[test]
    fn a_frame_split_across_two_reads_is_reassembled() {
        let mut p = SseParser::new();
        p.push(b"data: {\"ty");
        assert_eq!(p.next_event(), None);
        p.push(b"pe\":\"x\"}\n\n");
        assert_eq!(p.next_event().as_deref(), Some("{\"type\":\"x\"}"));
    }

    #[test]
    fn a_multi_line_data_payload_is_joined_with_newlines() {
        // The event-stream spec joins multiple `data:` lines with `\n`. JSON never
        // needs it, but the parser must not silently drop the tail if it happens.
        let mut p = parser_with(b"data: one\ndata: two\n\n");
        assert_eq!(p.next_event().as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn several_frames_in_one_read_are_returned_in_order() {
        let mut p = parser_with(b"data: 1\n\ndata: 2\n\ndata: 3\n\n");
        assert_eq!(p.next_event().as_deref(), Some("1"));
        assert_eq!(p.next_event().as_deref(), Some("2"));
        assert_eq!(p.next_event().as_deref(), Some("3"));
        assert_eq!(p.next_event(), None);
    }

    #[test]
    fn crlf_line_endings_are_accepted() {
        let mut p = parser_with(b"id: 1\r\ndata: {\"ok\":true}\r\n\r\n");
        assert_eq!(p.next_event().as_deref(), Some("{\"ok\":true}"));
    }

    #[test]
    fn a_frame_with_no_data_lines_is_skipped_not_returned_as_empty() {
        let mut p = parser_with(b"event: ping\n\ndata: real\n\n");
        assert_eq!(p.next_event().as_deref(), Some("real"));
    }

    #[test]
    fn overflow_is_detected_rather_than_buffering_forever() {
        let mut p = SseParser::new();
        assert!(!p.overflowed());
        p.pending = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(p.overflowed());
    }
}
