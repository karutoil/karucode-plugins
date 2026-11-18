//! karucode plugin: drive and inspect a real Chrome/Chromium over the DevTools
//! Protocol.
//!
//! This is the binary half. It owns one [`BrowserManager`] for the life of the
//! process and exposes it as tools; the library half (`src/lib.rs`) holds the
//! protocol and analysis logic so it can be unit-tested without a browser.
//!
//! Configuration arrives as JSON in `KARUCODE_PLUGIN_SETTINGS` (the `settings`
//! table of the `[[plugins]]` entry) and is parsed by
//! [`BrowserConfig::from_settings`].

use kc_chrome_devtools::browser::{
    BrowserConfig, BrowserManager, DEFAULT_VIEWPORT_HEIGHT, DEFAULT_VIEWPORT_WIDTH, MAX_VIEWPORT,
    MIN_VIEWPORT,
};
use kc_plugin_sdk::{
    hooks, permissions, HostClient, Plugin, PluginBuilder, PluginSetting, PluginSettingKind,
    PluginToolResult, PluginToolResultExt, ToolRequest,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

fn merge_settings(env: Value, rpc: BTreeMap<String, Value>) -> Value {
    let mut map = match env {
        Value::Object(obj) => obj,
        _ => Map::new(),
    };
    for (k, v) in rpc {
        // If config.toml passed `extra_args = [...]` (array), the host's
        // effective_settings drops it against our `text` schema and sends `""`.
        // Preserve the env array unless the operator entered non-empty text.
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
        kc_plugin_sdk::settings()
    } else {
        BTreeMap::new()
    };
    merge_settings(env, rpc)
}

/// The manager is created on first use so a bad `settings` block fails the
/// first tool call with a clear message instead of killing the plugin at
/// startup, where the host would only report "failed to initialize".
fn manager() -> &'static Mutex<BrowserManager> {
    static MANAGER: OnceLock<Mutex<BrowserManager>> = OnceLock::new();
    MANAGER.get_or_init(|| {
        let settings = load_effective_settings();
        Mutex::new(BrowserManager::new(BrowserConfig::from_settings(&settings)))
    })
}

/// Run `f` against the shared manager, turning a poisoned lock into a tool
/// error rather than a panic (a panic would take the whole plugin down).
fn with_manager<T>(f: impl FnOnce(&mut BrowserManager) -> T) -> Result<T, PluginToolResult> {
    let mut guard = manager().lock().map_err(|_| {
        PluginToolResult::err("the browser manager is unusable after an earlier panic")
    })?;
    Ok(f(&mut guard))
}

/// Map a library error onto a failed tool result.
fn fail(context: &str, err: impl std::fmt::Display) -> PluginToolResult {
    PluginToolResult::err(format!("{context}: {err}"))
}

/// `pageId` argument, if the model supplied one.
fn page_arg(req: &ToolRequest) -> Option<i64> {
    req.arguments.get("pageId").and_then(|v| v.as_i64())
}

/// True when the page is missing a `<meta name="viewport">` tag, which is what
/// makes mobile emulation fall back to Chrome's 980px legacy layout viewport.
fn mobile_layout_hint(has_meta: Option<bool>) -> bool {
    // `None` means we could not tell; assume the common case (no tag) so the
    // caller still gets the actionable hint.
    !has_meta.unwrap_or(false)
}

// Tool classes, and why these tools are not `mutating`:
//
// The host maps `mutating` to the `files.write` permission and withholds the
// tool without it. These tools change *browser* state, not files, so claiming
// `mutating` would demand a permission that has nothing to do with what they
// do. `external` is the honest class: it always goes through the approval
// policy, which is the right gate for "drive a real browser", and it needs no
// permission the plugin does not genuinely use.
//
// `read_only` is used only where the tool cannot change page state at all.

