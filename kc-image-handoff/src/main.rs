//! `image-handoff`: let a text-only model see images, **entirely through
//! karucode**.
//!
//! The agent calls `plugin__image-handoff__see` with an image (path or URL)
//! and the question it wants answered. The plugin uploads the image to a
//! karucode session it creates for this one call - pinned to a vision model
//! from karucode's own provider configuration - lets karucode's runtime run
//! the model, reads the answer from the event stream, and returns it. The
//! session is a **temporary (`ephemeral`) session** on server builds that
//! have them, so it never appears in any list and retires itself when its
//! run ends; older builds get a durable session that this plugin deletes.
//!
//! No provider endpoint, model id or API key is ever handled here: karucode's
//! runtime is the only thing that talks to a model, and the only credential
//! the plugin holds is the server token the operator gave it.

#[allow(dead_code)]
mod protocol;
#[allow(dead_code, unused_imports)]
mod sdk;

mod answer_bus;
mod config;
mod flow;
mod karucode;
mod media;
mod server;
mod session_vision;

use config::Settings;
use media::Image;
use sdk::{
    hooks, HostClient, PluginBuilder, PluginSetting, PluginSettingKind, PluginToolResult,
    PluginToolResultExt, ToolRequest,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

fn settings() -> &'static Settings {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    SETTINGS.get_or_init(|| {
        let raw = std::env::var("KARUCODE_PLUGIN_SETTINGS").ok();
        let env = raw
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .and_then(|r| match serde_json::from_str::<Value>(r) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!(
                        "[image-handoff] plugin settings are not valid JSON ({e}); using defaults"
                    );
                    None
                }
            })
            .unwrap_or(Value::Null);
        let rpc = if HostClient::global().is_live() {
            sdk::settings()
        } else {
            BTreeMap::new()
        };
        let merged = Settings::merge_env_and_rpc(env, rpc);
        match serde_json::from_value::<Settings>(merged) {
            Ok(s) => s.normalized(),
            Err(e) => {
                eprintln!("[image-handoff] plugin settings are invalid ({e}); using defaults");
                Settings::default()
            }
        }
    })
}

/// Last few `see` calls, for the status tool and panel.
#[derive(Debug, Clone, serde::Serialize)]
struct CallLog {
    question: String,
    source: String,
    model: String,
    ok: bool,
    chars: usize,
    elapsed_ms: u64,
    at: u64,
}

fn calls() -> &'static Mutex<Vec<CallLog>> {
    static CALLS: OnceLock<Mutex<Vec<CallLog>>> = OnceLock::new();
    CALLS.get_or_init(|| Mutex::new(Vec::new()))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn record_call(log: CallLog) {
    if let Ok(mut calls) = calls().lock() {
        calls.push(log);
        let len = calls.len();
        calls.drain(..len.saturating_sub(20));
    }
}

fn require_question(question: &str) -> Result<(), String> {
    if question.trim().is_empty() {
        return Err("`question` is required: what do you want to know about the image?".into());
    }
    if question.chars().count() > config::DEFAULT_MAX_QUESTION_CHARS {
        return Err(format!(
            "`question` must be at most {} characters",
            config::DEFAULT_MAX_QUESTION_CHARS
        ));
    }
    Ok(())
}

