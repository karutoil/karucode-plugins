//! karucode plugin backed by Lightpanda (https://github.com/lightpanda-io/browser).
//!
//! The plugin process owns a lazily-created `lightpanda serve` subprocess. No
//! Chrome/Chromium process, MCP model service or OpenAI key is involved.

#[allow(dead_code)]
mod protocol;
#[allow(dead_code, unused_imports)]
mod sdk;

use crate::sdk::{
    hooks, permissions, HostClient, Plugin, PluginBuilder, PluginSetting, PluginSettingKind,
    PluginToolResult, PluginToolResultExt, ToolRequest,
};
use kc_lightpanda::browser::{
    BrowserConfig, BrowserManager, DEFAULT_VIEWPORT_HEIGHT, DEFAULT_VIEWPORT_WIDTH, MAX_VIEWPORT,
    MIN_VIEWPORT,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

static MANAGER: OnceLock<Mutex<BrowserManager>> = OnceLock::new();

fn merge_settings(env: Value, rpc: BTreeMap<String, Value>) -> Value {
    let mut map = match env {
        Value::Object(obj) => obj,
        _ => Map::new(),
    };
    for (k, v) in rpc {
        if k == "extra_args"
            && v.as_str().is_some_and(|s| s.trim().is_empty())
            && map.get("extra_args").is_some_and(Value::is_array)
        {
            continue;
        }
        map.insert(k, v);
    }
    Value::Object(map)
}

fn load_effective_settings() -> Value {
    let env = std::env::var("KARUCODE_PLUGIN_SETTINGS")
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let rpc = if HostClient::global().is_live() {
        sdk::settings()
    } else {
        BTreeMap::new()
    };
    merge_settings(env, rpc)
}

fn manager() -> &'static Mutex<BrowserManager> {
    MANAGER.get_or_init(|| {
        let settings = load_effective_settings();
        Mutex::new(BrowserManager::new(BrowserConfig::from_settings(&settings)))
    })
}

fn with_manager<T>(f: impl FnOnce(&mut BrowserManager) -> T) -> Result<T, PluginToolResult> {
    let mut guard = manager().lock().map_err(|_| {
        PluginToolResult::err("the browser manager is unusable after an earlier panic")
    })?;
    Ok(f(&mut guard))
}

fn fail(context: &str, err: impl std::fmt::Display) -> PluginToolResult {
    PluginToolResult::err(format!("{context}: {err}"))
}

fn page_arg(req: &ToolRequest) -> Option<i64> {
    req.arguments.get("pageId").and_then(|v| v.as_i64())
}

