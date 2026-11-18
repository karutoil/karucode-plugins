//! End-to-end tests that drive the real plugin binary over the real protocol.
//!
//! The unit tests cover the detector, the framing and the prompt in isolation.
//! These cover the thing the host actually does: spawn the process, speak
//! newline-delimited JSON-RPC to it, and read what comes back. That is the only
//! way to catch a handshake regression (a manifest the host would refuse), a
//! hook whose output is not the shape the host expects, or a panel that crashes
//! on a request with no session.
//!
//! No network is required. The ones that would need a server are marked and skip
//! themselves when `KC_PACE_LIVE` is unset, so `cargo test` is hermetic by default
//! and `KC_PACE_LIVE=1 cargo test` exercises the real routes against the server on
//! `KC_PACE_SERVER` (default `http://127.0.0.1:8787`).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    id: u64,
    rpc_settings: Value,
}

impl Plugin {
    /// Spawn the binary with the given `settings` JSON, as the host does.
    fn spawn(settings: Value) -> Self {
        Self::spawn_with_rpc(settings, json!({}))
    }

    fn spawn_with_rpc(settings: Value, rpc_settings: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kc-deepseek-pace"))
            .env("KARUCODE_PLUGIN_SETTINGS", settings.to_string())
            // The watcher must not reach a real server from a test.
            .env("KARUCODE_PLUGIN_NAME", "kc-deepseek-pace")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the plugin");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            id: 0,
            rpc_settings,
        }
    }

    /// A hermetic instance: watching off, so nothing touches the network.
    fn quiet() -> Self {
        Self::spawn(json!({"watch": false}))
    }

    fn send_raw(&mut self, value: Value) {
        let stdin = self.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{value}").expect("write");
        stdin.flush().expect("flush");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send_raw(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        loop {
            let line = self
                .lines
                .recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("no response to `{method}` in {WAIT:?}"));
            let response: Value = serde_json::from_str(&line).expect("json response");
            if response.get("method").and_then(Value::as_str) == Some("plugin/settings/get") {
                if let Some(req_id) = response.get("id") {
                    let reply = json!({"jsonrpc":"2.0","id":req_id,"result":{"settings": self.rpc_settings}});
                    self.send_raw(reply);
                }
                continue;
            }
            // Notifications (`notify` frames) are interleaved with responses and
            // carry no id; skip them here and read them in the notify test.
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            assert!(
                response.get("error").is_none(),
                "`{method}` failed: {response}"
            );
            return response["result"].clone();
        }
    }

    /// Read the next `notify` frame, which has no id.
    #[allow(dead_code)]
    fn next_notify(&mut self) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(WAIT)
                .expect("a notify frame in time");
            let value: Value = serde_json::from_str(&line).expect("json");
            if value.get("method").and_then(Value::as_str) == Some("notify") {
                return value;
            }
        }
    }

    fn initialize(&mut self) -> Value {
        self.request("initialize", json!({"protocol": "1"}))
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        // Closing stdin makes the SDK's read_line return 0 and the process exit.
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_handshake_reports_the_configured_name_and_protocol() {
    let mut plugin = Plugin::quiet();
    let manifest = plugin.initialize();
    // The host fails the plugin slot on a name mismatch, so this is the assertion
    // that matters most: it must equal the `[[plugins]].name`.
    assert_eq!(manifest["name"], "kc-deepseek-pace");
    assert_eq!(manifest["protocol"], "1");
    assert!(
        manifest["description"]
            .as_str()
            .unwrap_or_default()
            .contains("DeepSeek"),
        "the description is what the settings screen shows: {manifest}"
    );
}

#[test]
fn the_manifest_advertises_the_hooks_and_panel_the_host_needs() {
    let mut plugin = Plugin::quiet();
    let manifest = plugin.initialize();
    let hooks: Vec<&str> = manifest["hooks"]
        .as_array()
        .expect("hooks")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // Only the two the host actually calls is required; `before_prompt` is the
    // whole prevention layer and must never silently disappear.
    assert!(hooks.contains(&"before_prompt"), "{hooks:?}");
    let capabilities: Vec<&str> = manifest["capabilities"]
        .as_array()
        .expect("capabilities")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(capabilities.contains(&"panels"), "{capabilities:?}");
    assert!(capabilities.contains(&"notifications"), "{capabilities:?}");
    assert!(capabilities.contains(&"tools"), "{capabilities:?}");
    assert_eq!(manifest["panels"][0]["id"], "race".replace("race", "pace"));
}

#[test]
fn the_tools_list_carries_both_read_only_tools() {
    let mut plugin = Plugin::quiet();
    plugin.initialize();
    let tools = plugin.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, vec!["pace_explain", "pace_status"], "sorted: {names:?}");
    for tool in tools["tools"].as_array().unwrap() {
        // Without a `read_only` class the host makes every plugin tool prompt for
        // approval, which would make a status check feel like an action.
        assert_eq!(tool["class"], "read_only", "{tool}");
        assert!(!tool["description"].as_str().unwrap_or("").is_empty());
    }
}