fn see(req: ToolRequest) -> PluginToolResult {
    // A session_id means: collect the answer of a pending run, not start one;
    // a collect call carries no question.
    let collect_id = req.str_arg("session_id").trim().to_string();
    if !collect_id.is_empty() {
        return collect_answer(req);
    }
    let question = req.str_arg("question").trim().to_string();
    if let Err(error) = require_question(&question) {
        return PluginToolResult::err(error);
    }
    let settings = settings();

    // The handoff is for text-only models. A session whose own model can see
    // images gets a pointer to `read_file` instead - ferrying its image to
    // another model would spend a session and lose the pixels' fidelity.
    let token = settings.resolve_token().unwrap_or_default();
    if let (Some(endpoint), Some(session_id)) = (server_endpoint(settings).ok(), req.session_id()) {
        if session_vision::session_vision(&endpoint, &token, session_id)
            == session_vision::SessionVision::Vision
        {
            return PluginToolResult::err(
                "this session's model can view images directly: call `read_file` with the \
                 image's path and the pixels are attached to your context natively - no \
                 handoff needed. (`plugin__image-handoff__see` only serves text-only \
                 sessions.)",
            );
        }
    }

    // The vision model: per-call override, then settings, then karucode's own
    // configuration (active model if vision, else the first vision model).
    let requested = req.str_arg("model").trim().to_string();
    let resolved = match resolve_vision_model(settings, &requested) {
        Ok(resolved) => resolved,
        Err(error) => return PluginToolResult::err(error),
    };

    // The image bytes.
    let url = req.str_arg("url").trim().to_string();
    let path = req.str_arg("path").trim().to_string();
    if url.is_empty() && path.is_empty() {
        return PluginToolResult::err(
            "give either `path` (workspace-relative or absolute) or `url` (http/https) - one of them is required",
        );
    }
    if !url.is_empty() && !path.is_empty() {
        return PluginToolResult::err("give either `path` or `url`, not both");
    }
    let max_bytes = settings.max_image_mb * 1024 * 1024;
    let image = if !url.is_empty() {
        match Image::from_url(
            &url,
            std::time::Duration::from_secs(settings.run_timeout_secs.min(30)),
            max_bytes,
        ) {
            Ok(image) => image,
            Err(error) => return PluginToolResult::err(error),
        }
    } else {
        match Image::from_path(&path, req.workspace(), max_bytes) {
            Ok(image) => image,
            Err(error) => return PluginToolResult::err(error),
        }
    };

    // Everything from here on is karucode: session, upload, run, answer.
    let token = match settings.resolve_token() {
        Ok(token) => token,
        Err(error) => return PluginToolResult::err(error),
    };
    let handoff = flow::HandoffConfig {
        endpoint: match server_endpoint(settings) {
            Ok(endpoint) => endpoint,
            Err(error) => return PluginToolResult::err(error),
        },
        token,
        run_timeout: std::time::Duration::from_secs(settings.run_timeout_secs),
    };
    let name = image
        .source
        .rsplit('/')
        .next()
        .unwrap_or("image.png")
        .to_string();
    let question_with_contract = format!(
        "{question}\n\nRead the attached image file first, then answer. You are answering for \
         another AI that cannot see images: report exactly what is visible - text transcribed \
         verbatim, colors, layouts, shapes, counts - and say explicitly what is not visible or \
         not legible. Do not speculate beyond the image."
    );
    let result = flow::see(
        &handoff,
        &resolved,
        &question_with_contract,
        &name,
        &image.data,
        image.mime,
    );
    finish_tool_result(
        settings,
        &image.source,
        question,
        resolved.model.clone(),
        result,
    )
}

/// Answer a `see` call for a run a previous call started.
fn collect_answer(req: ToolRequest) -> PluginToolResult {
    let session_id = req.str_arg("session_id").trim().to_string();
    if session_id.is_empty() {
        return PluginToolResult::err(
            "`session_id` is required: the id a pending `see` call returned",
        );
    }
    let settings = settings();
    let token = match settings.resolve_token() {
        Ok(token) => token,
        Err(error) => return PluginToolResult::err(error),
    };
    let handoff = flow::HandoffConfig {
        endpoint: match server_endpoint(settings) {
            Ok(endpoint) => endpoint,
            Err(error) => return PluginToolResult::err(error),
        },
        token,
        run_timeout: std::time::Duration::from_secs(settings.run_timeout_secs),
    };
    let result = flow::collect(&handoff, &session_id, Instant::now());
    finish_tool_result(
        settings,
        &session_id,
        session_id.clone(),
        String::new(),
        result,
    )
}

use std::time::Instant;

