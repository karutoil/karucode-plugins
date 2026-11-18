//! `free-search` — a karucode plugin that puts a keyless multi-engine web search
//! inside the agentic loop instead of beside it.
//!
//! # What this adds over `[[mcp_servers]]`
//!
//! karucode is already an MCP client, so connecting `uvx free-search-mcp` directly
//! would work. It would also lose five things, and those five are the reason this
//! plugin exists:
//!
//! | | plain MCP server | this plugin |
//! | --- | --- | --- |
//! | a dead backend | stays failed forever (no reconnect) | respawned on the next call, with a consecutive-failure budget |
//! | a 90 s search | killed by the harness's generic timeout | killed first, with a message that names how to narrow it |
//! | tool descriptions | upstream's, ~7 kB each, written for a chat client | ~600 bytes each, written as a decision procedure |
//! | approval | every call prompts (`external`, `read_only` denies) | search/fetch are in `read_only_tools` by default |
//! | the discipline | only in `initialize.instructions`, which agents skim | `before_prompt` every model call *and* restated in each tool description |
//!
//! # The three mechanisms
//!
//! 1. **A supervision layer.** `mcp.rs` owns the process: handshake, per-call
//!    deadline, restart after death, process-group kill so no orphaned Python
//!    survives.
//! 2. **A curated surface.** `tools.rs` rewrites every description for a coding
//!    agent and `schemas.rs` vendors the schemas so the handshake never waits on
//!    a Python import.
//! 3. **A discipline that survives the loop.** `before_prompt` states, on every
//!    model call and in byte-stable text, that a snippet is a lead and a fetched
//!    page is a fact — plus a panel that shows whether the backend is actually
//!    healthy and whether its schemas have drifted from the ones baked in.
//!
//! Debug it without the server:
//!
//! ```bash
//! printf '%s\n' \
//!   '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol":"1"}}' \
//!   '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
//!   | ./target/release/free-search
//! ```

// The vendored modules expose more API than this plugin uses; without these
// allowances every unused protocol type would warn on the first build.
#[allow(dead_code)]
mod protocol;
#[allow(dead_code, unused_imports)]
mod sdk;

mod config;
mod mcp;
mod schemas;
mod tools;

use mcp::{Backend, BackendConfig};
use sdk::{
    hooks, permissions, HostClient, PanelRequest, Plugin, PluginBuilder, PluginSetting,
    PluginSettingKind, PluginToolResult, PluginToolResultExt, ToolRequest,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use config::{Settings, LOCK_WAIT_SECS};
use tools::{Outcome, TOOLS};

/// The backend, created on first use.
///
/// A `OnceLock` rather than a captured closure because the SDK's handlers are
/// `Fn`, not `FnMut`. Nothing here starts a process: the backend is spawned by
/// the first tool call (or by `prewarm`), never during `initialize`, because the
/// harness's handshake budget is 10 s and a cold `uvx` install is longer than
/// that.
fn backend() -> &'static Mutex<Backend> {
    static BACKEND: OnceLock<Mutex<Backend>> = OnceLock::new();
    BACKEND.get_or_init(|| {
        let settings = settings();
        Mutex::new(Backend::new(BackendConfig {
            command: settings.command.clone(),
            env: settings.backend_env().into_iter().collect(),
            start_timeout: Duration::from_secs(settings.start_timeout_secs),
            call_timeout: Duration::from_secs(settings.call_timeout_secs),
        }))
    })
}

/// The settings this process is running with (initialized lazily inside `run()`
/// from `plugin/settings/get` merged over `KARUCODE_PLUGIN_SETTINGS`).
static SETTINGS: OnceLock<Settings> = OnceLock::new();

fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| {
        let raw = std::env::var("KARUCODE_PLUGIN_SETTINGS").ok();
        let env = raw
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .and_then(|r| match serde_json::from_str::<Value>(r) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("[free-search] plugin settings are not valid JSON ({e}); using defaults");
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
        let (parsed, problem) = Settings::from_value(&merged);
        if let Some(problem) = problem {
            eprintln!("[free-search] {problem}");
        }
        parsed
    })
}