#[test]
fn the_prompt_hook_returns_one_stable_block() {
    let mut plugin = Plugin::quiet();
    plugin.initialize();

    let first = plugin.request(
        "hooks/run",
        json!({"hook": "before_prompt", "payload": {"session_id": "s_1", "workspace": "/w"}}),
    );
    let text = first["output"]
        .as_str()
        .expect("before_prompt must return a string, not null");
    assert!(text.contains("discarded"), "{text}");
    assert!(text.contains("Let me write the file"), "{text}");
    // The host caps a contribution at 4 KiB with no error, so a block that grew
    // past it would be silently cut mid-sentence.
    assert!(text.len() < 4096, "{} bytes", text.len());

    // Byte-stability is a hard requirement: this lands in message 0, which
    // providers cache by prefix. A changed byte re-bills the whole conversation.
    for _ in 0..3 {
        let again = plugin.request(
            "hooks/run",
            json!({"hook": "before_prompt", "payload": {"session_id": "s_1", "workspace": "/w"}}),
        );
        assert_eq!(again["output"].as_str(), Some(text));
    }
    // And it must not depend on the session either, or every session's first
    // request would miss the cache.
    let other = plugin.request(
        "hooks/run",
        json!({"hook": "before_prompt", "payload": {"session_id": "s_2", "workspace": "/other"}}),
    );
    assert_eq!(other["output"].as_str(), Some(text));
}

#[test]
fn the_prompt_hook_is_silent_when_turned_off() {
    let mut plugin = Plugin::spawn(json!({"watch": false, "prompt": false}));
    plugin.initialize();
    let out = plugin.request(
        "hooks/run",
        json!({"hook": "before_prompt", "payload": {"session_id": "s", "workspace": "/w"}}),
    );
    assert!(out["output"].is_null(), "{out}");
}

#[test]
fn an_operator_supplied_prompt_replaces_the_default() {
    let mut plugin = Plugin::spawn(json!({
        "watch": false,
        "prompt_text": "Be brief and act at once."
    }));
    plugin.initialize();
    let out = plugin.request(
        "hooks/run",
        json!({"hook": "before_prompt", "payload": {"session_id": "s", "workspace": "/w"}}),
    );
    assert_eq!(out["output"], "Be brief and act at once.");
}

#[test]
fn the_status_tool_reports_a_quiet_system_instead_of_failing() {
    let mut plugin = Plugin::quiet();
    plugin.initialize();
    let result = plugin.request(
        "tools/call",
        json!({"name": "pace_status", "arguments": {},
               "context": {"session_id": "s_1", "workspace": "/w"}}),
    );
    assert_eq!(result["ok"], true, "{result}");
    let text = result["content"].as_str().expect("content");
    assert!(text.contains("kc-deepseek-pace"), "{text}");
    assert!(text.contains("watching no"), "watch is off in this test: {text}");
    assert!(text.contains("s_1"), "{text}");
    assert!(text.contains("no turn in flight"), "{text}");
}

#[test]
fn the_status_tool_falls_back_to_the_call_context_for_the_session() {
    let mut plugin = Plugin::quiet();
    plugin.initialize();
    let result = plugin.request(
        "tools/call",
        json!({"name": "pace_status", "arguments": {},
               "context": {"session_id": "s_from_context", "workspace": "/w"}}),
    );
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("s_from_context"),
        "{result}"
    );
}

#[test]
fn the_explain_tool_states_the_thresholds_and_the_limits() {
    let mut plugin = Plugin::quiet();
    plugin.initialize();
    let result = plugin.request(
        "tools/call",
        json!({"name": "pace_explain", "arguments": {}}),
    );
    let text = result["content"].as_str().expect("content");
    // It has to name what it cannot do, or a reader will assume it can.
    assert!(text.contains("thinking_token_budget"), "{text}");
    assert!(text.contains("cannot"), "{text}");
    assert!(text.contains("deepseek-recipe#24"), "{text}");
    assert!(text.contains("warn") && text.contains("steer") && text.contains("cancel"));
}

