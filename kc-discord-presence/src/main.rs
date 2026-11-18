//! `discord-presence` - what karucode is doing, in a Discord channel.
//!
//! The operator pastes one Discord webhook URL into a chat; this plugin posts a
//! status card and keeps editing it as work moves - project, session, run state,
//! elapsed time, the last tool call, pending approvals, live subagents - and posts
//! the notifications that actually want a human.
//!
//! Why a webhook and not a bot: no application, no bot user, no gateway socket, no
//! privileged intent, no TLS websocket client, no invite. The URL *is* the setup.
//!
//! Why the database and not the HTTP API: every API route needs a credential, and
//! a plugin cannot inherit one (the host clears its environment). The database
//! needs only a path, so this plugin works the moment it is installed.
//!
//! Where the parts live:
//!
//! * [`config`] - settings, the persisted webhook, redaction;
//! * [`karucode`] - the read-only snapshot of sessions, runs, approvals, agents;
//! * [`card`] - snapshot -> Discord embed, with every limit clamped;
//! * [`discord`] - the webhook client (post, edit, 429, 404);
//! * [`worker`] - the single publisher thread and the state tools read.

// The vendored modules expose more API than this plugin uses; without these
// allowances every unused protocol type would warn on the first build.
#[allow(dead_code)]
mod protocol;
#[allow(dead_code, unused_imports)]
mod sdk;

mod card;
mod config;
mod discord;
mod karucode;
mod presence;
mod worker;

use serde_json::{json, Value};
use std::sync::{Arc, Mutex, OnceLock};

use sdk::{
    hooks, PanelRequest, Plugin, PluginBuilder, PluginSetting, PluginSettingKind, PluginToolResult,
    PluginToolResultExt, ToolRequest, UiActionRequest, UiActionResult, UiActionResultExt,
};

static APP: OnceLock<Arc<worker::App>> = OnceLock::new();
static PUBLISHER: OnceLock<Mutex<Option<std::thread::JoinHandle<()>>>> = OnceLock::new();

fn app() -> &'static Arc<worker::App> {
    APP.get_or_init(|| {
        let cfg = config::Effective::load();
        for problem in &cfg.problems {
            eprintln!("discord-presence: {problem}");
        }
        let instance = worker::App::new(cfg);
        let handle = worker::App::spawn(Arc::clone(&instance));
        let slot = PUBLISHER.get_or_init(|| Mutex::new(None));
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(handle);
        }
        instance
    })
}

