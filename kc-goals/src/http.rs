//! A minimal HTTP/1.1 client: just enough to talk to the karucode server that
//! spawned this plugin.
//!
//! A plugin cannot add a dependency without shipping it, so this is written by
//! hand rather than pulled in as `ureq`: no TLS, no redirects, no pooling, no
//! proxies. It handles what karucode's server actually sends - `Content-Length`
//! on JSON routes, `Transfer-Encoding: chunked` when a handler streams - and
//! refuses anything else loudly instead of misparsing it.
//!
//! **Plain HTTP only**, loopback only. The plugin talks to the server that
//! spawned it; an `https://` base is a configuration error this build reports
//! rather than silently downgrading.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    BadUrl(String),
    Io(String),
    Malformed(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::BadUrl(m) => write!(f, "{m}"),
            HttpError::Io(m) => write!(f, "{m}"),
            HttpError::Malformed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for HttpError {}

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
                "`{base}` is not an http:// URL; this plugin only speaks plain HTTP to the \
                 loopback server that spawned it"
            ))
        })?;
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                let port = port.parse::<u16>().map_err(|_| {
                    HttpError::BadUrl(format!("`{base}` has a port that is not a number"))
                })?;
                (host.to_string(), port)
            }
            _ => (rest.to_string(), 80),
        };
        if host.is_empty() {
            return Err(HttpError::BadUrl(format!("`{base}` has no host")));
        }
        Ok(Endpoint {
            host: host.trim_start_matches('[').trim_end_matches(']').to_string(),
            port,
        })
    }
}

/// One HTTP response, as far as this plugin cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn json(&self) -> Result<serde_json::Value, HttpError> {
        if self.body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&self.body)
            .map_err(|e| HttpError::Malformed(format!("response body is not JSON: {e}")))
    }

    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The response head, parsed on its own so the framing rules are testable
/// without a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub content_length: Option<usize>,
    pub chunked: bool,
}

pub fn parse_head(head: &str) -> Result<Head, HttpError> {
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Malformed("empty response".into()))?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(HttpError::Malformed(format!(
            "`{status_line}` is not an HTTP status line"
        )));
    }
    let status: u16 = parts
        .next()
        .ok_or_else(|| HttpError::Malformed("no status code".into()))?
        .parse()
        .map_err(|_| HttpError::Malformed(format!("`{status_line}` has no status code")))?;
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    Ok(Head {
        status,
        content_length,
        chunked,
    })
}

/// A client for one server, with the bearer token the operator configured.
#[derive(Debug, Clone)]
pub struct Client {
    pub endpoint: Endpoint,
    pub token: String,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Result<Self, HttpError> {
        Ok(Self {
            endpoint: Endpoint::parse(base)?,
            token: token.to_string(),
        })
    }

    pub fn get(&self, path: &str) -> Result<Response, HttpError> {
        self.request("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &serde_json::Value) -> Result<Response, HttpError> {
        self.request("POST", path, Some(body))
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, HttpError> {
        let payload = match body {
            Some(value) => serde_json::to_vec(value)
                .map_err(|e| HttpError::Malformed(format!("could not encode the body: {e}")))?,
            None => Vec::new(),
        };
        let address = format!("{}:{}", self.endpoint.host, self.endpoint.port);
        let mut stream = TcpStream::connect_timeout(
            &address
                .to_socket_addrs()
                .map_err(|e| HttpError::Io(format!("{address}: {e}")))?
                .next()
                .ok_or_else(|| HttpError::Io(format!("{address} did not resolve")))?,
            CONNECT_TIMEOUT,
        )
        .map_err(|e| HttpError::Io(format!("{address}: {e}")))?;
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .and_then(|_| stream.set_write_timeout(Some(REQUEST_TIMEOUT)))
            .map_err(|e| HttpError::Io(e.to_string()))?;

        // `X-Karucode-Client: web` satisfies the CSRF header the server requires
        // on every mutating method; without it a POST is refused regardless of
        // the token.
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
             X-Karucode-Client: web\r\nAccept: application/json\r\n",
            self.endpoint.host
        );
        if !self.token.is_empty() {
            head.push_str(&format!("Authorization: Bearer {}\r\n", self.token));
        }
        if body.is_some() {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                payload.len()
            ));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .and_then(|_| stream.write_all(&payload))
            .and_then(|_| stream.flush())
            .map_err(|e| HttpError::Io(e.to_string()))?;
        read_response(stream)
    }
}