fn finish_tool_result(
    settings: &Settings,
    source: &str,
    question: String,
    model: String,
    result: Result<flow::HandoffResult, String>,
) -> PluginToolResult {
    match result {
        Ok(flow::HandoffResult::Done { answer, report }) => {
            let answer = truncate_chars(&answer, settings.max_output_chars);
            record_call(CallLog {
                question,
                source: source.to_string(),
                model: if report.model.is_empty() {
                    model
                } else {
                    report.model.clone()
                },
                ok: true,
                chars: answer.chars().count(),
                elapsed_ms: report.elapsed_ms,
                at: unix_now(),
            });
            let payload = json!({
                "answer": answer,
                "model": report.model,
                "provider": report.provider,
                "session_id": report.session_id,
                "ephemeral_session": report.ephemeral,
                "image_path": report.image_path,
                "image_bytes": report.image_bytes,
                "run_status": report.status,
                "elapsed_ms": report.elapsed_ms,
            });
            PluginToolResult::ok(
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
            )
        }
        Ok(flow::HandoffResult::Pending { session_id, report }) => {
            // The run outlived this tool call's budget. The frames keep
            // arriving in the plugin's own reader, so the caller can collect.
            let payload = json!({
                "answer": null,
                "pending": true,
                "session_id": session_id,
                "run_status": report.status,
                "elapsed_ms": report.elapsed_ms,
                "collect": "call `plugin__image-handoff__see` again with `session_id` to \
                            collect the answer; the vision run is still going",
            });
            PluginToolResult::ok(
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
            )
        }
        Err(error) => {
            record_call(CallLog {
                question,
                source: source.to_string(),
                model,
                ok: false,
                chars: 0,
                elapsed_ms: 0,
                at: unix_now(),
            });
            PluginToolResult::err(error)
        }
    }
}

/// Resolve the vision model, preferring the per-call `model` argument, then
/// the plugin settings, then karucode's own provider configuration.
fn resolve_vision_model(settings: &Settings, per_call: &str) -> Result<karucode::Resolved, String> {
    let per_call = per_call.trim();
    if !per_call.is_empty() {
        // Route the per-call id through karucode's provider configuration so
        // the session pin is always a valid {provider, model}.
        return karucode::resolve(None, Some(per_call));
    }
    if let Some((provider, model)) = settings.requested_model() {
        return karucode::resolve_provider_model(&provider, &model);
    }
    karucode::resolve(None, None)
}

fn server_endpoint(settings: &Settings) -> Result<crate::server::Endpoint, String> {
    crate::server::Endpoint::parse(&settings.server)
        .map_err(|e| format!("the `server` setting is not usable: {e}"))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = text.to_string();
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect();
        out.push_str("\n[… truncated]");
    }
    out
}

fn status(_req: ToolRequest) -> PluginToolResult {
    let settings = settings();
    let token = settings.resolve_token().unwrap_or_default();
    // Readiness is: the server is reachable and a vision model resolves.
    let mut problem = None;
    let endpoint = server_endpoint(settings).ok();
    if let Some(endpoint) = &endpoint {
        match crate::server::request_json(endpoint, &token, "GET", "/api/health", None) {
            Ok((status, _)) if (200..300).contains(&status) => {}
            Ok((status, _)) => {
                problem = Some(format!("the karucode server answered HTTP {status}"));
            }
            Err(e) => problem = Some(format!("cannot reach the karucode server: {e}")),
        }
    } else {
        problem = Some("the `server` setting is not an http:// URL".into());
    }
    let resolved = match &problem {
        None => resolve_vision_model(settings, "").ok(),
        Some(_) => None,
    };
    if problem.is_none() {
        if let Err(error) = resolve_vision_model(settings, "") {
            problem = Some(error);
        }
    }
    let mut value = settings.public_json(resolved.as_ref(), problem);
    if let Ok(list) = calls().lock() {
        value["recent_calls"] = json!(list.iter().rev().take(5).collect::<Vec<_>>());
    }
    PluginToolResult::ok(serde_json::to_string_pretty(&value).unwrap_or_default())
}