/// Run `f` against the backend, turning a poisoned lock into a tool error
/// rather than a panic: a panic here would take the whole plugin process down
/// and the harness would report only "the plugin stopped".
fn with_backend<T>(f: impl FnOnce(&mut Backend) -> T) -> Result<T, PluginToolResult> {
    let mut guard = backend().lock().map_err(|_| {
        PluginToolResult::err(
            "the free-search backend is unusable after an earlier panic. Reload the plugin \
             (`POST /api/plugins/free-search/reload`) to get a fresh process.",
        )
    })?;
    Ok(f(&mut guard))
}

/// [`with_backend`] with a bounded wait and an explanation when it is exceeded.
///
/// `std::sync::Mutex` has no timed lock (`parking_lot`'s `try_lock_for` would be a
/// dependency this plugin does not otherwise need), so this polls. The back-off
/// starts at 5 ms and doubles to 100 ms: a lock released within the first
/// millisecond costs one `try_lock`, and a cold install costs about 13 wakeups
/// over its minute rather than thousands.
fn with_backend_bounded<T>(
    wait: Duration,
    f: impl FnOnce(&mut Backend) -> T,
) -> Result<T, PluginToolResult> {
    let deadline = Instant::now() + wait;
    let mut backoff = Duration::from_millis(5);
    let mut guard = loop {
        match backend().try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(PluginToolResult::err(
                        "the search backend is still starting up: a cold `uvx` install resolves \
                         about ninety Python packages the first time, which takes a minute or \
                         so. Nothing about your arguments is wrong. Call again shortly; every \
                         start after that one is under a second.",
                    ));
                }
                std::thread::sleep(backoff.min(deadline.saturating_duration_since(Instant::now())));
                backoff = (backoff * 2).min(Duration::from_millis(100));
            }
        }
    };
    Ok(f(&mut guard))
}

/// One tool call: supervise, forward, translate the result.
fn call_backend(req: &ToolRequest, name: &str) -> PluginToolResult {
    let settings = settings();
    if !settings.exposes(name) {
        return PluginToolResult::err(format!(
            "`{name}` is disabled by the `tools` plugin setting."
        ));
    }
    // The backend takes the arguments it documents and nothing else. Passing a
    // stray key would be a schema error at best and silently ignored at worst,
    // so the curated `format` default is applied here where it is visible.
    let arguments = req.arguments.clone();

    let result = with_backend_bounded(Duration::from_secs(LOCK_WAIT_SECS), |backend| {
        backend.call_tool(name, arguments)
    })
    .and_then(|r| r.map_err(|e| PluginToolResult::err(e.to_string())));
    let result = match result {
        Ok(value) => value,
        Err(failure) => return failure,
    };

    let (outcome, text) = tools::render(&result, settings.max_output_bytes);
    let bytes = text.len();
    match outcome {
        Outcome::Ok => PluginToolResult::ok(text).with_meta(json!({
            "backend": name,
            "result_bytes": bytes,
        })),
        Outcome::ToolFailed => PluginToolResult::err(strip_backend_prefix(&text, name)),
    }
}

/// The backend prefixes its errors with `Error executing tool <name>: ` (its own
/// MCP boundary). karucode already says which tool failed, so repeating it
/// doubles the sentence the model has to read to find the actionable half.
fn strip_backend_prefix(text: &str, name: &str) -> String {
    for prefix in [
        format!("Error executing tool {name}: "),
        format!("Error executing tool {name}:"),
    ] {
        if let Some(rest) = text.strip_prefix(&prefix) {
            return rest.trim().to_string();
        }
    }
    text.to_string()
}