fn declared_settings() -> Vec<PluginSetting> {
    vec![
        PluginSetting {
            key: "browser_path".into(),
            label: "Lightpanda binary path".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional path to the lightpanda executable (empty = auto-detect on PATH).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "web_socket_url".into(),
            label: "External Lightpanda CDP WebSocket URL".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Attach to an existing ws://host:port/ CDP server instead of spawning lightpanda serve.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "viewport_width".into(),
            label: "Default viewport width (px)".into(),
            kind: PluginSettingKind::Number,
            default: json!(DEFAULT_VIEWPORT_WIDTH),
            help: "Initial viewport width in CSS pixels.".into(),
            min: Some(MIN_VIEWPORT as f64),
            max: Some(MAX_VIEWPORT as f64),
            options: Vec::new(),
        },
        PluginSetting {
            key: "viewport_height".into(),
            label: "Default viewport height (px)".into(),
            kind: PluginSettingKind::Number,
            default: json!(DEFAULT_VIEWPORT_HEIGHT),
            help: "Initial viewport height in CSS pixels.".into(),
            min: Some(MIN_VIEWPORT as f64),
            max: Some(MAX_VIEWPORT as f64),
            options: Vec::new(),
        },
        PluginSetting {
            key: "protocol_timeout_secs".into(),
            label: "CDP command timeout (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(30),
            help: "How long a single CDP command may wait before failing.".into(),
            min: Some(1.0),
            max: Some(600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "startup_timeout_secs".into(),
            label: "Lightpanda startup timeout (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(30),
            help: "How long to wait for lightpanda serve to bind its CDP port.".into(),
            min: Some(1.0),
            max: Some(600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "screenshot_dir".into(),
            label: "Screenshot output directory".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional directory for saved PNG screenshots (empty = temp dir).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "max_snapshot_nodes".into(),
            label: "Max accessibility snapshot nodes".into(),
            kind: PluginSettingKind::Number,
            default: json!(2500),
            help: "Default cap on rendered accessibility tree nodes in take_snapshot.".into(),
            min: Some(1.0),
            max: Some(100_000.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "extra_args".into(),
            label: "Extra Lightpanda CLI flags".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Space-separated extra flags passed to `lightpanda serve`.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "obey_robots".into(),
            label: "Obey robots.txt".into(),
            kind: PluginSettingKind::Bool,
            default: json!(false),
            help: "Respect robots.txt directives when Lightpanda fetches pages.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "telemetry".into(),
            label: "Enable Lightpanda usage telemetry".into(),
            kind: PluginSettingKind::Bool,
            default: json!(false),
            help: "Allow Lightpanda to send usage telemetry (disabled by default).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
    ]
}

fn build_plugin() -> Plugin {
    let mut builder = PluginBuilder::new("lightpanda", "0.1.0")
        .description(
            "Drive Lightpanda over CDP: list and open pages, navigate, inspect the \
             accessibility tree, click/fill/hover by element uid, evaluate JavaScript, \
             capture console output and save text-rendered PNG screenshots.",
        )
        .permission(permissions::SHELL)
        .permission(permissions::NETWORK);
    for setting in declared_settings() {
        builder = builder.setting(setting);
    }
    builder
        .tool_with_class(
            "list_pages",
            "List pages opened by this plugin, with their pageId, URL and title. Call this \
             first: every other tool takes an optional pageId and defaults to the active page.",
            json!({"type": "object", "properties": {}}),
            Some("read_only"),
            |_req: ToolRequest| match with_manager(|m| m.page_targets()) {
                Err(e) => e,
                Ok(Err(e)) => fail("could not list pages", e),
                Ok(Ok(targets)) if targets.is_empty() => {
                    PluginToolResult::ok("No pages are open. Use new_page to create one.")
                }
                Ok(Ok(targets)) => PluginToolResult::ok(
                    targets
                        .iter()
                        .map(|t| {
                            format!(
                                "pageId={} url={} title={}",
                                t.id,
                                if t.url.is_empty() { "about:blank" } else { &t.url },
                                if t.title.is_empty() { "(untitled)" } else { &t.title },
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            },
        )
        .tool_with_class(
            "new_page",
            "Open a new Lightpanda page at `url` and return its pageId.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "URL to open."},
                    "foreground": {"type": "boolean", "description": "Make this the active tab (default true)."}
                },
                "required": ["url"]
            }),
            Some("external"),
            |req: ToolRequest| {
                let url = req.str_arg("url").trim().to_string();
                if url.is_empty() {
                    return PluginToolResult::err("new_page needs a `url`");
                }
                let foreground = req.arguments.get("foreground")
                    .and_then(|v| v.as_bool()).unwrap_or(true);
                match with_manager(|m| m.create_page(&url, foreground)) {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not open a page", e),
                    Ok(Ok(page)) => PluginToolResult::ok(
                        format!("Opened pageId={} url={} title={}", page.id, page.url, page.title),
                    ).with_meta(json!({"pageId": page.id, "url": page.url, "title": page.title})),
                }
            },
        )
        .tool_with_class(
            "navigate_page",
            "Navigate a page to `url` and wait for its load event.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Destination URL."},
                    "pageId": {"type": "integer", "description": "Page to navigate; defaults to the active page."},
                    "timeout_ms": {"type": "integer", "description": "Load timeout in milliseconds (default 30000)."}
                },
                "required": ["url"]
            }),
            Some("external"),
            |req: ToolRequest| {
                let url = req.str_arg("url").trim().to_string();
                if url.is_empty() {
                    return PluginToolResult::err("navigate_page needs a `url`");
                }
                let timeout = match req.int_arg("timeout_ms") {
                    0 => 30_000,
                    n => n.max(1000) as u64,
                };
                match with_manager(|m| m.navigate(page_arg(&req), &url, timeout)) {
                    Err(e) => e,
                    Ok(Err(e)) => fail("navigation failed", e),
                    Ok(Ok(url_now)) => PluginToolResult::ok(format!("Navigated to {url_now}"))
                        .with_meta(json!({"url": url_now})),
                }
            },
        )
        .tool_with_class(
            "take_snapshot",
            "Return the page's accessibility tree as indented text. Interactive \
             elements carry a `uid`; pass that uid to click, fill or hover. Uids \
             belong to the latest snapshot on that page. Prefer this over raw HTML.",
            json!({
                "type": "object",
                "properties": {
                    "pageId": {"type": "integer", "description": "Page to snapshot; defaults to the active page."},
                    "max_nodes": {"type": "integer", "description": "Node cap (default 500, bounded by max_snapshot_nodes)."}
                }
            }),
            Some("read_only"),
            |req: ToolRequest| {
                let requested = match req.int_arg("max_nodes") {
                    0 => 500,
                    n => n.max(1) as usize,
                };
                let result = with_manager(|m| {
                    let cap = requested.min(m.config.max_snapshot_nodes);
                    m.snapshot(page_arg(&req), cap)
                });
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not snapshot the page", e),
                    Ok(Ok(snapshot)) => PluginToolResult::ok(snapshot.text).with_meta(json!({
                        "node_count": snapshot.node_count,
                        "elements": snapshot.elements.len(),
                    })),
                }
            },
        )
        .tool_with_class(
            "resize_page",
            "Set the page's CSS viewport for responsive behavior and text-rendered \
             screenshots. Call with clear=true, or no width/height, to inspect the \
             current size; clear=true also removes the override.",
            json!({
                "type": "object",
                "properties": {
                    "width": {"type": "integer", "description": "Viewport width in CSS pixels."},
                    "height": {"type": "integer", "description": "Viewport height in CSS pixels."},
                    "pageId": {"type": "integer", "description": "Page to resize; defaults to the active page."},
                    "device_scale_factor": {"type": "number", "description": "Device pixel ratio, e.g. 2 for HiDPI (default 1)."},
                    "mobile": {"type": "boolean", "description": "Use a mobile layout viewport (default false)."},
                    "clear": {"type": "boolean", "description": "Remove the override instead of setting one."}
                }
            }),
            Some("external"),
            |req: ToolRequest| {
                let page = page_arg(&req);
                let clear = req.arguments.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
                if clear {
                    return match with_manager(|m| m.clear_viewport(page)) {
                        Err(e) => e,
                        Ok(Err(e)) => fail("could not clear the viewport override", e),
                        Ok(Ok(())) => PluginToolResult::ok("Cleared the viewport override."),
                    };
                }
                let width = req.int_arg("width");
                let height = req.int_arg("height");
                if width <= 0 || height <= 0 {
                    return match with_manager(|m| m.viewport_size(page)) {
                        Err(e) => e,
                        Ok(Err(e)) => fail("could not read the viewport size", e),
                        Ok(Ok((w, h))) => PluginToolResult::err(format!(
                            "resize_page needs positive `width` and `height` values. The viewport is currently {w}x{h}."
                        )),
                    };
                }
                let dsf = req.arguments.get("device_scale_factor").and_then(|v| v.as_f64())
                    .filter(|v| v.is_finite() && *v > 0.0 && *v <= 10.0);
                let mobile = req.arguments.get("mobile").and_then(|v| v.as_bool()).unwrap_or(false);
                match with_manager(|m| m.resize_page(page, width as u32, height as u32, dsf, mobile)) {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not resize the page", e),
                    Ok(Ok((actual_w, actual_h))) => PluginToolResult::ok(format!(
                        "Requested viewport {width}x{height}; the page reports {actual_w}x{actual_h}."
                    )).with_meta(json!({
                        "requested": {"width": width, "height": height},
                        "actual": {"width": actual_w, "height": actual_h}
                    })),
                }
            },
        )
        .tool_with_class(
            "click",
            "Click an element identified by `uid` from take_snapshot.",
            json!({
                "type": "object",
                "properties": {
                    "uid": {"type": "string", "description": "Element uid from take_snapshot."},
                    "pageId": {"type": "integer", "description": "Page containing the element."},
                    "double": {"type": "boolean", "description": "Double-click instead of a single click (default false)."}
                },
                "required": ["uid"]
            }),
            Some("external"),
            |req: ToolRequest| {
                let uid = req.str_arg("uid").trim().to_string();
                if uid.is_empty() {
                    return PluginToolResult::err("click needs a `uid` from take_snapshot");
                }
                let double = req.arguments.get("double").and_then(|v| v.as_bool()).unwrap_or(false);
                match with_manager(|m| m.click(page_arg(&req), &uid, double)) {
                    Err(e) => e,
                    Ok(Err(e)) => fail(&format!("could not click uid {uid}"), e),
                    Ok(Ok(())) => PluginToolResult::ok(format!("Clicked uid {uid}")),
                }
            },
        )
        .tool_with_class(
            "fill",
            "Fill an input, textarea, select, checkbox/radio, or contenteditable element \
             identified by `uid`, firing input and change events where applicable.",
            json!({
                "type": "object",
                "properties": {
                    "uid": {"type": "string", "description": "Element uid from take_snapshot."},
                    "value": {"type": "string", "description": "Value to set (true/false for checkbox/radio)."},
                    "pageId": {"type": "integer", "description": "Page containing the element."}
                },
                "required": ["uid", "value"]
            }),
            Some("external"),
            |req: ToolRequest| {
                let uid = req.str_arg("uid").trim().to_string();
                if uid.is_empty() {
                    return PluginToolResult::err("fill needs a `uid` from take_snapshot");
                }
                let value = req.str_arg("value").to_string();
                match with_manager(|m| m.fill(page_arg(&req), &uid, &value)) {
                    Err(e) => e,
                    Ok(Err(e)) => fail(&format!("could not fill uid {uid}"), e),
                    Ok(Ok(())) => PluginToolResult::ok(format!("Filled uid {uid}")),
                }
            },
        )
        .tool_with_class(
            "hover",
            "Dispatch mouse hover events on the element identified by `uid`.",
            json!({
                "type": "object",
                "properties": {
                    "uid": {"type": "string", "description": "Element uid from take_snapshot."},
                    "pageId": {"type": "integer", "description": "Page containing the element."}
                },
                "required": ["uid"]
            }),
            Some("external"),
            |req: ToolRequest| {
                let uid = req.str_arg("uid").trim().to_string();
                if uid.is_empty() {
                    return PluginToolResult::err("hover needs a `uid` from take_snapshot");
                }
                match with_manager(|m| m.hover(page_arg(&req), &uid)) {
                    Err(e) => e,
                    Ok(Err(e)) => fail(&format!("could not hover uid {uid}"), e),
                    Ok(Ok(())) => PluginToolResult::ok(format!("Hovered uid {uid}")),
                }
            },
        )
        .tool_with_class(
            "evaluate_script",
            "Evaluate a JavaScript expression in the page's main world and return its \
             result. Use await_promise=true for asynchronous expressions.",
            json!({
                "type": "object",
                "properties": {
                    "expression": {"type": "string", "description": "JavaScript expression to evaluate."},
                    "pageId": {"type": "integer", "description": "Page to evaluate in; defaults to the active page."},
                    "await_promise": {"type": "boolean", "description": "Await a returned promise (default false)."}
                },
                "required": ["expression"]
            }),
            Some("external"),
            |req: ToolRequest| {
                let expression = req.str_arg("expression").to_string();
                if expression.trim().is_empty() {
                    return PluginToolResult::err("evaluate_script needs an `expression`");
                }
                let await_promise = req.arguments.get("await_promise")
                    .and_then(|v| v.as_bool()).unwrap_or(false);
                match with_manager(|m| m.evaluate(page_arg(&req), &expression, await_promise)) {
                    Err(e) => e,
                    Ok(Err(e)) => fail("the script threw", e),
                    Ok(Ok(value)) => {
                        let rendered = match &value {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        PluginToolResult::ok(rendered).with_meta(json!({"value": value}))
                    }
                }
            },
        )
        .tool_with_class(
            "take_screenshot",
            "Save Lightpanda's text-rendered representation of the page as a PNG and \
             return the path. This is not a Chromium-style pixel screenshot: Lightpanda \
             flows the page's text into blocks because it has no graphical layout engine.",
            json!({
                "type": "object",
                "properties": {
                    "pageId": {"type": "integer", "description": "Page to capture; defaults to the active page."},
                    "format": {"type": "string", "enum": ["png"], "description": "Image format (only png is supported)."},
                    "full_page": {"type": "boolean", "description": "Capture beyond the viewport to include the full text document (default false)."}
                }
            }),
            // Capturing is page-read-only but this handler creates a file, so
            // retain the normal external approval gate rather than claiming the
            // whole operation is filesystem-read-only.
            Some("external"),
            |req: ToolRequest| {
                let format = match req.str_arg("format") {
                    "" => "png".to_string(),
                    other => other.to_ascii_lowercase(),
                };
                if format != "png" {
                    return PluginToolResult::err("Lightpanda take_screenshot supports only format=png");
                }
                let full_page = req.arguments.get("full_page").and_then(|v| v.as_bool()).unwrap_or(false);
                let result = with_manager(|m| {
                    let bytes = m.screenshot(page_arg(&req), &format, full_page)?;
                    let dir = m.screenshot_dir();
                    std::fs::create_dir_all(&dir).map_err(|e| {
                        kc_lightpanda::cdp::TransportError::Io(format!("could not create {}: {e}", dir.display()))
                    })?;
                    // Nanoseconds plus pid avoid replacing another plugin's image;
                    // create_new closes the tiny same-timestamp collision race.
                    for attempt in 0..10u8 {
                        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos()).unwrap_or(0);
                        let path = dir.join(format!("lightpanda-{stamp}-{}-{attempt}.png", std::process::id()));
                        let mut options = std::fs::OpenOptions::new();
                        options.write(true).create_new(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt as _;
                            options.mode(0o600);
                        }
                        match options.open(&path) {
                            Ok(mut file) => {
                                use std::io::Write as _;
                                if let Err(err) = file.write_all(&bytes) {
                                    drop(file);
                                    let _ = std::fs::remove_file(&path);
                                    return Err(kc_lightpanda::cdp::TransportError::Io(
                                        format!("could not write {}: {err}", path.display()),
                                    ));
                                }
                                return Ok((path, bytes));
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                            Err(e) => return Err(kc_lightpanda::cdp::TransportError::Io(
                                format!("could not create the screenshot file: {e}"))),
                        }
                    }
                    Err(kc_lightpanda::cdp::TransportError::Io("could not allocate a unique screenshot path".into()))
                });
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not capture a text-rendered screenshot", e),
                    Ok(Ok((path, bytes))) => {
                        let dims = BrowserManager::screenshot_size(&bytes);
                        let size_note = dims.map(|(w, h)| format!(", {w}x{h} px")).unwrap_or_default();
                        PluginToolResult::ok(format!(
                            "Saved Lightpanda's text-rendered PNG ({} bytes{size_note}) to {}",
                            bytes.len(), path.display()
                        )).with_meta(json!({
                            "path": path.display().to_string(),
                            "bytes": bytes.len(),
                            "width": dims.map(|d| d.0),
                            "height": dims.map(|d| d.1),
                            "renderer": "text-only"
                        }))
                    }
                }
            },
        )
        .tool_with_class(
            "get_console",
            "Return console and log entries captured since the last call, oldest first. \
             Messages emitted before this plugin attached are not available.",
            json!({
                "type": "object",
                "properties": {
                    "pageId": {"type": "integer", "description": "Page to read; defaults to the active page."}
                }
            }),
            Some("read_only"),
            |req: ToolRequest| match with_manager(|m| m.take_console(page_arg(&req))) {
                Err(e) => e,
                Ok(Err(e)) => fail("could not read the console", e),
                Ok(Ok(lines)) if lines.is_empty() => {
                    PluginToolResult::ok("No console messages since the last call.")
                }
                Ok(Ok(lines)) => PluginToolResult::ok(lines.join("\n")).with_meta(json!({"count": lines.len()})),
            },
        )
        .hook(hooks::BEFORE_PROMPT, |payload| {
            let workspace = payload.get("workspace").and_then(|v| v.as_str()).unwrap_or("the workspace");
            json!(format!(
                "The lightpanda plugin is active in {workspace}. It drives Lightpanda directly \
                 over CDP, not Chrome. Start with list_pages, then take_snapshot to get element \
                 uids before clicking or filling. Screenshots are PNGs of Lightpanda's text-only \
                 page rendering; it is not a pixel-identical graphical browser."
            ))
        })
        .build()
}

