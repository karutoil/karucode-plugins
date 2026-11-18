//! Integration tests: drive the real plugin binary over the wire protocol
//! against a **scripted karucode API server** (std TcpListener in-process),
//! plus a live mode against a real karucode server.
//!
//! The scripted server answers the five calls the plugin makes - create
//! session, upload image, send message, stream events, delete session - so
//! the whole handoff is exercised without any model or network. `#[ignore]`d
//! tests at the bottom run the real thing.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;

const BIN: &str = "target/debug/image-handoff";

/// What one scripted exchange saw, for assertions.
#[derive(Debug, Clone, Default)]
struct Seen {
    request_path: String,
    auth_header: Option<String>,
    body: Value,
    raw_body: Option<Vec<u8>>,
}

impl Seen {
    fn assert_bearer(&self, fragment: &str) {
        let header = self.auth_header.as_deref().expect("expected Authorization");
        assert!(header.contains(fragment), "wrong header: {header}");
    }
}

/// Read one HTTP request; returns what was seen plus a writer clone.
fn read_request(stream: TcpStream) -> (Seen, TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut seen = Seen {
        request_path: request_line.trim().to_string(),
        auth_header: None,
        body: Value::Null,
        raw_body: None,
    };
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("authorization:") {
            seen.auth_header = Some(trimmed.to_string());
        }
        if let Some(len) = lower.strip_prefix("content-length:") {
            content_length = len.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf).unwrap();
        seen.raw_body = Some(buf.clone());
        seen.body = serde_json::from_slice(&buf).unwrap_or(Value::Null);
    }
    (seen, stream)
}

fn respond_json(stream: &TcpStream, status: u16, body: &Value) {
    let mut writer = stream.try_clone().unwrap();
    let text = serde_json::to_string(body).unwrap();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        text.len()
    );
    writer.write_all(head.as_bytes()).unwrap();
    writer.write_all(text.as_bytes()).unwrap();
    writer.flush().unwrap();
}

fn respond_events(stream: &TcpStream, frames: &[Value]) {
    let mut writer = stream.try_clone().unwrap();
    let mut payload = String::new();
    for (i, frame) in frames.iter().enumerate() {
        payload.push_str(&format!("id: {}\ndata: {}\n\n", i + 1, frame));
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{payload}"
    );
    writer.write_all(response.as_bytes()).unwrap();
    writer.flush().unwrap();
    // Keep the stream open a moment so the reader consumes it before EOF.
    thread::sleep(std::time::Duration::from_millis(100));
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    }
}

struct PluginProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    rpc_settings: Value,
}

impl PluginProcess {
    /// Hermetic default: a fixture karucode config, never the real install.
    fn start(settings: &Value) -> Self {
        let dir = fixture_data_dir();
        Self::spawn(settings, Some(dir), json!({}))
    }

    fn start_with_rpc(settings: &Value, rpc_settings: Value) -> Self {
        let dir = fixture_data_dir();
        Self::spawn(settings, Some(dir), rpc_settings)
    }

    /// For the live test: keep whatever `KARUCODE_DATA_DIR` / `KARUCODE_CONFIG`
    /// the harness set, so model resolution runs exactly as the hosted plugin
    /// would (the default `spawn` pins a fixture instead).
    fn start_keeping_host_env(settings: &Value) -> Self {
        Self::spawn(settings, None, json!({}))
    }

    fn spawn(settings: &Value, data_dir: Option<std::path::PathBuf>, rpc_settings: Value) -> Self {
        let mut command = Command::new(BIN);
        if let Some(dir) = &data_dir {
            command.env("KARUCODE_DATA_DIR", dir);
            command.env("KARUCODE_CONFIG", dir.join("nonexistent.toml"));
        }
        let mut child = command
            .env("KARUCODE_PLUGIN_SETTINGS", settings.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn plugin binary; run `cargo build` first");
        let stdin = child.stdin.take().expect("plugin stdin");
        let stdout = child.stdout.take().expect("plugin stdout");
        Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            rpc_settings,
        }
    }