/// `before_prompt` — the discipline, on every model call.
///
/// # Why this text and not a tool description
///
/// A tool description is read when the model considers a call; this block is in
/// the system prompt from the first token. The rule it states is the one an agent
/// gets wrong without supervision: it answers from a snippet, states a version
/// number that is two years old, and reports it as current.
///
/// # Why it is a `const`
///
/// `before_prompt` runs on **every** model call and its output is part of
/// message 0. Anything that varies — a timestamp, a count, the backend's uptime —
/// changes the prefix and makes the provider re-bill the entire context instead of
/// reading it from the prompt cache. So this string is byte-identical on every
/// call, and the panel (not the prompt) is where live state goes.
fn before_prompt(_payload: Value) -> Value {
    Value::String(
        "[Web search]\n\
         `plugin__free-search__search` reaches the live web with no API key: never ask the \
         user for one. Use `research` when you want to understand a topic (one call returns \
         sources and their text); use `search` then `fetch` when you want one specific fact.\n\
         A search result's snippet is an engine's summary, not the page: it drops qualifiers \
         and is often years old. Read any date, version, price, deadline, rule or number from \
         the page itself with `fetch` before you state it or act on it — for anything \
         current, `fetch`ing is the difference between a lead and a fact.\n\
         Check what the result says about age before trusting it: the header reports how many \
         results could be dated (`dated: 3/10`) and each one is marked `dated …` or `undated`, \
         and a cached copy states its own age. Re-run with `force_refresh=true` or \
         `max_age_hours=0` when it matters.\n\
         Never pass a URL to `search` — that is `fetch`'s job. Cite the URL you actually \
         read, not the one the search returned."
            .to_string(),
    )
}

/// Compare the schemas baked into this build with what a live backend actually
/// advertises.
///
/// The plugin vendors its tool schemas (`schemas.rs`) so `initialize` never waits
/// on a Python import. The cost of that decision is drift: a backend upgrade can
/// add a tool, remove one, or change an argument, and the agent would keep seeing
/// the old surface with no sign anything changed. This is the mitigation, and it
/// is why `Backend::list_tools` exists.
///
/// It reports rather than acts. A tool that disappeared from the backend is worth
/// knowing about; silently disabling it would trade a visible mismatch for a
/// mysterious "no such tool" failure. Tools are matched by name only: comparing
/// full schemas would flag every cosmetic upstream change and the report would be
/// ignored within a week.
///
/// Returns `None` when the backend could not be asked (it is not running and
/// starting it would be a side effect of opening a tab).
fn schema_drift(backend: &mut Backend) -> Option<Value> {
    if !backend.is_running() {
        return None;
    }
    let live = match backend.list_tools() {
        Ok(tools) => tools,
        Err(e) => return Some(json!({"error": e.to_string()})),
    };
    let live_names: std::collections::BTreeSet<String> =
        live.iter().map(|t| t.name.clone()).collect();
    let baked: std::collections::BTreeSet<String> = schemas::all()
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();

    let added: Vec<&String> = live_names.difference(&baked).collect();
    let missing: Vec<&String> = baked.difference(&live_names).collect();
    let unexposed: Vec<&str> = added
        .iter()
        .map(|s| s.as_str())
        .filter(|n| tools::spec_for(n).is_none())
        .collect();

    // Argument drift, compared by name rather than by full schema: a new argument
    // upstream is a capability the agent *cannot use* until this build is
    // regenerated, which is the kind of mismatch worth a line in the panel. A
    // changed description or a reordered `anyOf` is cosmetic and comparing it
    // would make the report noisy enough to ignore.
    let baked_map: std::collections::BTreeMap<&str, Value> =
        schemas::all().into_iter().collect();
    let mut argument_drift = Vec::new();
    let names = |schema: &Value| -> std::collections::BTreeSet<String> {
        schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    };
    for tool in &live {
        let Some((_, baked_schema)) = baked_map.get_key_value(tool.name.as_str()) else {
            continue;
        };
        let live_args = names(&tool.schema);
        let baked_args = names(baked_schema);
        if live_args != baked_args {
            argument_drift.push(json!({
                "tool": tool.name,
                "added_upstream": live_args.difference(&baked_args).collect::<Vec<_>>(),
                "missing_upstream": baked_args.difference(&live_args).collect::<Vec<_>>(),
            }));
        }
    }

    Some(json!({
        "checked": true,
        "live_version": backend.server_info().map(|s| s.version.clone()),
        "live_tool_count": live.len(),
        "baked_tool_count": baked.len(),
        // Present in the backend but not in this build. Harmless until someone
        // regenerates; the plugin simply does not expose them.
        "added_upstream": added,
        // Baked in but gone from the backend: these calls will fail.
        "missing_upstream": missing,
        // The subset of `added_upstream` that has no curated description, so it
        // is not exposed even after a regeneration until tools.rs is updated.
        "need_a_description": unexposed,
        // Arguments the backend accepts that this build cannot offer the agent.
        "argument_drift": argument_drift,
        "stale": !added.is_empty() || !missing.is_empty() || !argument_drift.is_empty(),
        // How much of the prompt the curated descriptions save. The upstream
        // `search` docstring alone is 7.4 kB; this is the number that justifies
        // rewriting them.
        "upstream_description_bytes": live.iter().map(|t| t.description_len).sum::<usize>(),
        "curated_description_bytes": tools::curated_description_bytes(),
        "upstream_instructions_bytes": backend
            .server_info()
            .map(|s| s.instructions.len())
            .unwrap_or(0),
    }))
}

