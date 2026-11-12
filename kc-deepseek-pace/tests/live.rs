//! Live tests against a running karucode server.
//!
//! Ignored by default (`cargo test` stays hermetic); run them explicitly:
//!
//! ```bash
//! KC_PACE_LIVE=1 cargo test --release --test live -- --test-threads=1 --nocapture
//! ```
//!
//! These exist because the watcher is the core of the plugin and it is the one
//! part a unit test cannot honestly cover: it needs a real `text/event-stream`,
//! real chunked framing, a real session id and a real cancel/steer round trip.
//! Everything below talks to the server the same way the plugin does.
//!
//! Set `KC_PACE_SERVER` to point somewhere other than `http://127.0.0.1:8787`.

#![allow(clippy::needless_range_loop)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn server() -> String {
    std::env::var("KC_PACE_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string())
}

fn live() -> bool {
    std::env::var("KC_PACE_LIVE").ok().as_deref() == Some("1")
}

/// A tiny blocking client, separate from the plugin's own so a bug in `http.rs`
/// cannot make a test pass against itself.
struct Client {
    base: String,
}

impl Client {
    fn new() -> Self {
        Self { base: server() }
    }

    fn addr(&self) -> String {
        self.base.trim_start_matches("http://").to_string()
    }

    fn get(&self, path: &str) -> (u16, String) {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str) -> (u16, String) {
        self.request("POST", path, Some("{}"))
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let addr = self.addr();
        let mut stream = TcpStream::connect(&addr).unwrap_or_else(|e| {
            panic!("cannot reach the server at {addr}: {e}. Set KC_PACE_SERVER or start it first.")
        });
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nX-Karucode-Client: web\r\n\
             Connection: close\r\n"
        );
        if let Some(body) = body {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        if let Some(body) = body {
            req.push_str(body);
        }
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut raw = Vec::new();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line.trim().is_empty() {
                break;
            }
        }
        let _ = reader.read_to_end(&mut raw);
        (status, String::from_utf8_lossy(&raw).to_string())
    }

    /// The session list, newest first - the server's own ordering.
    fn sessions(&self) -> Vec<String> {
        let (status, body) = self.get("/api/sessions");
        assert_eq!(status, 200, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).expect("session list json");
        let array = value
            .get("sessions")
            .and_then(|s| s.as_array())
            .or_else(|| value.as_array())
            .expect("a sessions array");
        array
            .iter()
            .filter_map(|s| s.get("id").and_then(|i| i.as_str()))
            .map(str::to_string)
            .collect()
    }
}

