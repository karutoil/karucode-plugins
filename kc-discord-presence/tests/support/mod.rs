//! A scriptable stand-in for `discord.com`, for tests.
//!
//! Shared by two test targets that each use a different subset of it, so the
//! unused-method warnings are expected rather than a smell.
#![allow(dead_code)]

//! It speaks just enough HTTP/1.1 to answer one request per connection, records
//! every request it saw (method, path, query, JSON body) and replays a scripted
//! response. That is what lets the publisher be tested end to end - post, edit,
//! `429`, `404` - without a network or a real webhook.
//!
//! Used from `src/discord.rs`'s unit tests and from `tests/protocol.rs`, which is
//! why it lives in `tests/support/` and is pulled in with `#[path]`.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Option<Value>,
}

impl Recorded {
    /// The path without its query string.
    pub fn route(&self) -> &str {
        &self.path
    }
}

#[derive(Debug, Clone)]
struct Reply {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

pub struct FakeDiscord {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    replies: Arc<Mutex<Vec<Reply>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeDiscord {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a fake discord");
        let port = listener.local_addr().unwrap().port();
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let requests = Arc::clone(&requests);
            let replies = Arc::clone(&replies);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests);
                            let replies = Arc::clone(&replies);
                            std::thread::spawn(move || {
                                let _ = serve(stream, &requests, &replies);
                            });
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            port,
            requests,
            replies,
            stop,
            handle: Some(handle),
        }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}/api/v10", self.port)
    }

    pub fn url(&self, id: &str, token: &str) -> String {
        format!("{}/webhooks/{}/{}", self.base(), id, token)
    }

    /// Queue the next reply. Replies are consumed in order; when the queue is
    /// empty a successful message is answered.
    pub fn reply(&self, status: u16, body: Value) {
        self.replies.lock().unwrap().push(Reply {
            status,
            body: body.to_string(),
            headers: Vec::new(),
        });
    }

    pub fn reply_with_headers(&self, status: u16, body: Value, headers: &[(&str, &str)]) {
        self.replies.lock().unwrap().push(Reply {
            status,
            body: body.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn last(&self) -> Option<Recorded> {
        self.requests.lock().unwrap().last().cloned()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Wait until at least `n` requests have arrived (or time out).
    pub fn wait_for(&self, n: usize, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.count() >= n {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Drop for FakeDiscord {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<Recorded>>>,
    replies: &Arc<Mutex<Vec<Reply>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let parsed = serde_json::from_slice::<Value>(&body).ok();
    requests.lock().unwrap().push(Recorded {
        method: method.clone(),
        path,
        query,
        body: parsed,
    });

    let reply = replies.lock().unwrap().pop().unwrap_or_else(|| Reply {
        status: 200,
        body: json!({"id": "m_default", "channel_id": "c_default"}).to_string(),
        headers: Vec::new(),
    });
    let mut response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        status_text(reply.status),
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&reply.body);
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

// ---------------------------------------------------------------------------
// A stand-in for the Discord desktop client's IPC socket.
// ---------------------------------------------------------------------------

use std::os::unix::net::{UnixListener, UnixStream as IpcStream};
use std::path::PathBuf;

/// One frame the client received: the opcode and the decoded body.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedFrame {
    pub opcode: u32,
    pub body: Value,
}

/// A fake `discord-ipc-0` that speaks the same framing the real client does.
///
/// It answers a handshake with `READY`, acknowledges `SET_ACTIVITY`, and records
/// every frame, so a test can assert on the exact bytes a real client would have
/// received. `reject_handshake` makes it answer the way Discord does for a bad
/// Application ID; `drop_connections` makes it go away, which is what a client
/// restart looks like from the plugin's side.
pub struct FakeDiscordIpc {
    dir: PathBuf,
    path: PathBuf,
    frames: Arc<Mutex<Vec<RecordedFrame>>>,
    live: Arc<Mutex<Vec<IpcStream>>>,
    reject: Arc<Mutex<Option<(i64, String)>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeDiscordIpc {
    pub fn start() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kc-discord-ipc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create the fake runtime dir");
        let path = dir.join("discord-ipc-0");
        let listener = UnixListener::bind(&path).expect("bind discord-ipc-0");
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let frames = Arc::new(Mutex::new(Vec::new()));
        let live = Arc::new(Mutex::new(Vec::new()));
        let reject = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let frames = Arc::clone(&frames);
            let live = Arc::clone(&live);
            let reject = Arc::clone(&reject);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let frames = Arc::clone(&frames);
                            let live = Arc::clone(&live);
                            let reject = Arc::clone(&reject);
                            std::thread::spawn(move || {
                                let _ = serve_ipc(stream, &frames, &live, &reject);
                            });
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            dir,
            path,
            frames,
            live,
            reject,
            stop,
            handle: Some(handle),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The directory to hand the plugin as `XDG_RUNTIME_DIR`.
    pub fn runtime_dir(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn frames(&self) -> Vec<RecordedFrame> {
        self.frames.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.frames.lock().unwrap().len()
    }

    pub fn wait_for(&self, n: usize, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.count() >= n {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Answer the next handshake the way Discord answers a bad Application ID.
    pub fn reject_handshake(&self, code: i64, message: &str) {
        *self.reject.lock().unwrap() = Some((code, message.to_string()));
    }

    /// Close every live connection, as a client restart would.
    pub fn drop_connections(&self) {
        for stream in self.live.lock().unwrap().drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for FakeDiscordIpc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.drop_connections();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn serve_ipc(
    mut stream: IpcStream,
    frames: &Arc<Mutex<Vec<RecordedFrame>>>,
    live: &Arc<Mutex<Vec<IpcStream>>>,
    reject: &Arc<Mutex<Option<(i64, String)>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    live.lock().unwrap().push(stream.try_clone()?);
    loop {
        let mut head = [0u8; 8];
        if stream.read_exact(&mut head).is_err() {
            return Ok(());
        }
        let opcode = u32::from_le_bytes(head[0..4].try_into().expect("4 bytes"));
        let length = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes")) as usize;
        let mut body = vec![0u8; length];
        if stream.read_exact(&mut body).is_err() {
            return Ok(());
        }
        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        frames.lock().unwrap().push(RecordedFrame {
            opcode,
            body: value.clone(),
        });
        let reply = match opcode {
            0 => {
                if let Some((code, message)) = reject.lock().unwrap().take() {
                    let error = json!({
                        "cmd": "DISPATCH", "evt": "ERROR",
                        "data": { "code": code, "message": message }
                    });
                    write_ipc(&mut stream, 1, &error)?;
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return Ok(());
                }
                json!({
                    "cmd": "DISPATCH", "evt": "READY",
                    "data": { "v": 1, "config": {}, "user": { "id": "1", "username": "tester" } }
                })
            }
            1 => {
                let nonce = value.get("nonce").cloned().unwrap_or(Value::Null);
                json!({
                    "cmd": value.get("cmd").cloned().unwrap_or(Value::Null),
                    "data": {},
                    "evt": Value::Null,
                    "nonce": nonce
                })
            }
            3 => json!({}),
            _ => json!({}),
        };
        let reply_opcode = if opcode == 3 { 4 } else { 1 };
        write_ipc(&mut stream, reply_opcode, &reply)?;
    }
}

fn write_ipc(stream: &mut IpcStream, opcode: u32, payload: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let mut buffer = Vec::with_capacity(8 + body.len());
    buffer.extend_from_slice(&opcode.to_le_bytes());
    buffer.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&body);
    stream.write_all(&buffer)?;
    stream.flush()
}