fn declared_settings() -> Vec<PluginSetting> {
    vec![
        PluginSetting {
            key: "application_id".into(),
            label: "Discord Application ID".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "15-25 digit Application ID from discord.com/developers/applications for Rich Presence.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "presence".into(),
            label: "Enable Discord Rich Presence".into(),
            kind: PluginSettingKind::Bool,
            default: json!(true),
            help: "Publish profile activity ('Playing <app>') over the local Discord IPC socket.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "large_image".into(),
            label: "Rich Presence large image".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Asset key uploaded to your Discord app or an https image URL.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "large_text".into(),
            label: "Rich Presence image tooltip".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Tooltip shown when hovering the large Rich Presence image.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "presence_socket".into(),
            label: "Explicit Discord IPC socket path".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional discord-ipc-N socket path when auto-discovery misses a container/flatpak.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "webhook_url".into(),
            label: "Channel webhook URL".into(),
            kind: PluginSettingKind::Secret,
            default: json!(""),
            help: "Optional Discord webhook URL (https://discord.com/api/webhooks/...) for live cards and notifications.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "thread_id".into(),
            label: "Webhook thread ID".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional Discord forum/thread ID to post into instead of the main channel.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "refresh_secs".into(),
            label: "Channel card refresh interval (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(config::DEFAULT_REFRESH_SECS),
            help: "Seconds between channel card edits (10..3600, or 0 to disable the live card).".into(),
            min: Some(0.0),
            max: Some(config::MAX_REFRESH_SECS as f64),
            options: Vec::new(),
        },
        PluginSetting {
            key: "notify".into(),
            label: "Notification kinds to post".into(),
            kind: PluginSettingKind::Text,
            default: json!(config::DEFAULT_NOTIFY),
            help: "Comma-separated: run_finished, needs_input, error, info, or all/none.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "username".into(),
            label: "Webhook display name".into(),
            kind: PluginSettingKind::Text,
            default: json!(config::DEFAULT_USERNAME),
            help: "Display name used when posting messages to the Discord webhook.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "avatar_url".into(),
            label: "Webhook avatar URL".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional HTTPS avatar image URL for the Discord webhook.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "include_recent".into(),
            label: "Include recent run outcomes".into(),
            kind: PluginSettingKind::Bool,
            default: json!(true),
            help: "Show recent run completions/failures from the last hour in the channel card.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "max_sessions".into(),
            label: "Max sessions on channel card".into(),
            kind: PluginSettingKind::Number,
            default: json!(config::DEFAULT_MAX_SESSIONS),
            help: "Maximum number of sessions listed on the Discord channel status card (1..8).".into(),
            min: Some(1.0),
            max: Some(config::MAX_SESSIONS as f64),
            options: Vec::new(),
        },
        PluginSetting {
            key: "data_dir".into(),
            label: "karucode data directory override".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Only needed when karucode runs with a custom --data-dir.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
    ]
}

fn build_plugin() -> Plugin {
    let mut builder = PluginBuilder::new("discord-presence", env!("CARGO_PKG_VERSION"))
        .description(
            "Show what karucode is doing as Discord Rich Presence on the operator's profile - \
             project, session, elapsed time - through the local Discord client, and optionally \
             mirror the same state into a channel as a live card with notifications.",
        )
        // The presence is a Unix socket; the optional card is HTTPS; the snapshot
        // reads karucode's database; the settings are persisted under the data dir.
        .permission(sdk::permissions::NETWORK)
        .permission(sdk::permissions::FILES_READ)
        .permission(sdk::permissions::FILES_WRITE);
    for setting in declared_settings() {
        builder = builder.setting(setting);
    }
    builder
        .tool_with_class(
            "discord_status",
            "Report whether the Discord Rich Presence is live and what it says: the Application ID, \
             whether a Discord client is connected and on which socket, the last error, the optional \
             channel card's state, the notification kinds, and whether karucode's database is \
             readable. Sends nothing. Call this first when someone asks whether Discord is set up.",
            json!({"type": "object", "properties": {}}),
            Some("read_only"),
            |_req: ToolRequest| PluginToolResult::ok(pretty(&app().status_json())),
        )
        .tool_with_class(
            "discord_configure",
            "Connect this plugin to Discord. The main setup is an Application ID, which makes the \
             operator's Discord profile show 'Playing <app>' with the project and session they are \
             working on: create an application at discord.com/developers/applications (name it \
             'karucode'), open it, and copy the Application ID - a long number, not the client \
             secret. The Discord desktop client must be running on this machine; if it is not, the \
             ID is still stored and the presence attaches as soon as it is. Optionally also pass a \
             channel webhook URL to mirror the same state into a channel as a live card.",
            json!({
                "type": "object",
                "properties": {
                    "application_id": {
                        "type": "string",
                        "description": "The Discord Application ID (digits only) from discord.com/developers/applications. This is what the profile line is named after."
                    },
                    "webhook_url": {
                        "type": "string",
                        "description": "Optional: a channel webhook URL (https://discord.com/api/webhooks/<id>/<token>) to also publish a live status card and notifications."
                    },
                    "large_image": {
                        "type": "string",
                        "description": "Optional Rich Presence asset key (uploaded to the application) or an https image URL, shown as the large image."
                    },
                    "large_text": {
                        "type": "string",
                        "description": "Optional tooltip for the large image (max 128 bytes)."
                    },
                    "presence_socket": {
                        "type": "string",
                        "description": "Optional explicit discord-ipc-N socket path, for a client that is not where discovery looks."
                    },
                    "thread_id": {
                        "type": "string",
                        "description": "Optional: post the card into this thread of the webhook's channel instead of the channel itself."
                    },
                    "refresh_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 3600,
                        "description": "Seconds between card updates (minimum 10). 0 turns the card off and keeps notifications. The presence is unaffected."
                    },
                    "notify": {
                        "type": "string",
                        "description": "Comma list of notification kinds to post as their own message: run_finished, needs_input, error, info, or all/none. Default needs_input,error."
                    }
                }
            }),
            Some("mutating"),
            |req: ToolRequest| {
                let request = worker::ConfigureRequest {
                    application_id: nonempty(req.str_arg("application_id")),
                    webhook_url: nonempty(req.str_arg("webhook_url")),
                    thread_id: nonempty(req.str_arg("thread_id")),
                    refresh_secs: req.arguments.get("refresh_secs").and_then(Value::as_u64),
                    notify: nonempty(req.str_arg("notify")),
                    large_image: nonempty(req.str_arg("large_image")),
                    large_text: nonempty(req.str_arg("large_text")),
                    presence_socket: nonempty(req.str_arg("presence_socket")),
                };
                if request.application_id.is_none() && request.webhook_url.is_none() {
                    return PluginToolResult::err(
                        "pass `application_id` for the profile presence (discord.com/developers/\
                         applications -> your application -> Application ID) and/or `webhook_url` \
                         for a channel card",
                    );
                }
                match app().configure(request) {
                    Ok(value) => PluginToolResult::ok(pretty(&value)),
                    Err(error) => PluginToolResult::err(error),
                }
            },
        )
        .tool(
            "discord_publish",
            "Push the current state to Discord now instead of waiting for the next refresh: the \
             profile presence first, then the channel card if one is configured. Use it after a \
             change the operator should see immediately, or to prove the connection still works.",
            json!({"type": "object", "properties": {}}),
            |_req: ToolRequest| match app().publish_now() {
                Ok(value) => PluginToolResult::ok(pretty(&value)),
                Err(error) => PluginToolResult::err(error),
            },
        )
        .tool(
            "discord_notify",
            "Post a one-off message to the Discord channel this plugin is connected to. Use it to \
             tell the operator something on their phone - a result they asked to be told about, a \
             decision you need, a summary of finished work. Keep the title to one line.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "One line, what happened."},
                    "body": {"type": "string", "description": "Optional detail: the answer, the path, the command."},
                    "kind": {
                        "type": "string",
                        "enum": ["info", "needs_input", "error", "run_finished"],
                        "description": "Colour and urgency. Default info."
                    }
                },
                "required": ["title"]
            }),
            |req: ToolRequest| {
                let title = req.str_arg("title").trim().to_string();
                if title.is_empty() {
                    return PluginToolResult::err("`title` is required");
                }
                let body = req.str_arg("body").trim().to_string();
                let kind = match req.str_arg("kind") {
                    "needs_input" | "error" | "run_finished" => req.str_arg("kind"),
                    _ => "info",
                };
                match app().notify(&title, &body, kind) {
                    Ok(value) => PluginToolResult::ok(pretty(&value)),
                    Err(error) => PluginToolResult::err(error),
                }
            },
        )
        // The live card, as the inspector's own tab. Memory only, so a 5 s poll
        // costs nothing.
        .panel_with(
            "board",
            "Discord",
            Some("bolt"),
            5_000,
            |_req: PanelRequest| app().panel_json(),
        )
        // One click to push the card now, for the times a person is looking at
        // karucode rather than at Discord.
        .button_with(
            "inspector.header",
            "push",
            "Push to Discord",
            Some("bolt"),
            Some("Publish the Discord status card now"),
            |_req: UiActionRequest| match app().publish_now() {
                Ok(value) => {
                    let message = if let Some(id) = value
                        .get("card")
                        .and_then(|card| card.get("message_id"))
                        .and_then(Value::as_str)
                    {
                        format!("Pushed to Discord ({id})")
                    } else if let Some(presence) = value.get("presence") {
                        let live = presence
                            .get("connected")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if live {
                            "Presence pushed to Discord".to_string()
                        } else {
                            "Presence stored; no Discord client is running yet".to_string()
                        }
                    } else {
                        "Pushed to Discord".to_string()
                    };
                    UiActionResult::ok(message).refresh("board")
                }
                Err(error) => UiActionResult::err(error),
            },
        )
        // Live detail comes from the host, not from re-reading the transcript:
        // the hook is cheap, the publisher decides when Discord hears about it.
        .hook(hooks::ON_TOOL_RESULT, |payload| {
            let session = payload
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let tool = payload.get("tool").and_then(Value::as_str).unwrap_or_default();
            let ok = payload.get("ok").and_then(Value::as_bool).unwrap_or(true);
            app().on_tool_result(session, tool, ok);
            Value::Null
        })
        .hook(hooks::ON_RUN_END, |payload| {
            let session = payload
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let run = payload.get("run_id").and_then(Value::as_str).unwrap_or_default();
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            app().on_run_end(session, run, status);
            Value::Null
        })
        .on_notify(|payload| {
            app().on_notify(&payload);
            Value::Null
        })
        .build()
}

fn main() -> std::io::Result<()> {
    // Initialize App and spawn the publisher once the JSON-RPC loop is live so
    // `plugin/settings/get` can reach the host.
    std::thread::Builder::new()
        .name("discord-init".into())
        .spawn(|| {
            while !sdk::HostClient::global().is_live() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = app();
        })
        .ok();
    let result = build_plugin().run();
    // Leave an honest card behind rather than a stale "running" forever.
    if let Some(instance) = APP.get() {
        instance.shutdown();
    }
    if let Some(slot) = PUBLISHER.get() {
        if let Ok(mut guard) = slot.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
    result
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn nonempty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_settings_pass_host_validation() {
        let p = build_plugin();
        let settings = &p.manifest().settings;
        assert_eq!(settings.len(), 14);
        sdk::validate_settings(settings).expect("declared settings must be valid");
    }
}