    fn call(&mut self, id: i64, request: Value) -> Value {
        let frame = wire(id, &request);
        self.stdin
            .write_all(format!("{frame}\n").as_bytes())
            .unwrap();
        self.stdin.flush().unwrap();
        self.recv(id)
    }

    fn recv(&mut self, id: i64) -> Value {
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).expect("read plugin line");
            assert!(!line.is_empty(), "plugin closed stdout");
            let value: Value = serde_json::from_str(line.trim()).expect("parse plugin response");
            if value.get("method").and_then(Value::as_str) == Some("plugin/settings/get") {
                if let Some(req_id) = value.get("id") {
                    let reply = json!({"jsonrpc": "2.0", "id": req_id, "result": {"settings": self.rpc_settings}});
                    self.stdin
                        .write_all(format!("{reply}\n").as_bytes())
                        .unwrap();
                    self.stdin.flush().unwrap();
                }
                continue;
            }
            assert_eq!(value["id"], json!(id), "response id mismatch: {value}");
            return value;
        }
    }

    fn initialize(&mut self) -> Value {
        self.call(
            1,
            json!({"method": "initialize", "params": {"protocol": "1"}}),
        )
    }
}

/// Build one wire frame.
fn wire(id: i64, request: &Value) -> String {
    let mut frame = serde_json::Map::new();
    frame.insert("jsonrpc".into(), json!("2.0"));
    frame.insert("id".into(), json!(id));
    if let Some(method) = request.get("method").and_then(Value::as_str) {
        frame.insert("method".into(), json!(method));
        frame.insert(
            "params".into(),
            request.get("params").cloned().unwrap_or(json!({})),
        );
    }
    Value::Object(frame).to_string()
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extract the tool result text and ok-flag from a tools/call response.
fn tool_result(response: &Value) -> (bool, String) {
    let result = &response["result"];
    let ok = result["ok"].as_bool().unwrap_or(false);
    let text = result["content"].as_str().unwrap_or_default().to_string();
    (ok, text)
}

/// A data dir with a karucode-style settings DB: one provider, a vision model.
fn fixture_data_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ih-flow-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let providers = json!([{
        "id": "omniroute",
        "base_url": "http://127.0.0.1:20128/v1",
        "api_key": "sk-fixture-live",
        "models": [
            {"id": "od/vision-a", "vision": true},
            {"id": "od/vision-b", "vision": true},
            {"id": "od/text-only", "vision": false}
        ]
    }]);
    let conn = rusqlite::Connection::open(dir.join("karucode.db")).unwrap();
    conn.execute(
        "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL DEFAULT 0)",
        [],
    )
    .unwrap();
    for (key, value) in [
        ("model.providers", providers.to_string()),
        ("model.active_provider", "omniroute".to_string()),
        // A text-only active model: the calling session cannot see, so the
        // handoff engages (and falls back to a vision sibling at create).
        ("model.active_model", "od/text-only".to_string()),
    ] {
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }
    dir
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00\x90wS\x00\x00\x00\x0cIDAT\x08\xd7c\xf8\xff\xff?\x00\x05\xfe\x02\xfe\xa7]\x8f\x00\x00\x00\x00IEND\xaeB`\x82";

fn temp_png() -> String {
    let dir = std::env::temp_dir().join(format!(
        "ih-flow-png-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("red.png");
    std::fs::write(&path, PNG).unwrap();
    path.to_string_lossy().to_string()
}

/// The frames a successful vision run emits on the event stream.
fn run_frames(session_id: &str, answer: &str) -> Vec<Value> {
    vec![
        json!({"seq": 1, "type": "run_started", "session_id": session_id, "run_id": "r_1"}),
        json!({"seq": 2, "type": "text_delta", "session_id": session_id, "delta": answer}),
        json!({"seq": 3, "type": "run_finished", "session_id": session_id, "run_id": "r_1", "status": "completed", "error": null}),
    ]
}

/// Wire a whole fake karucode interaction for one `see` call.
///
/// One dispatcher thread accepts every connection and hands it to its own
/// handler thread; each handler routes by request line, because the plugin's
/// persistent event reader and the handoff flow race for the listen backlog
/// and any fixed order would be a coin flip. The events connection streams
/// the run's frames once the message has been posted, then holds open (the
/// reader keeps the connection); every JSON request is answered immediately.
fn serve_see_flow(
    answer: &str,
    ephemeral: bool,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Seen>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let session_id = "s_fake123".to_string();
    // Progress: 0 start, 1 created, 2 uploaded, 3 message posted (frames
    // stream), 4 delete answered.
    let step = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));

    let record = |seen: &std::sync::Arc<std::sync::Mutex<Vec<Seen>>>, request: Seen| {
        seen.lock().unwrap().push(request);
    };

    for _ in 0..2 {
        // One accept loop per clone; both spawn per-connection handlers.
        let listener = listener.try_clone().expect("clone listener");
        let seen = seen.clone();
        let session_id = session_id.clone();
        let answer = answer.to_string();
        let step = step.clone();
        thread::spawn(move || loop {
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let seen = seen.clone();
            let session_id = session_id.clone();
            let answer = answer.clone();
            let step = step.clone();
            thread::spawn(move || {
                let (request, rest) = read_request(stream);
                let path = request.request_path.clone();
                let mut request = request;
                if path.starts_with("GET /api/events") {
                    record(&seen, request);
                    // The frames stream once the message has been posted.
                    while step.load(std::sync::atomic::Ordering::SeqCst) < 3 {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    respond_events(&rest, &run_frames(&session_id, &answer));
                    // Hold the connection: the plugin's reader keeps it.
                    thread::sleep(std::time::Duration::from_secs(30));
                    return;
                }
                if path.starts_with("GET /api/sessions/") {
                    // The session's own row: `model` is the pin or null for
                    // the install default. These sessions follow the default
                    // (the fixture's text-only active model).
                    respond_json(
                        &rest,
                        200,
                        &json!({
                            "session": {"id": session_id, "model": null, "title": "t"}
                        }),
                    );
                    record(&seen, request);
                    return;
                }
                if path.starts_with("POST /api/sessions HTTP") {
                    let asks_ephemeral = request
                        .body
                        .get("ephemeral")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let refused = asks_ephemeral && !ephemeral;
                    if refused {
                        // An old server: unknown field. The plugin retries.
                        respond_json(
                            &rest,
                            400,
                            &json!({"error": {"code": "bad_request", "message": "unknown field `ephemeral`"}}),
                        );
                        record(&seen, request);
                        return;
                    }
                    respond_json(
                        &rest,
                        200,
                        &json!({"id": session_id, "ephemeral": asks_ephemeral, "title": "t"}),
                    );
                    record(&seen, request);
                    step.store(1, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
                if path.starts_with("POST /api/workspace/upload") {
                    let size = request.raw_body.as_ref().map(|b| b.len()).unwrap_or(0);
                    respond_json(
                        &rest,
                        201,
                        &json!({
                            "session": session_id,
                            "name": "image.png",
                            "path": format!(".karucode/attachments/{session_id}/image.png"),
                            "size": size,
                            "content_type": "image/png",
                            "deduplicated": false,
                        }),
                    );
                    request.body = Value::Null;
                    record(&seen, request);
                    step.store(2, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
                if path.contains("/messages") {
                    respond_json(
                        &rest,
                        202,
                        &json!({"mode": "send", "session_id": session_id, "run_id": "r_1"}),
                    );
                    record(&seen, request);
                    // Frames may stream now.
                    step.store(3, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
                if path.starts_with("DELETE /api/sessions") {
                    respond_json(&rest, 200, &json!({"deleted": true}));
                    record(&seen, request);
                    return;
                }
                respond_json(
                    &rest,
                    404,
                    &json!({"error": {"code": "not_found", "message": "unexpected"}}),
                );
            });
        });
    }
    (url, seen)
}

/// Wait (bounded) until the recorded requests reach `count`, because the
/// server thread records the last exchange after the plugin has its answer.
fn wait_for_count(seen: &std::sync::Arc<std::sync::Mutex<Vec<Seen>>>, count: usize) -> Vec<Seen> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        {
            let guard = seen.lock().unwrap();
            if guard.len() >= count {
                return guard.clone();
            }
        }
        if std::time::Instant::now() >= deadline {
            return seen.lock().unwrap().clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn settings_for(server_url: &str, token: &str) -> Value {
    json!({"server": server_url, "api_token": token})
}

/// A minimal fake that answers `GET /api/sessions/:id` with a model pin (or
/// null) and nothing else: enough to drive the vision gate.
struct SessionLookupServer {
    url: String,
}

impl SessionLookupServer {
    fn start(model: Option<(String, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        let listener2 = listener.try_clone().unwrap();
        thread::spawn(move || {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let (_, rest) = read_request(stream);
                let model = match &model {
                    Some((provider, model)) => json!({
                        "provider": provider, "model": model, "reasoning_effort": null
                    }),
                    None => Value::Null,
                };
                respond_json(
                    &rest,
                    200,
                    &json!({"session": {"id": "s_t", "model": model, "title": "t"}}),
                );
            }
        });
        let _ = listener2;
        Self { url }
    }
}

#[test]
fn see_refuses_for_a_session_whose_model_can_view_images() {
    // The calling session is pinned to a vision model.
    let server = SessionLookupServer::start(Some(("omniroute".into(), "od/vision-a".into())));
    let mut plugin = PluginProcess::start(&json!({
        "server": server.url, "api_token": "kck"
    }));
    plugin.initialize();
    let response = plugin.call(
        2,
        json!({"method": "tools/call", "params": {
            "name": "see",
            "arguments": {"path": temp_png(), "question": "what is this?"},
            "context": {"session_id": "s_t", "workspace": "/tmp"}
        }}),
    );
    let (ok, text) = tool_result(&response);
    assert!(!ok, "a vision session must be refused: {text}");
    assert!(text.contains("read_file"), "{text}");
    assert!(text.contains("can view images directly"), "{text}");

    // And the prompt note is suppressed for that session: the hook answers
    // null, which the host drops.
    let hook = plugin.call(
        3,
        json!({"method": "hooks/run", "params": {
            "hook": "before_prompt",
            "payload": {"session_id": "s_t", "workspace": "/tmp"}
        }}),
    );
    assert!(hook["result"]["output"].is_null(), "full response: {hook}");
}

#[test]
fn see_serves_a_text_only_session_and_notes_it() {
    // The calling session follows the install default: the fixture's
    // text-only active model.
    let server = SessionLookupServer::start(None);
    let mut plugin = PluginProcess::start(&json!({
        "server": server.url, "api_token": "kck"
    }));
    plugin.initialize();
    let hook = plugin.call(
        2,
        json!({"method": "hooks/run", "params": {
            "hook": "before_prompt",
            "payload": {"session_id": "s_t", "workspace": "/tmp"}
        }}),
    );
    let note = hook["result"]["output"].as_str().unwrap_or_default();
    assert!(note.contains("plugin__image-handoff__see"), "{note}");
    assert!(note.contains("cannot view images"), "{note}");

    // `see` on this server would need the full flow; the note test is the
    // point here - the refusal case is covered above, the full flow in the
    // serve_see_flow tests (whose sessions also resolve text-only).
}

#[test]
fn initialize_and_list_tools() {
    let mut plugin = PluginProcess::start(&json!({}));
    let init = plugin.initialize();
    assert_eq!(init["result"]["name"], json!("image-handoff"));
    assert_eq!(init["result"]["protocol"], json!("1"));
    let listed = plugin.call(2, json!({"method": "tools/list", "params": {}}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["see", "status"]);
}

#[test]
fn see_hands_off_entirely_through_karucode_on_an_ephemeral_server() {
    let (url, seen) = serve_see_flow("The sign says GREEN OTTER 31415.", true);
    let mut plugin = PluginProcess::start(&settings_for(&url, "kck_test_token"));
    plugin.initialize();
    let response = plugin.call(
        2,
        json!({"method": "tools/call", "params": {
            "name": "see",
            "arguments": {"path": temp_png(), "question": "what does the sign say?"},
            "context": {"session_id": "s_t", "workspace": "/tmp"}
        }}),
    );
    let (ok, text) = tool_result(&response);
    assert!(ok, "full response: {response}");
    let parsed: Value = serde_json::from_str(&text).expect("tool result is JSON");
    assert_eq!(parsed["answer"], json!("The sign says GREEN OTTER 31415."));
    assert_eq!(parsed["model"], json!("od/vision-a"));
    assert_eq!(parsed["ephemeral_session"], json!(true));
    assert!(parsed["image_path"]
        .as_str()
        .unwrap()
        .starts_with(".karucode/attachments/"));

    // The vision-gate session lookup precedes everything.
    let requests = wait_for_count(&seen, 5);
    assert_eq!(
        requests.len(),
        5,
        "lookup + create + upload + message + events: {requests:#?}"
    );
    assert!(requests[0]
        .request_path
        .starts_with("GET /api/sessions/s_t"));
    // The create carries the model pin and the ephemeral flag, authenticated.
    let create = &requests[1];
    assert!(create.request_path.starts_with("POST /api/sessions"));
    create.assert_bearer("kck_test_token");
    assert_eq!(create.body["ephemeral"], json!(true));
    assert_eq!(create.body["model"]["provider"], json!("omniroute"));
    assert_eq!(create.body["model"]["model"], json!("od/vision-a"));
    // The upload carries the real image bytes.
    let upload = &requests[2];
    assert!(upload
        .request_path
        .starts_with("POST /api/workspace/upload"));
    assert_eq!(upload.raw_body.as_deref(), Some(PNG));
    // The message references the uploaded path with an attachment chip.
    let message = &requests[3];
    assert!(message
        .request_path
        .starts_with("POST /api/sessions/s_fake123/messages"));
    let content = message.body["content"].as_str().unwrap();
    assert!(content.contains("what does the sign say?"), "{content}");
    assert!(content.contains(".karucode/attachments/"), "{content}");
    assert_eq!(message.body["attachments"][0]["mode"], json!("upload"));
    // No delete: the ephemeral session retires itself.
    assert!(requests
        .iter()
        .all(|r| !r.request_path.starts_with("DELETE")));
}

#[test]
fn see_falls_back_to_a_durable_session_on_an_older_server() {
    let (url, seen) = serve_see_flow("fallback answer", false);
    let mut plugin = PluginProcess::start(&settings_for(&url, "kck_test_token"));
    plugin.initialize();
    let response = plugin.call(
        2,
        json!({"method": "tools/call", "params": {
            "name": "see",
            "arguments": {"path": temp_png(), "question": "what?"},
            "context": {"session_id": "s_t", "workspace": "/tmp"}
        }}),
    );
    let (ok, text) = tool_result(&response);
    assert!(ok, "full response: {response}");
    let parsed: Value = serde_json::from_str(&text).expect("tool result is JSON");
    assert_eq!(parsed["answer"], json!("fallback answer"));
    assert_eq!(parsed["ephemeral_session"], json!(false));

    // lookup + probe (refused) + retry create + upload + message + events + delete
    let requests = wait_for_count(&seen, 7);
    assert_eq!(requests.len(), 7, "requests: {requests:#?}");
    let last = requests.last().unwrap();
    assert!(last.request_path.starts_with("DELETE /api/sessions/"));
}

#[test]
fn a_pending_run_is_collected_by_a_second_call_with_the_session_id() {
    // A server whose run "hangs": the message is accepted but the frames only
    // stream after the second tool call asks for them (step 4).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Seen>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let session_id = "s_pending1".to_string();
    // 0 created, 1 uploaded, 2 message posted, 3 frames released.
    let step = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
    let answer = "LATE ANSWER".to_string();

    for _ in 0..2 {
        let listener = listener.try_clone().expect("clone");
        let seen = seen.clone();
        let session_id = session_id.clone();
        let answer = answer.clone();
        let step = step.clone();
        thread::spawn(move || loop {
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let seen = seen.clone();
            let session_id = session_id.clone();
            let answer = answer.clone();
            let step = step.clone();
            thread::spawn(move || {
                let (request, rest) = read_request(stream);
                let path = request.request_path.clone();
                let mut request = request;
                if path.starts_with("GET /api/sessions/") {
                    respond_json(
                        &rest,
                        200,
                        &json!({
                            "session": {"id": session_id, "model": null, "title": "t"}
                        }),
                    );
                    seen.lock().unwrap().push(request);
                    return;
                }
                if path.starts_with("GET /api/events") {
                    seen.lock().unwrap().push(request);
                    // Frames arrive only after the message (step 3) AND an
                    // 8-second delay: longer than the first call's 3s wait,
                    // so that call returns pending; shorter than the second
                    // call's wait, so the collect observes the answer.
                    while step.load(std::sync::atomic::Ordering::SeqCst) < 3 {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    thread::sleep(std::time::Duration::from_secs(8));
                    respond_events(&rest, &run_frames(&session_id, &answer));
                    thread::sleep(std::time::Duration::from_secs(30));
                    return;
                }
                if path.starts_with("POST /api/sessions HTTP") {
                    respond_json(
                        &rest,
                        200,
                        &json!({"id": session_id, "ephemeral": true, "title": "t"}),
                    );
                    seen.lock().unwrap().push(request);
                    step.store(1, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
                if path.starts_with("POST /api/workspace/upload") {
                    let size = request.raw_body.as_ref().map(|b| b.len()).unwrap_or(0);
                    respond_json(
                        &rest,
                        201,
                        &json!({"session": session_id, "name": "image.png",
                                "path": format!(".karucode/attachments/{session_id}/image.png"),
                                "size": size, "content_type": "image/png", "deduplicated": false}),
                    );
                    request.body = Value::Null;
                    seen.lock().unwrap().push(request);
                    step.store(2, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
                if path.contains("/messages") {
                    respond_json(
                        &rest,
                        202,
                        &json!({"mode": "send", "session_id": session_id, "run_id": "r_1"}),
                    );
                    seen.lock().unwrap().push(request);
                    step.store(3, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
                respond_json(
                    &rest,
                    404,
                    &json!({"error": {"code": "not_found", "message": "unexpected"}}),
                );
            });
        });
    }

    let mut plugin = PluginProcess::start(&json!({
        "server": url,
        "api_token": "kck",
        "run_timeout_secs": 6,
    }));
    plugin.initialize();
    let response = plugin.call(
        2,
        json!({"method": "tools/call", "params": {
            "name": "see",
            "arguments": {"path": temp_png(), "question": "what?"},
            "context": {"session_id": "s_t", "workspace": "/tmp"}
        }}),
    );
    let (ok, text) = tool_result(&response);
    assert!(ok, "full response: {response}");
    let parsed: Value = serde_json::from_str(&text).expect("tool result is JSON");
    assert_eq!(parsed["pending"], json!(true), "text: {text}");
    let session_id = parsed["session_id"].as_str().unwrap().to_string();

    // The collect call: no question needed, just the session id.
    let response = plugin.call(
        3,
        json!({"method": "tools/call", "params": {
            "name": "see",
            "arguments": {"session_id": session_id},
            "context": {"session_id": "s_t", "workspace": "/tmp"}
        }}),
    );
    let (ok, text) = tool_result(&response);
    assert!(ok, "full response: {response}");
    let parsed: Value = serde_json::from_str(&text).expect("tool result is JSON");
    assert_eq!(parsed["answer"], json!("LATE ANSWER"));
}

#[test]
fn a_per_call_model_override_pins_that_model() {
    let (url, seen) = serve_see_flow("override answer", true);
    let mut plugin = PluginProcess::start(&settings_for(&url, "kck"));
    plugin.initialize();
    let response = plugin.call(
        2,
        json!({"method": "tools/call", "params": {
            "name": "see",
            "arguments": {"path": temp_png(), "question": "what?", "model": "od/vision-b"},
            "context": {"session_id": "s_t", "workspace": "/tmp"}
        }}),
    );
    let (ok, _) = tool_result(&response);
    assert!(ok, "full response: {response}");
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests[1].body["model"]["model"], json!("od/vision-b"));
}

// ─── Live tests: a real karucode server ──────────────────────────────────────

fn live_config() -> Option<(String, String, Option<String>)> {
    let server = std::env::var("IMAGE_HANDOFF_LIVE_SERVER").ok()?;
    let token = std::env::var("IMAGE_HANDOFF_LIVE_TOKEN").ok()?;
    let model = std::env::var("IMAGE_HANDOFF_LIVE_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty());
    Some((server, token, model))
}

#[test]
#[ignore = "live: needs IMAGE_HANDOFF_LIVE_SERVER + IMAGE_HANDOFF_LIVE_TOKEN (a real karucode); optional IMAGE_HANDOFF_LIVE_MODEL"]
fn live_round_trip_reads_text_through_a_karucode_session() {
    let Some((server, token, model)) = live_config() else {
        panic!("live test needs IMAGE_HANDOFF_LIVE_SERVER and IMAGE_HANDOFF_LIVE_TOKEN");
    };
    let mut settings = json!({"server": server, "api_token": token, "run_timeout_secs": 120});
    if let Some(model) = model {
        settings["model"] = json!(model);
    }
    let png = draw_text_png("KARUCODE 271828");
    let mut plugin = PluginProcess::start_keeping_host_env(&settings);
    plugin.initialize();
    let response = plugin.call(
        2,
        json!({"method": "tools/call", "params": {
            "name": "see",
            "arguments": {"path": png, "question": "Transcribe all text in this image exactly."},
            "context": {"session_id": "s_live", "workspace": "/tmp"}
        }}),
    );
    let (ok, text) = tool_result(&response);
    assert!(ok, "full response: {response}");
    let parsed: Value = serde_json::from_str(&text).expect("tool result is JSON");
    let answer = parsed["answer"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let squashed: String = answer.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        squashed.contains("271828") && squashed.contains("KARUCODE"),
        "vision model did not read the text: {answer}"
    );
    // The handoff must report which karucode model answered and that a session ran.
    assert!(parsed["model"].as_str().is_some());
    assert!(parsed["session_id"].as_str().is_some());
}

fn draw_text_png(text: &str) -> String {
    use std::process::Command as Shell;
    let dir = std::env::temp_dir().join("image-handoff-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("text.png");
    let path_str = path.to_string_lossy().to_string();
    let pil = Shell::new("python3")
        .arg("-c")
        .arg(format!(
            "from PIL import Image, ImageDraw; im = Image.new('RGB', (640, 128), 'white'); d = ImageDraw.Draw(im); d.text((24, 48), {text:?}, fill='black'); im.save({path:?})",
            text = text,
            path = path_str,
        ))
        .status();
    if pil.map(|s| s.success()).unwrap_or(false) {
        return path_str;
    }
    let magick = Shell::new("convert")
        .args([
            "-size",
            "640x128",
            "xc:white",
            "-pointsize",
            "48",
            "-fill",
            "black",
            "-annotate",
            "+24+72",
            text,
            &path_str,
        ])
        .status();
    assert!(
        magick.map(|s| s.success()).unwrap_or(false),
        "need Pillow or ImageMagick to draw the live-test image"
    );
    path_str
}

#[test]
fn plugin_settings_get_overrides_env_settings() {
    let mut plugin = PluginProcess::start_with_rpc(
        &json!({"run_timeout_secs": 30, "max_output_chars": 5000}),
        json!({"run_timeout_secs": 42.0, "max_output_chars": 9000, "model": "od/vision-b"}),
    );
    plugin.initialize();
    let response = plugin.call(
        2,
        json!({"method": "tools/call", "params": {"name": "status", "arguments": {}}}),
    );
    let (ok, text) = tool_result(&response);
    assert!(ok, "{text}");
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["run_timeout_secs"], 42);
    assert_eq!(parsed["max_output_chars"], 9000);
    assert_eq!(parsed["requested_model"], "od/vision-b");
}