/// The note exists for **text-only** models: it tells a session that cannot
/// see images that the handoff tool gives it eyes. A session whose own model
/// already has `vision = true` gets **nothing** - its `read_file` already
/// attaches pixels, and being told it cannot see would be wrong and wasteful.
///
/// Cache-stable for a given session: the host re-asks on every model call and
/// a changing answer would re-bill the context, so the session-vision lookup
/// is cached per session id (60s).
fn before_prompt(payload: Value) -> Value {
    let settings = settings();
    let token = settings.resolve_token().unwrap_or_default();
    let endpoint = server_endpoint(settings).ok();
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(endpoint) = endpoint {
        if session_vision::session_vision(&endpoint, &token, session_id)
            == session_vision::SessionVision::Vision
        {
            // This session's model can see: no note, `see` refuses politely.
            return Value::Null;
        }
    }
    json!(
        "You cannot view images directly. When a task needs image content - reading a \
         screenshot, a diagram, a photo, OCR, a UI mock - call the tool \
         `plugin__image-handoff__see` with the image's `path` (workspace-relative or \
         absolute) or `url`, and the specific `question` you need answered. The image is \
         handed to one of karucode's own vision models through a temporary session and \
         that model's answer comes back as the tool result; treat it as another model's \
         report of the pixels, not as the pixels themselves. Call \
         `plugin__image-handoff__status` if the tool reports a configuration problem."
    )
}

fn declared_settings() -> Vec<PluginSetting> {
    let defaults = Settings::default();
    vec![
        PluginSetting {
            key: "server".into(),
            label: "karucode server URL".into(),
            kind: PluginSettingKind::Text,
            default: json!(defaults.server),
            help: "Loopback HTTP URL of the karucode server that spawned this plugin.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "api_token".into(),
            label: "Server bearer token".into(),
            kind: PluginSettingKind::Secret,
            default: json!(""),
            help: "Bearer token when the karucode server runs with KARUCODE_AUTH_TOKEN.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "api_token_env".into(),
            label: "Token environment variable name".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional env var holding the token (empty = IMAGE_HANDOFF_TOKEN).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "model".into(),
            label: "Vision model override".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional karucode model id to pin for image reads (empty = auto-select).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "provider".into(),
            label: "Vision provider override".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional provider profile id paired with `model` (empty = auto-select).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "run_timeout_secs".into(),
            label: "Vision handoff timeout (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.run_timeout_secs),
            help: "End-to-end deadline for one vision handoff (keep under 55s for 60s tool cap)."
                .into(),
            min: Some(1.0),
            max: Some(600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "max_image_mb".into(),
            label: "Max image size (MiB)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.max_image_mb),
            help: "Maximum accepted image file size in MiB (1..8 MiB upload ceiling).".into(),
            min: Some(1.0),
            max: Some(8.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "max_output_chars".into(),
            label: "Max vision answer length (chars)".into(),
            kind: PluginSettingKind::Number,
            default: json!(defaults.max_output_chars),
            help: "Maximum characters returned from the vision model response.".into(),
            min: Some(1000.0),
            max: Some(20_000.0),
            options: Vec::new(),
        },
    ]
}

fn build_plugin() -> sdk::Plugin {
    let mut builder = PluginBuilder::new("image-handoff", env!("CARGO_PKG_VERSION"))
        .description("Hand images to one of karucode's vision models through a temporary session")
        .permission(sdk::permissions::FILES_READ)
        .permission(sdk::permissions::NETWORK);
    for setting in declared_settings() {
        builder = builder.setting(setting);
    }
    builder
        .tool_with_class(
            "see",
            "Read an image (a PNG, JPEG, GIF, WebP or BMP) with a vision model and return \
             what it sees. Use it whenever image content matters: reading text in a \
             screenshot or photo, describing a diagram, UI, chart or layout, identifying what \
             is depicted. Give `path` (workspace-relative or absolute file) or `url` \
             (http/https), plus a specific `question` about the image - the answer you get \
             back is the vision model's literal report of the pixels. The vision model is one \
             of karucode's own, run in a temporary karucode session (resolved from its \
             provider configuration: the session pin if it has vision, else the active model, \
             else the first vision model); pass `model` to name a different karucode model \
             for this call. For several images, call once per image.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Image file path, workspace-relative or absolute."},
                    "url": {"type": "string", "description": "Image URL, http:// or https://."},
                    "question": {"type": "string", "description": "What you want to know about this image. Be specific: the text to transcribe, the region to describe, the thing to count."},
                    "model": {"type": "string", "description": "Optional: a model id from karucode's provider configuration to use for this call instead of the resolved default."},
                    "session_id": {"type": "string", "description": "Collect the answer of a pending call: pass the session_id a pending result returned. The vision run continues in the background between the two calls."}
                },
                "required": ["question"]
            }),
            Some("read_only"),
            see,
        )
        .tool_with_class(
            "status",
            "Show the image-handoff plugin's configuration: the karucode server it drives, \
             the vision model karucode's own provider configuration resolved, and the last \
             few calls. Read-only; use it when `see` reports a configuration problem.",
            json!({
                "type": "object",
                "properties": {}
            }),
            Some("read_only"),
            status,
        )
        .hook(hooks::BEFORE_PROMPT, before_prompt)
        .panel_with("status", "Image handoff", Some("eye"), 5_000, panel)
        .build()
}