fn main() -> std::io::Result<()> {
    let run_result = build_plugin().run();

    // The process-lifetime OnceLock intentionally outlives ordinary Rust stack
    // drop order. Explicitly release pages and the owned browser on stdin EOF or
    // the SDK's shutdown notification, including non-Linux graceful shutdown.
    if let Some(lock) = MANAGER.get() {
        match lock.lock() {
            Ok(mut manager) => manager.shutdown(),
            Err(_) => eprintln!("kc-lightpanda: could not lock the manager during shutdown"),
        }
    }
    run_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_settings_pass_host_validation() {
        let plugin = build_plugin();
        let settings = &plugin.manifest().settings;
        assert_eq!(settings.len(), 11);
        sdk::validate_settings(settings).expect("declared settings must be valid");
    }

    #[test]
    fn merge_settings_preserves_env_array_extra_args() {
        let env = json!({"extra_args": ["--foo"]});
        let mut rpc = BTreeMap::new();
        rpc.insert("extra_args".into(), json!(""));
        rpc.insert("obey_robots".into(), json!(true));
        let merged = merge_settings(env, rpc);
        let cfg = BrowserConfig::from_settings(&merged);
        assert!(cfg.obey_robots);
        assert_eq!(cfg.extra_args, vec!["--foo".to_string()]);
    }
}