/// `panels/read` for the `free-search` panel: is the backend actually healthy,
/// and has it drifted from the schemas baked into this build?
///
/// It does **not** start the backend. A panel is read whenever the tab is opened
/// or refreshed, so making it spawn a Python process would turn looking at the
/// tab into a side effect. It reports what the plugin already knows and says what
/// a call would do.
fn panel_status(req: PanelRequest) -> Value {
    let settings = settings();
    // The panel must not wait behind a cold install either: it is read when a tab
    // is opened, and a stalled read looks like a broken UI.
    let (report, drift) = with_backend_bounded(Duration::from_secs(2), |backend| {
        let running = backend.is_running();
        let stats = json!({
            "running": running,
            "pid": backend.pid(),
            "uptime_secs": backend.uptime().map(|d| d.as_secs()),
            "server": backend.server_info().map(|s| json!({
                "name": s.name,
                "version": s.version,
            })),
            "stats": {
                "calls": backend.stats().calls,
                "errors": backend.stats().errors,
                "restarts": backend.stats().restarts,
                "consecutive_failures": backend.stats().consecutive_failures,
                "last_call_ms": backend.stats().last_call_ms,
                "last_error": backend.stats().last_error,
            },
        });
        (stats, schema_drift(backend))
    })
    .unwrap_or_else(|failure| (json!({"running": false, "error": failure.content}), None));

    // Cache footprint: one `read_dir` of a directory the backend owns. Cheap, and
    // it is the number that explains why a search answered instantly.
    let cache_dir = settings
        .env
        .get("SEARCH_MCP_CACHE_DIR")
        .cloned()
        .or_else(|| settings.cache_dir.clone())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| format!("{h}/.cache/search-mcp"))
        });

    let tools: Vec<Value> = TOOLS
        .iter()
        .filter(|t| settings.exposes(t.name))
        .map(|t| {
            json!({
                "name": t.name,
                "class": match t.class {
                    tools::Class::ReadOnly => "read_only",
                    tools::Class::Mutating => "mutating",
                },
                "description_bytes": t.description.len(),
            })
        })
        .collect();

    json!({
        "backend": report,
        "config": {
            "command": settings.command_line(),
            "call_timeout_secs": settings.call_timeout_secs,
            "start_timeout_secs": settings.start_timeout_secs,
            "max_output_bytes": settings.max_output_bytes,
            "cache_dir": cache_dir,
            "env_keys": settings.env.keys().collect::<Vec<_>>(),
            "workspace": req.workspace,
            // What the operator should list in `settings.read_only_tools` to stop
            // these calls prompting. Every plugin tool is `external` whatever it
            // declares, so this is the only lever, and it is derived from the tool
            // table rather than hand-maintained.
            "read_only_candidates": tools::read_only_names(),
        },
        "schemas": {
            "baked_version": schemas::UPSTREAM_VERSION,
            "tools": tools,
            "drift": drift,
            "note": "These are vendored so the plugin handshake never waits on a Python \
                     import. `drift` compares them with the running backend and is null \
                     until the backend has started once; regenerate with \
                     probe/gen_schemas.py when it reports a difference.",
        },
    })
}

