//! Supervised background commands exposed as karucode tools.
#[allow(dead_code)]
mod protocol;
#[allow(dead_code)]
mod sdk;
mod terminal;
use sdk::{permissions, Plugin, PluginBuilder, PluginToolResult, PluginToolResultExt, ToolRequest};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use terminal::TerminalManager;

fn manager() -> &'static TerminalManager {
    static MANAGER: OnceLock<TerminalManager> = OnceLock::new();
    MANAGER.get_or_init(TerminalManager::new)
}
fn owners() -> &'static Mutex<HashMap<String, (String, String)>> {
    static OWNERS: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn scope(req: &ToolRequest) -> Result<(String, String), String> {
    let session = req
        .session_id()
        .filter(|s| !s.is_empty())
        .ok_or("session context is required")?;
    let workspace = req
        .workspace()
        .filter(|s| !s.is_empty())
        .ok_or("workspace context is required")?;
    Ok((session.to_owned(), workspace.to_owned()))
}
fn authorize(req: &ToolRequest, id: &str) -> Result<(), String> {
    let scope = scope(req)?;
    if owners().lock().unwrap().get(id) == Some(&scope) {
        Ok(())
    } else {
        Err(format!("unknown terminal: {id}"))
    }
}
fn result(result: Result<Value, String>) -> PluginToolResult {
    match result {
        Ok(value) => PluginToolResult::ok(value.to_string()).with_meta(value),
        Err(err) => PluginToolResult::err(err),
    }
}
fn required<'a>(req: &'a ToolRequest, key: &str) -> Result<&'a str, String> {
    req.arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("`{key}` must be a non-empty string"))
}
fn optional_u64(req: &ToolRequest, key: &str) -> Result<Option<u64>, String> {
    match req.arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
    }
}
fn cwd(req: &ToolRequest) -> Result<PathBuf, String> {
    let explicit = match req.arguments.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
        Some(_) => return Err("`cwd` must be a non-empty path".into()),
    };
    let workspace = req.workspace().map(Path::new);
    match (explicit, workspace) {
        (Some(path), _) if Path::new(path).is_absolute() => Ok(PathBuf::from(path)),
        (Some(path), Some(root)) => Ok(root.join(path)),
        (Some(_), None) => Err(
            "relative `cwd` requires a workspace in the call context; pass an absolute path".into(),
        ),
        (None, Some(root)) => Ok(root.to_path_buf()),
        (None, None) => Err("workspace context is unavailable; pass an absolute `cwd`".into()),
    }
}
fn start(req: ToolRequest) -> PluginToolResult {
    result((|| {
        let owner = scope(&req)?;
        let value = manager().start(
            required(&req, "command")?,
            &cwd(&req)?,
            optional_u64(&req, "timeout_ms")?,
        )?;
        let id = value["id"]
            .as_str()
            .ok_or("missing terminal id")?
            .to_owned();
        let active = manager().list();
        let mut owners = owners().lock().unwrap();
        owners.retain(|id, _| {
            active["terminals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == *id)
        });
        owners.insert(id, owner);
        Ok(value)
    })())
}
fn read(req: ToolRequest) -> PluginToolResult {
    result((|| {
        let id = required(&req, "id")?;
        authorize(&req, id)?;
        manager().read(id, optional_u64(&req, "cursor")?)
    })())
}
fn write(req: ToolRequest) -> PluginToolResult {
    result((|| {
        let id = required(&req, "id")?;
        authorize(&req, id)?;
        let input = req
            .arguments
            .get("input")
            .and_then(Value::as_str)
            .ok_or_else(|| "`input` must be a string".to_string())?;
        manager().write(id, input)
    })())
}
fn stop(req: ToolRequest) -> PluginToolResult {
    result((|| {
        let id = required(&req, "id")?;
        authorize(&req, id)?;
        manager().stop(id)
    })())
}
fn plugin() -> Plugin {
    PluginBuilder::new("background-terminals", env!("CARGO_PKG_VERSION"))
        .description("Run and manage supervised background commands.")
        .permission(permissions::SHELL)
        .tool_with_class("terminal_start", "Start a background shell command. Returns its id; use terminal_read for output and terminal_stop to stop it. Commands run with the server user's permissions.",
            json!({"type":"object","properties":{
                "command":{"type":"string","description":"Shell command to start."},
                "cwd":{"type":"string","description":"Working directory; defaults to the call's workspace. Relative paths are resolved against it."},
                "timeout_ms":{"type":"integer","minimum":1,"description":"Optional process lifetime limit in milliseconds."}
            },"required":["command"]}), Some("shell"), start)
        .tool_with_class("terminal_read", "Read retained combined stdout/stderr from a terminal. Pass the previous cursor to read new output; truncation is reported.",
            json!({"type":"object","properties":{
                "id":{"type":"string","description":"Terminal id."},
                "cursor":{"type":"integer","minimum":0,"description":"Byte cursor returned by a previous read."}
            },"required":["id"]}), Some("read_only"), read)
        .tool_with_class("terminal_write", "Write input to a running terminal's stdin. Include a newline when needed.",
            json!({"type":"object","properties":{
                "id":{"type":"string","description":"Terminal id."},
                "input":{"type":"string","description":"Text to write to stdin."}
            },"required":["id","input"]}), Some("shell"), write)
        .tool_with_class("terminal_stop", "Stop a background terminal and its process group.",
            json!({"type":"object","properties":{"id":{"type":"string","description":"Terminal id."}},"required":["id"]}), Some("shell"), stop)
        .tool_with_class("terminal_list", "List background terminals and their status.",
            json!({"type":"object","properties":{}}), Some("read_only"),
            |req: ToolRequest| result((|| {
                let owner = scope(&req)?;
                let mut value = manager().list();
                let owners = owners().lock().unwrap();
                value["terminals"].as_array_mut().unwrap().retain(|t| t["id"].as_str().and_then(|id| owners.get(id)) == Some(&owner));
                Ok(value)
            })()))
        .build()
}
fn main() -> std::io::Result<()> {
    use signal_hook::consts::signal::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let handle = signals.handle();
    let signal_thread = std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            manager().shutdown();
            std::process::exit(128 + signal);
        }
    });
    let result = plugin().run();
    manager().shutdown();
    handle.close();
    let _ = signal_thread.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(plugin: &mut Plugin, name: &str, arguments: Value, context: Value) -> Value {
        plugin
            .handle_request(
                json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
                    "name":name, "arguments":arguments, "context":context
                }}),
            )
            .unwrap()["result"]
            .clone()
    }
    #[test]
    fn handshake_and_tool_classes() {
        let mut p = plugin();
        let manifest = p
            .handle_request(json!({"id":1,"method":"initialize"}))
            .unwrap();
        assert_eq!(manifest["result"]["name"], "background-terminals");
        assert_eq!(manifest["result"]["protocol"], "1");
        let listed = p
            .handle_request(json!({"id":2,"method":"tools/list"}))
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        for tool in tools {
            let class = if ["terminal_start", "terminal_write", "terminal_stop"]
                .contains(&tool["name"].as_str().unwrap())
            {
                "shell"
            } else {
                "read_only"
            };
            assert_eq!(tool["class"], class);
        }
    }
    #[test]
    fn validates_inputs_and_workspace() {
        let mut p = plugin();
        assert_eq!(
            call(
                &mut p,
                "terminal_start",
                json!({"command":"true"}),
                Value::Null
            )["ok"],
            false
        );
        assert_eq!(
            call(
                &mut p,
                "terminal_start",
                json!({"command":"true", "timeout_ms":-1}),
                json!({"workspace":"/tmp"})
            )["ok"],
            false
        );
        assert_eq!(
            call(
                &mut p,
                "terminal_read",
                json!({"id":"x", "cursor":"bad"}),
                Value::Null
            )["ok"],
            false
        );
        assert_eq!(
            call(&mut p, "terminal_write", json!({"id":"x"}), Value::Null)["ok"],
            false
        );
    }
}