fn panel(_req: sdk::PanelRequest) -> Value {
    let settings = settings();
    let stat = match resolve_vision_model(settings, "") {
        Ok(endpoint) => json!({
            "view": "stat",
            "label": "Vision model (karucode)",
            "value": endpoint.model,
            "detail": format!(
                "provider {} · via temporary karucode sessions at {}",
                endpoint.provider_id(),
                settings.server
            ),
            "tone": "ok"
        }),
        Err(problem) => json!({
            "view": "stat",
            "label": "Vision model (karucode)",
            "value": "not ready",
            "detail": problem,
            "tone": "warn"
        }),
    };
    let mut views = vec![stat];
    if let Ok(list) = calls().lock() {
        for call in list.iter().rev().take(8) {
            views.push(json!({
                "view": "list",
                "title": call.question,
                "items": [{
                    "text": call.source,
                    "detail": format!(
                        "{} · {} chars · {}ms · {}",
                        call.model,
                        call.chars,
                        call.elapsed_ms,
                        if call.ok { "ok" } else { "failed" }
                    )
                }]
            }));
        }
    }
    json!({"view": "stack", "views": views})
}

fn main() -> std::io::Result<()> {
    build_plugin().run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_note_shape_is_bounded() {
        // The note text itself (what a text-only session sees).
        let note = json!(
            "You cannot view images directly. When a task needs image content - reading a \
             screenshot, a diagram, a photo, OCR, a UI mock - call the tool \
             `plugin__image-handoff__see` with the image's `path` (workspace-relative or \
             absolute) or `url`, and the specific `question` you need answered. The image is \
             handed to one of karucode's own vision models through a temporary session and \
             that model's answer comes back as the tool result; treat it as another model's \
             report of the pixels, not as the pixels themselves. Call \
             `plugin__image-handoff__status` if the tool reports a configuration problem."
        );
        let text = note.as_str().unwrap();
        assert!(text.contains("plugin__image-handoff__see"));
        assert!(text.len() < 4_096);
    }

    #[test]
    fn tool_surface_is_stable_and_bounded() {
        let plugin = build_plugin();
        assert_eq!(plugin.manifest().name, "image-handoff");
        assert_eq!(plugin.manifest().protocol, "1");
        let listed = plugin
            .handle_request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["see", "status"]);
        for tool in tools {
            assert!(tool["description"].as_str().unwrap().len() < 1_000);
        }
        let see = tools.iter().find(|t| t["name"] == "see").unwrap();
        assert_eq!(see["parameters"]["required"], json!(["question"]));
    }

    #[test]
    fn question_validation_is_strict() {
        assert!(require_question("what is in this image?").is_ok());
        assert!(require_question("   ").is_err());
        assert!(require_question(&"x".repeat(4_001)).is_err());
    }

    #[test]
    fn truncation_marks_the_cut() {
        let out = truncate_chars(&"x".repeat(50), 10);
        assert!(out.contains("[… truncated]"));
        assert_eq!(out.chars().count(), 10 + "\n[… truncated]".chars().count());
    }

    #[test]
    fn declared_settings_pass_host_validation() {
        let plugin = build_plugin();
        let settings = &plugin.manifest().settings;
        assert_eq!(settings.len(), 8);
        sdk::validate_settings(settings).expect("declared settings must be valid");
    }
}