fn read_response(stream: TcpStream) -> Result<Response, HttpError> {
    let mut reader = BufReader::new(stream);
    let mut head_text = String::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| HttpError::Io(e.to_string()))?;
        if read == 0 {
            return Err(HttpError::Malformed(
                "the connection closed before the response head".into(),
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head_text.push_str(&line);
        if head_text.len() > 64 * 1024 {
            return Err(HttpError::Malformed("response head is too large".into()));
        }
    }
    let head = parse_head(&head_text)?;
    let body = if head.chunked {
        read_chunked(&mut reader)?
    } else if let Some(length) = head.content_length {
        let mut buf = vec![0u8; length.min(MAX_BODY_BYTES)];
        reader
            .read_exact(&mut buf)
            .map_err(|e| HttpError::Io(e.to_string()))?;
        String::from_utf8_lossy(&buf).to_string()
    } else {
        let mut buf = Vec::new();
        reader
            .take(MAX_BODY_BYTES as u64)
            .read_to_end(&mut buf)
            .map_err(|e| HttpError::Io(e.to_string()))?;
        String::from_utf8_lossy(&buf).to_string()
    };
    Ok(Response {
        status: head.status,
        body,
    })
}

fn read_chunked(reader: &mut BufReader<TcpStream>) -> Result<String, HttpError> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| HttpError::Io(e.to_string()))?;
        let size_text = line.trim().split(';').next().unwrap_or("").to_string();
        let size = usize::from_str_radix(size_text.trim(), 16)
            .map_err(|_| HttpError::Malformed(format!("bad chunk size `{size_text}`")))?;
        if size == 0 {
            // Trailer: read the terminating blank line and stop.
            let mut trailer = String::new();
            loop {
                trailer.clear();
                let read = reader
                    .read_line(&mut trailer)
                    .map_err(|e| HttpError::Io(e.to_string()))?;
                if read == 0 || trailer == "\r\n" || trailer == "\n" {
                    break;
                }
            }
            break;
        }
        if body.len() + size > MAX_BODY_BYTES {
            return Err(HttpError::Malformed("response body is too large".into()));
        }
        let mut buf = vec![0u8; size];
        reader
            .read_exact(&mut buf)
            .map_err(|e| HttpError::Io(e.to_string()))?;
        body.extend_from_slice(&buf);
        let mut crlf = String::new();
        reader
            .read_line(&mut crlf)
            .map_err(|e| HttpError::Io(e.to_string()))?;
    }
    Ok(String::from_utf8_lossy(&body).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_parses_host_and_port() {
        let e = Endpoint::parse("http://127.0.0.1:8787").unwrap();
        assert_eq!(e.host, "127.0.0.1");
        assert_eq!(e.port, 8787);
    }

    #[test]
    fn an_endpoint_without_a_port_defaults_to_80() {
        assert_eq!(Endpoint::parse("http://localhost").unwrap().port, 80);
    }

    #[test]
    fn https_is_refused_with_an_explanation() {
        let err = Endpoint::parse("https://example.test").unwrap_err();
        assert!(err.to_string().contains("plain HTTP"), "{err}");
    }

    #[test]
    fn a_status_line_without_a_code_is_refused() {
        assert!(parse_head("HTTP/1.1 nope").is_err());
        assert!(parse_head("not http").is_err());
    }

    #[test]
    fn the_head_finds_length_and_chunking() {
        let head = parse_head("HTTP/1.1 202 Accepted\r\nContent-Length: 12\r\nContent-Type: application/json");
        let head = head.unwrap();
        assert_eq!(head.status, 202);
        assert_eq!(head.content_length, Some(12));
        assert!(!head.chunked);

        let chunked = parse_head("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked").unwrap();
        assert!(chunked.chunked);
        assert_eq!(chunked.content_length, None);
    }

    #[test]
    fn an_empty_body_decodes_to_null_not_an_error() {
        let r = Response {
            status: 204,
            body: String::new(),
        };
        assert!(r.is_ok());
        assert_eq!(r.json().unwrap(), serde_json::Value::Null);
    }

    #[test]
    fn a_non_json_body_is_a_malformed_response() {
        let r = Response {
            status: 200,
            body: "<html>".into(),
        };
        assert!(r.json().is_err());
    }
}