#[test]
fn the_servers_event_stream_frames_read_the_way_the_plugin_expects() {
    if !live() {
        eprintln!("skipping: set KC_PACE_LIVE=1 to run the live tests");
        return;
    }
    let client = Client::new();
    let addr = client.addr();
    let mut stream = TcpStream::connect(&addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .write_all(
            format!(
                "GET /api/events HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    reader.read_line(&mut head).unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    // The plugin's client hand-parses chunk framing, so this is not incidental:
    // if the server ever stopped chunking, `fill()` would have to be revisited.
    assert!(chunked, "the event stream is expected to be chunk-framed");

    // Read for a few seconds and assert every frame we got parses as an event
    // with the fields the detector reads.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut frames = 0usize;
    let mut seen_types = std::collections::BTreeSet::new();
    let mut buf = vec![0u8; 8192];
    let mut acc: Vec<u8> = Vec::new();
    while Instant::now() < deadline && frames < 50 {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        acc.extend_from_slice(&buf[..n]);
        // Chunk headers and payloads are interleaved; search for `data:` lines
        // rather than implementing the framing twice.
        let text = String::from_utf8_lossy(&acc).to_string();
        let mut consumed = 0usize;
        for line in text.lines() {
            consumed += line.len() + 1;
            let Some(rest) = line.trim_end_matches('\r').strip_prefix("data: ") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(rest) else {
                continue;
            };
            let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if kind.is_empty() {
                continue;
            }
            frames += 1;
            seen_types.insert(kind.to_string());
            // Every frame the detector reads must carry a session id; that is the
            // assumption `watch::handle` makes when it early-returns.
            assert!(
                value.get("session_id").and_then(|s| s.as_str()).is_some(),
                "a frame with no session_id would be dropped by the watcher: {value}"
            );
        }
        if consumed > 0 && consumed <= acc.len() {
            acc.drain(..consumed);
        }
    }
    eprintln!("frames: {frames}, types: {seen_types:?}");
    assert!(frames > 0, "the event stream produced no frames in 8s");
}

#[test]
fn the_routes_the_ladder_calls_answer_the_way_the_plugin_expects() {
    if !live() {
        eprintln!("skipping: set KC_PACE_LIVE=1 to run the live tests");
        return;
    }
    let client = Client::new();
    let sessions = client.sessions();
    let Some(session) = sessions.first() else {
        eprintln!("skipping: the server has no sessions to probe");
        return;
    };

    // `/info` is where `api::session_model` reads the model from. If this shape
    // changes the model gate silently fails open, so it is asserted directly.
    let (status, body) = client.get(&format!("/api/sessions/{session}/info"));
    assert_eq!(status, 200, "{body}");
    let info: serde_json::Value = serde_json::from_str(&body).expect("info json");
    assert!(
        info.get("context").is_some(),
        "`api::session_model` reads `.context.model`: {body}"
    );
    eprintln!(
        "session {session} model: {:?}",
        info.pointer("/context/model")
    );

    // Cancel on an idle session is a harmless no-op that answers 200 with
    // `cancelled: false`. That is exactly what the cancel rung relies on when a
    // run finished between the decision and the call.
    let (status, body) = client.post(&format!("/api/sessions/{session}/cancel"));
    assert_eq!(status, 200, "{body}");
    let cancelled: serde_json::Value = serde_json::from_str(&body).expect("cancel json");
    assert!(
        cancelled.get("cancelled").is_some(),
        "the cancel route must answer with a `cancelled` field: {body}"
    );
    eprintln!("cancel on an idle session -> {cancelled}");

    // A steer with nothing running is *refused* (400/409), not queued. The plugin
    // treats that as a failed action and reports it, so the status code is part
    // of the contract.
    let (status, body) = client.request(
        "POST",
        &format!("/api/sessions/{session}/messages?mode=steer"),
        Some("{\"content\":\"test\"}"),
    );
    assert!(
        status == 400 || status == 404 || status == 409,
        "a steer with no run must be refused, got {status}: {body}"
    );
    eprintln!("steer with no run -> {status}");
}

#[test]
fn a_plugins_own_watcher_reads_the_live_stream_and_reports_it() {
    if !live() {
        eprintln!("skipping: set KC_PACE_LIVE=1 to run the live tests");
        return;
    }
    // Drive the real binary with `watch` ON, and ask its panel whether it managed
    // to connect. This is the end-to-end proof that `http::EventStream` speaks
    // the server's framing - a unit test with a fake socket cannot give that.
    use serde_json::json;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_kc-deepseek-pace"))
        .env(
            "KARUCODE_PLUGIN_SETTINGS",
            json!({"watch": true, "server": server(), "dry_run": true}).to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the plugin");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol":"1"}})
    )
    .unwrap();
    stdin.flush().unwrap();

    // Give the watcher time to connect and see at least one frame.
    let mut connected = false;
    let mut seen_a_turn = false;
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut id = 1u64;
    while Instant::now() < deadline {
        id += 1;
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":"panels/read",
                   "params":{"panel":"pace","session_id":""}})
        )
        .unwrap();
        stdin.flush().unwrap();
        let expect = id;
        let mut panel = None;
        while let Ok(line) = lines.recv_timeout(Duration::from_secs(5)) {
            let value: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            if value.get("id").and_then(|i| i.as_u64()) == Some(expect) {
                panel = Some(value);
                break;
            }
        }
        let Some(panel) = panel else {
            eprintln!("no panel response for id {expect}");
            continue;
        };
        // The raw frame is the JSON-RPC response, so the panel payload sits under
        // `result.data` - unlike the helper in tests/protocol.rs, which returns
        // `result` directly.
        let data = &panel["result"]["data"];
        if data["connected"].as_bool() == Some(true) {
            connected = true;
        }
        if data["live_runs"].as_u64().unwrap_or(0) > 0 {
            seen_a_turn = true;
        }
        if connected {
            eprintln!(
                "watcher connected={connected} live_runs={} last_error={:?}",
                data["live_runs"], data["last_error"]
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        connected,
        "the plugin's watcher never connected to {}/api/events",
        server()
    );
    eprintln!("saw_live_turn={seen_a_turn}");
}