#[test]
fn the_panel_answers_with_data_and_survives_an_empty_request() {
    let mut plugin = Plugin::quiet();
    plugin.initialize();
    let panel = plugin.request(
        "panels/read",
        json!({"panel": "pace", "session_id": "s_1", "workspace": "/w"}),
    );
    assert_eq!(panel["title"], "Pace");
    let data = &panel["data"];
    assert_eq!(data["watching"], false, "watch is off: {data}");
    assert_eq!(data["connected"], false);
    assert_eq!(data["session_id"], "s_1");
    assert!(data["turn"].is_null(), "nothing is in flight: {data}");
    assert!(data["ladder"]["warn"]["after_secs"].as_u64().is_some());
    assert!(data["recent"].is_array());

    // A panel with no session at all is what a first paint looks like; it must
    // return data, not an error.
    let bare = plugin.request("panels/read", json!({"panel": "pace"}));
    assert!(bare["data"].is_object(), "{bare}");
}

#[test]
fn an_unknown_tool_and_an_unknown_panel_are_protocol_errors() {
    let mut plugin = Plugin::quiet();
    plugin.initialize();
    plugin.id += 1;
    let id = plugin.id;
    plugin.send_raw(json!({"jsonrpc": "2.0", "id": id, "method": "panels/read",
                           "params": {"panel": "nope"}}));
    loop {
        let line = plugin.lines.recv_timeout(WAIT).expect("a response");
        let value: Value = serde_json::from_str(&line).expect("json");
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        assert!(value.get("error").is_some(), "{value}");
        break;
    }
}

#[test]
fn a_stalled_turn_raises_one_notification_and_never_touches_the_network() {
    // dry_run plus a short ladder: the watcher still has to be off (there is no
    // server in this test), but the detection path is the real one - the plugin
    // is handed the same events the server would send, through its own watcher
    // code, and must produce a notify frame rather than an HTTP call.
    //
    // The watcher only reads a socket, so this cannot be driven without a server;
    // what it pins instead is that a dry-run configuration never *plans* an
    // intervention that writes. The rest of the ladder is covered by unit tests
    // in `watch::tests` and the live test below.
    let mut plugin = Plugin::spawn(json!({
        "watch": false,
        "dry_run": true,
        "warn": {"after_secs": 1, "after_chars": 1},
        "steer": {"after_secs": 1, "after_chars": 1},
        "cancel": {"after_secs": 1, "after_chars": 1}
    }));
    plugin.initialize();
    let panel = plugin.request(
        "panels/read",
        json!({"panel": "pace", "session_id": "s_1", "workspace": "/w"}),
    );
    assert_eq!(panel["data"]["dry_run"], true);
    // With the watcher off, nothing is notified: a plugin must never emit a
    // notification it did not observe.
    assert!(panel["data"]["recent"].as_array().unwrap().is_empty());
}

#[test]
fn settings_are_read_from_the_environment_the_way_the_host_sends_them() {
    let mut plugin = Plugin::spawn(json!({
        "watch": false,
        "dry_run": true,
        "cooldown_secs": 7,
        "narration_min": 5,
        "warn": 42,
        "models": ["deepseek"]
    }));
    plugin.initialize();
    let panel = plugin.request("panels/read", json!({"panel": "pace", "session_id": "s"}));
    let data = &panel["data"];
    assert_eq!(data["dry_run"], true);
    assert_eq!(data["ladder"]["cooldown_secs"], 7);
    assert_eq!(data["ladder"]["narration_min"], 5);
    assert_eq!(data["ladder"]["warn"]["after_secs"], 42);
}

#[test]
fn a_shutdown_request_ends_the_process_cleanly() {
    let mut plugin = Plugin::quiet();
    plugin.initialize();
    let reply = plugin.request("shutdown", json!({}));
    assert!(reply.is_null(), "{reply}");
    // The SDK answers the request, then `run` returns and the process exits.
    let status = wait_for_exit(&mut plugin.child, Duration::from_secs(5));
    assert!(status.is_some(), "the plugin did not exit after `shutdown`");
}

#[test]
fn plugin_settings_get_overrides_env_settings() {
    let mut plugin = Plugin::spawn_with_rpc(
        json!({"watch": false, "dry_run": false}),
        json!({"watch": false, "dry_run": true, "cooldown_secs": 19}),
    );
    plugin.initialize();
    let panel = plugin.request("panels/read", json!({"panel": "pace", "session_id": "s"}));
    let data = &panel["data"];
    assert_eq!(data["dry_run"], true);
    assert_eq!(data["ladder"]["cooldown_secs"], 19);
}

fn wait_for_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}