fn declared_settings() -> Vec<PluginSetting> {
    vec![
        PluginSetting {
            key: "headless".into(),
            label: "Run browser headless".into(),
            kind: PluginSettingKind::Bool,
            default: json!(true),
            help: "Launch Chrome/Chromium without a visible window.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "isolated".into(),
            label: "Use throwaway browser profile".into(),
            kind: PluginSettingKind::Bool,
            default: json!(true),
            help: "Isolate cookies and storage per plugin process unless user_data_dir is set.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "chrome_path".into(),
            label: "Chrome/Chromium binary path".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional path to the Chrome/Chromium executable (empty = auto-detect).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "user_data_dir".into(),
            label: "Persistent user data directory".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional profile directory; setting this disables isolated mode.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "viewport_width".into(),
            label: "Default viewport width (px)".into(),
            kind: PluginSettingKind::Number,
            default: json!(DEFAULT_VIEWPORT_WIDTH),
            help: "Initial desktop viewport width in CSS pixels.".into(),
            min: Some(MIN_VIEWPORT as f64),
            max: Some(MAX_VIEWPORT as f64),
            options: Vec::new(),
        },
        PluginSetting {
            key: "viewport_height".into(),
            label: "Default viewport height (px)".into(),
            kind: PluginSettingKind::Number,
            default: json!(DEFAULT_VIEWPORT_HEIGHT),
            help: "Initial desktop viewport height in CSS pixels.".into(),
            min: Some(MIN_VIEWPORT as f64),
            max: Some(MAX_VIEWPORT as f64),
            options: Vec::new(),
        },
        PluginSetting {
            key: "protocol_timeout_secs".into(),
            label: "CDP command timeout (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(30),
            help: "How long a single DevTools command may wait before failing.".into(),
            min: Some(1.0),
            max: Some(600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "startup_timeout_secs".into(),
            label: "Browser startup timeout (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(30),
            help: "How long to wait for Chrome's DevTools endpoint after launch.".into(),
            min: Some(1.0),
            max: Some(600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "extra_args".into(),
            label: "Extra Chromium flags".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Space-separated Chromium CLI flags (for example: --no-sandbox).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "snapshot_dir".into(),
            label: "Heap snapshot directory".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional directory for saved V8 heap snapshots (empty = temp dir).".into(),
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
            min: Some(50.0),
            max: Some(100_000.0),
            options: Vec::new(),
        },
    ]
}

fn build_plugin() -> Plugin {
    let mut builder = PluginBuilder::new("chrome-devtools", "0.1.0")
        .description(
            "Drive and inspect a real Chrome/Chromium browser: navigate, read the \
             accessibility tree, click and type by element uid, run script, capture \
             screenshots and console output, and analyse heap snapshots.",
        )
        // The plugin launches a browser process and opens sockets, so it needs
        // both. Without these the host would not expose the tools at all.
        .permission(permissions::SHELL)
        .permission(permissions::NETWORK);
    for setting in declared_settings() {
        builder = builder.setting(setting);
    }
    builder
        .tool_with_class(
            "list_pages",
            "List the open browser pages with their pageId, URL and title. Call this \
             first: every other tool takes an optional pageId and defaults to the \
             active page.",
            json!({"type": "object", "properties": {}}),
            Some("read_only"),
            |_req: ToolRequest| {
                let result = with_manager(|m| {
                    m.ensure_browser()?;
                    m.page_targets()
                });
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not list pages", e),
                    Ok(Ok(targets)) => {
                        if targets.is_empty() {
                            return PluginToolResult::ok(
                                "No pages are open. Use new_page to create one.",
                            );
                        }
                        let lines: Vec<String> = targets
                            .iter()
                            .map(|t| {
                                format!(
                                    "pageId={} url={} title={}",
                                    kc_chrome_devtools::browser::page_id_for(&t.id),
                                    if t.url.is_empty() { "about:blank" } else { &t.url },
                                    if t.title.is_empty() { "(untitled)" } else { &t.title },
                                )
                            })
                            .collect();
                        PluginToolResult::ok(lines.join("\n"))
                    }
                }
            },
        )
        .tool_with_class(
            "new_page",
            "Open a new browser tab at `url` and return its pageId.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "URL to open."},
                    "foreground": {"type": "boolean", "description": "Focus the new tab (default true)."}
                },
                "required": ["url"]
            }),
            Some("external"),
            |req: ToolRequest| {
                let url = req.str_arg("url").trim().to_string();
                if url.is_empty() {
                    return PluginToolResult::err("new_page needs a `url`");
                }
                let foreground = req
                    .arguments
                    .get("foreground")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let result = with_manager(|m| m.create_page(&url, foreground));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not open a page", e),
                    Ok(Ok(target)) => PluginToolResult::ok(format!(
                        "Opened pageId={} url={}",
                        kc_chrome_devtools::browser::page_id_for(&target.id),
                        target.url
                    )),
                }
            },
        )
        .tool_with_class(
            "navigate_page",
            "Navigate a page to `url` and wait for the load event.",
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
                    n => n.max(1_000) as u64,
                };
                let page = page_arg(&req);
                let result = with_manager(|m| m.navigate(page, &url, timeout));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("navigation failed", e),
                    Ok(Ok(url_now)) => PluginToolResult::ok(format!("Navigated to {url_now}")),
                }
            },
        )
        .tool_with_class(
            "take_snapshot",
            "Return the page's accessibility tree as indented text. Interactive \
             elements carry a `uid`; pass that uid to click, fill or hover. Prefer \
             this over reading raw HTML.",
            json!({
                "type": "object",
                "properties": {
                    "pageId": {"type": "integer", "description": "Page to snapshot; defaults to the active page."},
                    "max_nodes": {"type": "integer", "description": "Node cap (default 500)."}
                }
            }),
            Some("read_only"),
            |req: ToolRequest| {
                let max_nodes = match req.int_arg("max_nodes") {
                    0 => 500,
                    n => n.max(1) as usize,
                };
                let page = page_arg(&req);
                let result = with_manager(|m| m.snapshot(page, max_nodes));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not snapshot the page", e),
                    Ok(Ok(snapshot)) => PluginToolResult::ok(snapshot.text)
                        .with_meta(json!({
                            "node_count": snapshot.node_count,
                            "elements": snapshot.elements.len(),
                        })),
                }
            },
        )
        .tool_with_class(
            "resize_page",
            "Set the page's layout viewport, for responsiveness checks and \
             device-sized screenshots. Call with no width/height to clear the \
             override and return to the window's own size.",
            json!({
                "type": "object",
                "properties": {
                    "width": {"type": "integer", "description": "Viewport width in CSS pixels."},
                    "height": {"type": "integer", "description": "Viewport height in CSS pixels."},
                    "pageId": {"type": "integer", "description": "Page to resize; defaults to the active page."},
                    "device_scale_factor": {"type": "number", "description": "Device pixel ratio, e.g. 2 for HiDPI (default 1)."},
                    "mobile": {"type": "boolean", "description": "Use a mobile layout viewport."},
                    "clear": {"type": "boolean", "description": "Remove the override instead of setting one."}
                }
            }),
            Some("external"),
            |req: ToolRequest| {
                let page = page_arg(&req);
                let clear = req
                    .arguments
                    .get("clear")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if clear {
                    let result = with_manager(|m| m.clear_viewport(page));
                    return match result {
                        Err(e) => e,
                        Ok(Err(e)) => fail("could not clear the viewport override", e),
                        Ok(Ok(())) => PluginToolResult::ok(
                            "Cleared the viewport override; the page is back to its window size.",
                        ),
                    };
                }
                let width = req.int_arg("width");
                let height = req.int_arg("height");
                if width <= 0 || height <= 0 {
                    // Report what is in effect so the caller can correct itself.
                    let result = with_manager(|m| m.viewport_size(page));
                    return match result {
                        Err(e) => e,
                        Ok(Err(e)) => fail("could not read the viewport size", e),
                        Ok(Ok((w, h))) => PluginToolResult::err(format!(
                            "resize_page needs a positive `width` and `height`. The viewport is \
                             currently {w}x{h}."
                        )),
                    };
                }
                let dsf = req
                    .arguments
                    .get("device_scale_factor")
                    .and_then(|v| v.as_f64())
                    .filter(|v| *v > 0.0);
                let mobile = req
                    .arguments
                    .get("mobile")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let result =
                    with_manager(|m| m.resize_page(page, width as u32, height as u32, dsf, mobile));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not resize the page", e),
                    Ok(Ok((aw, ah))) => {
                        let asked = (width as u32, height as u32);
                        let note = if (aw, ah) == asked {
                            String::new()
                        } else if mobile
                            && aw != asked.0
                            && mobile_layout_hint(
                                with_manager(|m| {
                                    m.evaluate(
                                        page,
                                        "!!document.querySelector('meta[name=viewport]')",
                                        false,
                                    )
                                })
                                .ok()
                                .and_then(|r| r.ok())
                                .and_then(|v| v.as_bool()),
                            )
                        {
                            format!(
                                "\n\nThe page reports {aw}x{ah}, not the {}x{} requested: it has \
                                 no `<meta name=\"viewport\" content=\"width=device-width\">` tag, \
                                 so mobile emulation uses Chrome's legacy 980px layout viewport. \
                                 Add the meta tag, or call resize_page with `mobile: false` to \
                                 force an exact CSS width.",
                                asked.0, asked.1
                            )
                        } else {
                            format!("\n\nThe page reports {aw}x{ah} rather than the requested size.")
                        };
                        PluginToolResult::ok(format!(
                            "Requested viewport {}x{}.{note}",
                            asked.0, asked.1
                        ))
                        .with_meta(json!({
                            "requested": {"width": asked.0, "height": asked.1},
                            "actual": {"width": aw, "height": ah},
                        }))
                    }
                }
            },
        )
        .tool_with_class(
            "click",
            "Click the element with the given `uid` from take_snapshot.",
            json!({
                "type": "object",
                "properties": {
                    "uid": {"type": "string", "description": "Element uid from take_snapshot."},
                    "pageId": {"type": "integer", "description": "Page containing the element."},
                    "double": {"type": "boolean", "description": "Double-click instead of a single click."}
                },
                "required": ["uid"]
            }),
            Some("external"),
            |req: ToolRequest| {
                let uid = req.str_arg("uid").trim().to_string();
                if uid.is_empty() {
                    return PluginToolResult::err("click needs a `uid` from take_snapshot");
                }
                let double = req
                    .arguments
                    .get("double")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let page = page_arg(&req);
                let result = with_manager(|m| m.click(page, &uid, double));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail(&format!("could not click uid {uid}"), e),
                    Ok(Ok(())) => PluginToolResult::ok(format!("Clicked uid {uid}")),
                }
            },
        )
        .tool_with_class(
            "fill",
            "Set the value of an input, textarea, select or contenteditable element \
             identified by `uid`, firing the events frameworks listen for.",
            json!({
                "type": "object",
                "properties": {
                    "uid": {"type": "string", "description": "Element uid from take_snapshot."},
                    "value": {"type": "string", "description": "Value to set."},
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
                let page = page_arg(&req);
                let result = with_manager(|m| m.fill(page, &uid, &value));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail(&format!("could not fill uid {uid}"), e),
                    Ok(Ok(())) => PluginToolResult::ok(format!("Filled uid {uid}")),
                }
            },
        )
        .tool_with_class(
            "hover",
            "Move the mouse over the element with the given `uid`.",
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
                let page = page_arg(&req);
                let result = with_manager(|m| m.hover(page, &uid));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail(&format!("could not hover uid {uid}"), e),
                    Ok(Ok(())) => PluginToolResult::ok(format!("Hovered uid {uid}")),
                }
            },
        )
        .tool_with_class(
            "evaluate_script",
            "Evaluate a JavaScript expression in the page's main world and return the \
             result. Use `await_promise` for async expressions.",
            json!({
                "type": "object",
                "properties": {
                    "expression": {"type": "string", "description": "JavaScript to evaluate."},
                    "pageId": {"type": "integer", "description": "Page to evaluate in."},
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
                let await_promise = req
                    .arguments
                    .get("await_promise")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let page = page_arg(&req);
                let result = with_manager(|m| m.evaluate(page, &expression, await_promise));
                match result {
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
            "Capture the page as an image and save it to disk. Returns the file path.",
            json!({
                "type": "object",
                "properties": {
                    "pageId": {"type": "integer", "description": "Page to capture."},
                    "format": {"type": "string", "enum": ["png", "jpeg", "webp"], "description": "Image format (default png)."},
                    "full_page": {"type": "boolean", "description": "Capture the whole document, not just the viewport."}
                }
            }),
            Some("read_only"),
            |req: ToolRequest| {
                let format = match req.str_arg("format") {
                    "" => "png".to_string(),
                    other => other.to_ascii_lowercase(),
                };
                let full_page = req
                    .arguments
                    .get("full_page")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let page = page_arg(&req);
                let result = with_manager(|m| {
                    let bytes = m.screenshot(page, &format, full_page)?;
                    let dir = m.screenshot_dir();
                    std::fs::create_dir_all(&dir).map_err(|e| {
                        kc_chrome_devtools::cdp::TransportError::Io(format!(
                            "could not create {}: {e}",
                            dir.display()
                        ))
                    })?;
                    let name = format!(
                        "screenshot-{}.{format}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    );
                    let path = dir.join(name);
                    std::fs::write(&path, &bytes).map_err(|e| {
                        kc_chrome_devtools::cdp::TransportError::Io(format!(
                            "could not write {}: {e}",
                            path.display()
                        ))
                    })?;
                    Ok::<_, kc_chrome_devtools::cdp::TransportError>((path, bytes.len()))
                });
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not capture a screenshot", e),
                    Ok(Ok((path, len))) => {
                        let dims = std::fs::read(&path)
                            .ok()
                            .and_then(|b| BrowserManager::screenshot_size(&b));
                        let size_note = match dims {
                            Some((w, h)) => format!(", {w}x{h} px"),
                            None => String::new(),
                        };
                        PluginToolResult::ok(format!(
                            "Saved a {format} screenshot ({len} bytes{size_note}) to {}",
                            path.display()
                        ))
                        .with_meta(json!({
                            "path": path.display().to_string(),
                            "bytes": len,
                            "width": dims.map(|d| d.0),
                            "height": dims.map(|d| d.1),
                        }))
                    }
                }
            },
        )
        .tool_with_class(
            "get_console",
            "Return console messages and log entries captured since the last call, \
             oldest first. Messages emitted before the plugin attached are not \
             available.",
            json!({
                "type": "object",
                "properties": {
                    "pageId": {"type": "integer", "description": "Page to read the console from."}
                }
            }),
            Some("read_only"),
            |req: ToolRequest| {
                let page = page_arg(&req);
                let result = with_manager(|m| m.take_console(page));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not read the console", e),
                    Ok(Ok(lines)) => {
                        if lines.is_empty() {
                            PluginToolResult::ok("No console messages since the last call.")
                        } else {
                            PluginToolResult::ok(lines.join("\n"))
                                .with_meta(json!({"count": lines.len()}))
                        }
                    }
                }
            },
        )
        .tool_with_class(
            "take_heap_snapshot",
            "Capture a V8 heap snapshot and return a summary of the largest retained \
             object types and constructors. Use it to find memory leaks.",
            json!({
                "type": "object",
                "properties": {
                    "pageId": {"type": "integer", "description": "Page to sample."}
                }
            }),
            Some("read_only"),
            |req: ToolRequest| {
                let page = page_arg(&req);
                let result = with_manager(|m| m.take_heap_snapshot(page));
                match result {
                    Err(e) => e,
                    Ok(Err(e)) => fail("could not take a heap snapshot", e),
                    Ok(Ok((id, path))) => {
                        let summary = with_manager(|m| m.load_snapshot(&id))
                            .ok()
                            .and_then(|r| r.ok())
                            .map(|snap| snap.summary().render(20))
                            .unwrap_or_else(|| "(summary unavailable)".to_string());
                        PluginToolResult::ok(format!(
                            "Heap snapshot `{id}` saved to {}\n\n{summary}",
                            path.display()
                        ))
                        .with_meta(json!({"id": id, "path": path.display().to_string()}))
                    }
                }
            },
        )
        .hook(hooks::BEFORE_PROMPT, |payload| {
            let workspace = payload
                .get("workspace")
                .and_then(|v| v.as_str())
                .unwrap_or("the workspace");
            json!(format!(
                "The chrome-devtools plugin is active in {workspace}. It drives a real \
                 Chrome/Chromium browser. Start with list_pages, then take_snapshot to \
                 get element uids before clicking or filling."
            ))
        })
        .build()
}

fn main() -> std::io::Result<()> {
    build_plugin().run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_settings_pass_host_validation() {
        let plugin = build_plugin();
        let settings = &plugin.manifest().settings;
        assert_eq!(settings.len(), 11);
        kc_plugin_sdk::validate_settings(settings).expect("settings must be valid");
    }

    #[test]
    fn merge_settings_preserves_env_array_extra_args_over_empty_rpc_default() {
        let env = json!({
            "extra_args": ["--no-sandbox"],
            "headless": true
        });
        let mut rpc = BTreeMap::new();
        rpc.insert("extra_args".into(), json!(""));
        rpc.insert("headless".into(), json!(false));
        let merged = merge_settings(env, rpc);
        let cfg = BrowserConfig::from_settings(&merged);
        assert!(!cfg.headless);
        assert_eq!(cfg.extra_args, vec!["--no-sandbox".to_string()]);
    }
}