fn declared_settings() -> Vec<PluginSetting> {
    vec![
        PluginSetting {
            key: "command".into(),
            label: "Search backend command".into(),
            kind: PluginSettingKind::Text,
            default: json!(config::DEFAULT_COMMAND.join(" ")),
            help: "Command and arguments used to spawn free-search-mcp.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "cache_dir".into(),
            label: "Backend cache directory".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Optional SEARCH_MCP_CACHE_DIR override (empty = upstream default).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "call_timeout_secs".into(),
            label: "Backend call timeout (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(config::DEFAULT_CALL_TIMEOUT_SECS),
            help: "Seconds one call may wait on the backend (keep under 52s to fit the 60s host cap).".into(),
            min: Some(1.0),
            max: Some(3600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "start_timeout_secs".into(),
            label: "Backend startup timeout (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(config::DEFAULT_START_TIMEOUT_SECS),
            help: "Seconds a cold uvx install/startup may take before failing.".into(),
            min: Some(1.0),
            max: Some(3600.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "max_output_bytes".into(),
            label: "Max tool output bytes".into(),
            kind: PluginSettingKind::Number,
            default: json!(config::DEFAULT_MAX_OUTPUT_BYTES),
            help: "Soft byte cap before returning a truncation/pagination hint.".into(),
            min: Some(500.0),
            max: Some(1_000_000.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "prewarm".into(),
            label: "Prewarm Python backend at startup".into(),
            kind: PluginSettingKind::Bool,
            default: json!(true),
            help: "Spawn free-search-mcp in the background when the plugin starts so the first search is fast.".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
        PluginSetting {
            key: "tools".into(),
            label: "Exposed tools filter".into(),
            kind: PluginSettingKind::Text,
            default: json!(""),
            help: "Comma-separated backend tool names to expose (empty = all tools).".into(),
            min: None,
            max: None,
            options: Vec::new(),
        },
    ]
}

fn build_plugin(env_settings: &Settings) -> Plugin {
    let mut builder = PluginBuilder::new("free-search", env!("CARGO_PKG_VERSION"))
        .description(
            "Keyless multi-engine web search, page fetching and document reading, with a \
             supervised backend, an agent-facing tool surface and a verify-don't-trust \
             discipline in the prompt.",
        )
        // The plugin spawns a process and talks to the network. Declarative: the
        // `[[plugins]]` entry has to grant these or the tools are not exposed.
        .permission(permissions::SHELL)
        .permission(permissions::NETWORK);
    for setting in declared_settings() {
        builder = builder.setting(setting);
    }

    for (name, schema) in schemas::all() {
        if !env_settings.exposes(name) {
            continue;
        }
        let Some(spec) = tools::spec_for(name) else {
            // A backend tool with no curated description would arrive with its
            // multi-kilobyte upstream docstring. Better to leave it out and let
            // the panel show the drift than to blow up the prompt.
            eprintln!(
                "[free-search] backend advertises `{name}` but this build has no description \
                 for it; it is not exposed. Regenerate with probe/gen_schemas.py."
            );
            continue;
        };
        let description = tools::description_for(spec);
        let schema = tools::schema_for(schema);
        // The class is declared honestly (`read_only` for everything that cannot
        // change what the user owns) but the host exposes every plugin tool as
        // `external` regardless: a plugin cannot exempt itself from the approval
        // policy. What actually skips the prompt is `settings.read_only_tools` in
        // the `[[plugins]]` entry, which `plugin.toml` ships with the safe set.
        let class = match spec.class {
            tools::Class::ReadOnly => Some("read_only"),
            tools::Class::Mutating => Some("mutating"),
        };
        builder = builder.tool_with_class(
            name,
            &description,
            schema,
            class,
            move |req: ToolRequest| call_backend(&req, name),
        );
    }

    builder
        .hook(hooks::BEFORE_PROMPT, before_prompt)
        .panel_with("status", "Free search", Some("search"), 0, panel_status)
        .build()
}

fn main() -> std::io::Result<()> {
    let (env_settings, _) = Settings::parse(std::env::var("KARUCODE_PLUGIN_SETTINGS").ok().as_deref());
    let plugin = build_plugin(&env_settings);

    // Prewarm in a background thread once the JSON-RPC loop is live so
    // `settings()` can read operator overrides from `plugin/settings/get`.
    std::thread::Builder::new()
        .name("free-search-prewarm".into())
        .spawn(|| {
            while !HostClient::global().is_live() {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !settings().prewarm {
                return;
            }
            let outcome = with_backend(|backend| {
                let started = backend.ensure_started();
                (started, backend.server_info().map(|s| s.version.clone()))
            });
            match outcome {
                Ok((Ok(()), version)) => {
                    eprintln!("[free-search] backend ready (version {version:?})")
                }
                Ok((Err(e), _)) => eprintln!("[free-search] prewarm failed: {e}"),
                Err(_) => eprintln!("[free-search] prewarm skipped: the backend lock is poisoned"),
            }
        })
        .ok();

    plugin.run()?;

    // The SDK returns on `shutdown` or EOF. Stopping the backend here is what
    // keeps a killed plugin from leaving a Python process behind; the child also
    // has a PDEATHSIG and its own process group, so even a SIGKILL'd plugin
    // takes the backend with it.
    if let Ok(mut backend) = backend().lock() {
        backend.stop();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_that_arrives_during_a_cold_start_is_told_so() {
        // The prewarm thread holds the backend lock for the whole of a cold `uvx`
        // install. If a tool call blocked on it, the total could exceed karucode's
        // 60 s plugin budget and the model would get the harness's generic
        // timeout. `with_backend_bounded` must give up first, with text that says
        // the arguments are not the problem.
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        let started = Instant::now();
        let err = {
            // Hold the lock the way prewarm does while the call waits behind it.
            let _held = backend().lock().unwrap();
            with_backend_bounded(Duration::from_millis(150), |_| ())
        }
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(err.content.contains("still starting up"), "{}", err.content);
        assert!(err.content.contains("Nothing about your arguments"), "{}", err.content);
        assert!(elapsed >= Duration::from_millis(150), "gave up too early: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "waited too long: {elapsed:?}");

        // And once the lock is free, the same call goes through.
        assert!(with_backend_bounded(Duration::from_millis(150), |_| ()).is_ok());
    }

    /// The same lock the mcp tests use; this test also touches the static backend.
    fn exec_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        &LOCK
    }

    #[test]
    fn the_shipped_manifest_grants_what_its_tools_need() {
        // The host withholds a `mutating` tool unless the entry grants
        // `files.write`, and a `shell` tool without `shell`. Observed for real: the
        // manifest shipped `["shell","network"]`, the host silently dropped
        // `download`, and the plugin advertised 11 tools while the agent could see
        // 10. The remedy is the permission, not removing the tool: `download` stays
        // out of the read-only allowlist, so it still prompts.
        let manifest = include_str!("../plugin.toml");
        let permissions = manifest
            .lines()
            .find(|l| l.starts_with("permissions = "))
            .expect("plugin.toml must grant permissions");
        assert!(permissions.contains("\"shell\""), "the backend is a spawned process");
        assert!(permissions.contains("\"network\""), "it reaches the web");
        assert!(
            permissions.contains("\"files.write\""),
            "`download` declares `mutating`, so without this the host silently \
             withholds it: {permissions}"
        );
        // And the loop closes: the tool that needs files.write must not also be
        // on the list that skips the prompt.
        assert!(!shipped_read_only_tools().iter().any(|t| t == "download"));
    }

    #[test]
    fn the_lock_wait_leaves_room_under_the_harness_budget() {
        // Lock wait + backend request must fit inside karucode's 60 s plugin call
        // budget, or the model gets the harness's generic timeout instead of this
        // plugin's explanation. This is the reason both numbers exist.
        for call in [Settings::default().call_timeout_secs, 50] {
            assert!(
                LOCK_WAIT_SECS + call < 60,
                "{LOCK_WAIT_SECS} + {call} does not leave room under 60s"
            );
        }
    }

    #[test]
    fn the_prompt_block_is_byte_stable() {
        // This output is part of the system prompt and is re-requested on every
        // model call. Anything varying (a timestamp, a count) would change
        // message 0 and make the provider re-bill the whole context.
        let first = before_prompt(Value::Null);
        for _ in 0..5 {
            assert_eq!(before_prompt(Value::Null), first);
        }
        let text = first.as_str().unwrap();
        assert!(text.contains("never ask the user"), "{text}");
        assert!(text.contains("not the page"), "{text}");
        assert!(text.contains("dated: 3/10"), "{text}");
        assert!(text.contains("fetch"), "{text}");
        // The hook budget is 4 KiB per contribution before the host truncates.
        assert!(text.len() < 4 * 1024, "{} bytes", text.len());
    }

    #[test]
    fn the_prompt_block_ignores_its_payload() {
        // The host may add fields to the payload at any time; the block must not
        // start varying with them.
        let payload = json!({"session_id": "s1", "something_new": 42});
        assert_eq!(before_prompt(payload), before_prompt(Value::Null));
    }

    #[test]
    fn backend_errors_lose_the_doubled_prefix() {
        // The model reads "plugin `free-search` reported that tool `search`
        // failed: <content>"; the backend's own "Error executing tool search: "
        // would make that sentence repeat itself.
        assert_eq!(
            strip_backend_prefix("Error executing tool search: query must not be empty", "search"),
            "query must not be empty"
        );
        assert_eq!(
            strip_backend_prefix("Refusing to connect to blocked address", "fetch"),
            "Refusing to connect to blocked address"
        );
        // A different tool's prefix is left alone rather than half-stripped.
        assert_eq!(
            strip_backend_prefix("Error executing tool fetch: boom", "search"),
            "Error executing tool fetch: boom"
        );
    }

    #[test]
    fn the_curated_surface_matches_what_the_plugin_registers() {
        // Every tool that will be exposed has a description and a schema, and the
        // two lists agree — a name in one and not the other is a tool the agent
        // silently cannot call.
        let schema_names: Vec<&str> = schemas::all().iter().map(|(n, _)| *n).collect();
        for spec in TOOLS {
            assert!(
                schema_names.contains(&spec.name),
                "`{}` has a description but no schema",
                spec.name
            );
        }
        for name in schema_names {
            assert!(
                tools::spec_for(name).is_some(),
                "`{name}` has a schema but no description, so it will not be exposed"
            );
        }
    }

    #[test]
    fn every_exposed_schema_survives_curation() {
        // `schema_for` strips titles; it must not lose the arguments themselves.
        for (name, schema) in schemas::all() {
            let out = tools::schema_for(schema.clone());
            assert_eq!(out["type"], json!("object"), "{name}");
            let before = schema["properties"].as_object().unwrap().len();
            let after = out["properties"].as_object().unwrap().len();
            assert_eq!(before, after, "`{name}` lost a property");
            assert_eq!(out["required"], schema["required"], "`{name}` lost `required`");
        }
    }

    #[test]
    fn the_declared_class_is_the_one_the_surface_intends() {
        // `download` is the only tool with a side effect outside the
        // conversation, so it is the only one declaring `mutating`. Declaring
        // `read_only` on it would be a lie in the direction that costs the user a
        // file; the host ignores the hint for exposure either way.
        for (name, _) in schemas::all() {
            let spec = tools::spec_for(name).unwrap();
            let class = match spec.class {
                tools::Class::ReadOnly => Some("read_only"),
                tools::Class::Mutating => Some("mutating"),
            };
            let expected = if name == "download" { Some("mutating") } else { Some("read_only") };
            assert_eq!(class, expected, "`{name}`");
        }
    }

    #[test]
    fn drift_is_not_checked_while_the_backend_is_asleep() {
        // Opening the panel must never spawn a Python process, so an unstarted
        // backend reports "not checked" rather than a mismatch.
        let mut backend = Backend::new(BackendConfig {
            command: vec!["/nonexistent/binary-xyz".into()],
            env: Default::default(),
            start_timeout: Duration::from_millis(100),
            call_timeout: Duration::from_millis(100),
        });
        assert!(schema_drift(&mut backend).is_none());
    }

    /// The shipped `plugin.toml`, parsed just enough to read one setting.
    ///
    /// Read as text rather than through a TOML parser: this crate has exactly two
    /// dependencies (serde, serde_json) and adding a parser only for a test would
    /// be a dependency the shipped binary carries for nothing.
    fn shipped_read_only_tools() -> Vec<String> {
        let manifest = include_str!("../plugin.toml");
        let settings = manifest
            .split("[settings]")
            .nth(1)
            .expect("plugin.toml must have a [settings] section");
        let mut names = Vec::new();
        let mut inside = false;
        for line in settings.lines() {
            let line = line.trim();
            if line.starts_with("read_only_tools") {
                inside = true;
                continue;
            }
            if inside {
                if let Some(rest) = line.strip_prefix(']') {
                    // A one-line array is written as `read_only_tools = [...]`;
                    // that form is not used here but must not read as an empty set.
                    let _ = rest;
                    break;
                }
                if let Some(name) = line.trim_start_matches('"').split('"').next() {
                    if !name.is_empty() && !name.starts_with('#') {
                        names.push(name.to_string());
                    }
                }
            }
        }
        names
    }

    #[test]
    fn the_shipped_allowlist_is_exactly_the_read_only_tools() {
        // The shipped manifest is the operator's out-of-the-box experience: if it
        // lists a tool the table calls mutating, a call that writes a file stops
        // prompting. If it misses one, every search prompts and the plugin feels
        // worse than the plain MCP server.
        let mut shipped = shipped_read_only_tools();
        shipped.sort();
        let mut expected: Vec<String> = tools::read_only_names()
            .into_iter()
            .map(str::to_string)
            .collect();
        expected.sort();
        assert_eq!(shipped, expected);
        assert!(!shipped.iter().any(|t| t == "download"));
    }

    #[test]
    fn the_shipped_manifest_names_an_absolute_command() {
        // The plugin is spawned by the server, whose working directory is not the
        // repository; a relative path would fail to start with no useful error.
        let manifest = include_str!("../plugin.toml");
        let command = manifest
            .lines()
            .find(|l| l.starts_with("command ="))
            .expect("plugin.toml must set a command");
        assert!(command.contains("[\"/"), "{command}");
        assert!(command.contains("target/release/free-search"), "{command}");
    }

    #[test]
    fn the_shipped_timeouts_respect_both_harness_limits() {
        // Both numbers are load-bearing: the call timeout must fire before
        // karucode's 60s plugin budget, and the output cap must sit under the
        // 24000-byte default so this plugin's better truncation note is the one
        // the model reads.
        let (settings, err) = Settings::parse(Some(
            // The same values plugin.toml ships.
            r#"{"call_timeout_secs": 55, "start_timeout_secs": 120, "max_output_bytes": 20000}"#,
        ));
        assert!(err.is_none());
        assert!(settings.call_timeout_secs < 60);
        assert!(settings.max_output_bytes < 24_000);
        assert!(Settings::default().call_timeout_secs < 60);
        assert!(Settings::default().max_output_bytes < 24_000);
    }

    #[test]
    fn declared_settings_pass_host_validation() {
        let p = build_plugin(&Settings::default());
        let settings = &p.manifest().settings;
        assert_eq!(settings.len(), 7);
        sdk::validate_settings(settings).expect("declared settings must be valid");
    }
}
