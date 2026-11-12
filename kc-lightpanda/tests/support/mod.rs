#![allow(dead_code)]

use kc_lightpanda::browser::BrowserConfig;
use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub fn browser_config() -> BrowserConfig {
    BrowserConfig {
        browser_path: std::env::var("LIGHTPANDA_PATH").ok(),
        startup_timeout_secs: 20,
        protocol_timeout_secs: 20,
        ..Default::default()
    }
}

pub fn data_url(html: &str) -> String {
    // '%' and '#' are significant in data URLs; preserve all markup through
    // those two substitutions without bringing a URL dependency into tests.
    format!(
        "data:text/html,{}",
        html.replace('%', "%25").replace('#', "%23")
    )
}

pub struct PluginProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: Receiver<std::io::Result<String>>,
    next_id: u64,
}

impl PluginProcess {
    pub fn spawn(settings: Option<Value>) -> Self {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kc-lightpanda"));
        command
            .env("KARUCODE_PLUGIN_NAME", "lightpanda")
            .env("KARUCODE_PLUGIN_PROTOCOL", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(settings) = settings {
            command.env("KARUCODE_PLUGIN_SETTINGS", settings.to_string());
        }
        let mut child = command.spawn().expect("spawn kc-lightpanda plugin");
        let stdin = child.stdin.take().expect("plugin stdin");
        let stdout = child.stdout.take().expect("plugin stdout");
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in BufReader::new(stdout).lines() {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            lines: receiver,
            next_id: 1,
        }
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().expect("plugin stdin is open");
        writeln!(stdin, "{message}").expect("write plugin request");
        stdin.flush().expect("flush plugin request");
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(40))
            .unwrap_or_else(|err| panic!("plugin did not answer {method}: {err}"))
            .expect("read plugin response");
        let response: Value = serde_json::from_str(&line).expect("plugin JSON response");
        assert_eq!(
            response.get("id").and_then(|v| v.as_u64()),
            Some(id),
            "{response}"
        );
        if let Some(error) = response.get("error") {
            panic!("{method} returned JSON-RPC error: {error}");
        }
        response.get("result").cloned().unwrap_or(Value::Null)
    }

    pub fn initialize(&mut self) -> Value {
        self.request("initialize", json!({"protocol": "1"}))
    }

    pub fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments, "context": {
                "session_id": "integration-test",
                "workspace": std::env::temp_dir()
            }}),
        )
    }

    pub fn tool_ok(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.tool(name, arguments);
        assert_eq!(
            result.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "{name}: {result}"
        );
        result
    }

    pub fn tool_err(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.tool(name, arguments);
        assert_eq!(
            result.get("ok").and_then(|v| v.as_bool()),
            Some(false),
            "{name}: {result}"
        );
        result
    }

    pub fn text(result: &Value) -> &str {
        result.get("content").and_then(|v| v.as_str()).unwrap_or("")
    }

    /// Close the JSON-RPC input and require orderly process termination.
    pub fn finish_stdin(&mut self) -> bool {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => return false,
            }
        }
    }

    pub fn kill_now(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        self.kill_now();
    }
}

/// True while a Linux process is alive and not a zombie. A killed browser can
/// briefly remain as an adopted zombie while init reaps it.
#[cfg(target_os = "linux")]
pub fn linux_process_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| state != "Z")
}

#[cfg(target_os = "linux")]
pub fn linux_direct_children(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|value| value.parse().ok())
        .collect()
}
