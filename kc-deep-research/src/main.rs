//! `deep-research`: sustained, evidence-gated research built on free-search-mcp.
//!
//! The host caps a plugin tool call at 60s, so the research loop runs on a worker
//! thread. `start` answers immediately; `status(wait_secs<=45)` waits efficiently
//! for progress; `report` returns the fetched evidence and unresolved gaps.

mod config;
mod engine;
mod evidence;
mod gateway;
#[allow(dead_code)]
mod mcp;
mod model;
mod plan;
#[rustfmt::skip]
#[allow(dead_code)]
mod protocol;
mod report;
#[rustfmt::skip]
#[allow(dead_code, unused_imports)]
mod sdk;

use config::Settings;
use gateway::McpGateway;
use model::{Job, RunState, Store};
use sdk::{
    hooks, permissions, HostClient, PanelRequest, PluginBuilder, PluginSetting, PluginSettingKind,
    PluginToolResult, PluginToolResultExt, ToolRequest,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

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
                    eprintln!("[deep-research] plugin settings are not valid JSON ({e}); using defaults");
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
        let (settings, problem) = Settings::from_value(&merged);
        if let Some(problem) = problem {
            eprintln!("[deep-research] {problem}");
        }
        settings
    })
}

fn store() -> &'static Arc<Store> {
    static STORE: OnceLock<Arc<Store>> = OnceLock::new();
    STORE.get_or_init(|| Arc::new(Store::new()))
}

fn result_json(value: Value) -> PluginToolResult {
    PluginToolResult::ok(serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()))
}

fn notify_terminal(job: &Job) {
    if !job.state.terminal() {
        return;
    }
    let kind = if job.state == RunState::Failed {
        sdk::NotifyKind::Error
    } else {
        sdk::NotifyKind::RunFinished
    };
    let body = format!(
        "{}: {:?} after {} rounds; {} sources read. Use plugin__deep-research__report with this id.",
        job.id,
        job.state,
        job.round,
        job.sources.len()
    );
    if let Err(error) = sdk::notify_with(sdk::NotifyFrame {
        kind,
        title: format!("Deep research: {:?}", job.state),
        body,
        session_id: job.session_id.clone(),
        sound: None,
        sticky: false,
    }) {
        eprintln!("[deep-research] could not emit terminal notification: {error}");
    }
}

fn start(req: ToolRequest) -> PluginToolResult {
    let question = req.str_arg("question").trim();
    if question.is_empty() {
        return PluginToolResult::err("question is required and must not be empty");
    }
    if question.chars().count() > 4_000 {
        return PluginToolResult::err("question must be at most 4000 characters");
    }
    if evidence::keywords(question).is_empty() {
        return PluginToolResult::err(
            "question must contain a meaningful topic term; add a name, product, protocol, or concept rather than only generic words",
        );
    }
    let options = match model::ResearchOptions::from_args(&req.arguments) {
        Ok(options) => options,
        Err(error) => return PluginToolResult::err(error),
    };
    let requested = match plan::requested_aspects(&req.arguments) {
        Ok(aspects) => aspects,
        Err(error) => return PluginToolResult::err(error),
    };
    let session_id = req.session_id().map(str::to_string);
    let workspace = req.workspace().map(str::to_string);
    if let Some(existing) = store().list().into_iter().find(|job| {
        !job.state.terminal()
            && job.question == question
            && job.session_id == session_id
            && job.workspace == workspace
    }) {
        return PluginToolResult::err(format!(
            "an active job already has this question in this session: {}. Call status with that id instead of starting a duplicate",
            existing.id
        ));
    }

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = format!(
        "dr-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1
    );
    let aspects = plan::build_aspects(question, &requested);
    let job = Job::new(
        id.clone(),
        question.to_string(),
        aspects,
        options,
        session_id,
        workspace,
    );
    if let Err(error) = store().insert(job, settings().max_active_jobs) {
        return PluginToolResult::err(error);
    }

    let worker_store = Arc::clone(store());
    let worker_settings = settings().clone();
    let worker_id = id.clone();
    let spawned = std::thread::Builder::new().name(format!("deep-research-{id}")).spawn(move || {
        let Some(job) = worker_store.get(&worker_id) else { return };
        let mut gateway = McpGateway::new(&worker_settings);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine::execute(job, &mut gateway, |progress| {
                worker_store.publish(progress);
                worker_store.is_cancel_requested(&progress.id)
            })
        }));
        match outcome {
            Ok(job) => worker_store.publish(&job),
            Err(_) => {
                if let Some(mut job) = worker_store.get(&worker_id) {
                    job.state = RunState::Failed;
                    job.stop_reason = Some("The research worker panicked; collected evidence is preserved. Reload the plugin and report this bug.".into());
                    job.record(job.stop_reason.clone().unwrap());
                    worker_store.publish(&job);
                }
            }
        }
        if let Some(job) = worker_store.get(&worker_id) {
            notify_terminal(&job);
        }
    });
    if let Err(error) = spawned {
        if let Some(mut job) = store().get(&id) {
            job.state = RunState::Failed;
            job.stop_reason = Some(format!("could not spawn research worker: {error}"));
            store().publish(&job);
            notify_terminal(&job);
        }
        return PluginToolResult::err(format!("could not spawn research worker {id}: {error}"));
    }
    let job = store().get(&id).expect("inserted job must exist");
    result_json(job.status_json()).with_meta(json!({"id": id, "state": job.state}))
}

fn visible_to(job: &Job, req: &ToolRequest) -> bool {
    job.session_id.as_deref() == req.session_id() && job.workspace.as_deref() == req.workspace()
}

fn lookup(req: &ToolRequest) -> Result<(String, Job), PluginToolResult> {
    let id = req.str_arg("id").trim();
    if id.is_empty() {
        return Err(PluginToolResult::err(
            "id is required (returned by start or list)",
        ));
    }
    let job = store().get(id).filter(|job| visible_to(job, req)).ok_or_else(|| {
        PluginToolResult::err(format!(
            "unknown research job `{id}` in this session/workspace. Jobs are process-local and session-scoped; call list to see available jobs."
        ))
    })?;
    Ok((id.to_string(), job))
}

fn status(req: ToolRequest) -> PluginToolResult {
    let (id, snapshot) = match lookup(&req) {
        Ok(found) => found,
        Err(error) => return error,
    };
    let wait_secs = match req.arguments.get("wait_secs") {
        None | Some(Value::Null) => 30,
        Some(value) => match value.as_u64() {
            Some(seconds) if seconds <= 45 => seconds,
            _ => return PluginToolResult::err("wait_secs must be an integer between 0 and 45"),
        },
    };
    let job = if snapshot.state.terminal() || wait_secs == 0 {
        snapshot
    } else {
        store()
            .wait_for_progress(&id, snapshot.revision, Duration::from_secs(wait_secs))
            .unwrap_or(snapshot)
    };
    let state = job.state;
    let revision = job.revision;
    result_json(job.status_json())
        .with_meta(json!({"id": id, "state": state, "revision": revision}))
}

fn report(req: ToolRequest) -> PluginToolResult {
    let (_, job) = match lookup(&req) {
        Ok(found) => found,
        Err(error) => return error,
    };
    let format = match req.arguments.get("format") {
        None | Some(Value::Null) => "markdown",
        Some(value) => match value.as_str() {
            Some(format) => format,
            None => return PluginToolResult::err("format must be markdown or json"),
        },
    };
    let offset = match req.arguments.get("offset") {
        None | Some(Value::Null) => 0,
        Some(value) => match value.as_u64().and_then(|v| usize::try_from(v).ok()) {
            Some(offset) => offset,
            None => return PluginToolResult::err("offset must be a non-negative integer"),
        },
    };
    let max_bytes = match req.arguments.get("max_bytes") {
        None | Some(Value::Null) => settings().max_output_bytes,
        Some(value) => match value.as_u64().and_then(|v| usize::try_from(v).ok()) {
            Some(max) => max,
            None => {
                return PluginToolResult::err("max_bytes must be an integer between 1000 and 20000")
            }
        },
    };
    match report::render(&job, format, offset, max_bytes) {
        Ok(rendered) => PluginToolResult::ok(rendered.content).with_meta(rendered.meta),
        Err(error) => PluginToolResult::err(error),
    }
}

fn cancel(req: ToolRequest) -> PluginToolResult {
    let (id, _) = match lookup(&req) {
        Ok(found) => found,
        Err(error) => return error,
    };
    match store().cancel(&id) {
        Some(job) => result_json(job.status_json()),
        None => PluginToolResult::err(format!("unknown research job `{id}`")),
    }
}

fn list(req: ToolRequest) -> PluginToolResult {
    let jobs: Vec<_> = store()
        .list()
        .into_iter()
        .filter(|job| visible_to(job, &req))
        .collect();
    let active = jobs.iter().filter(|job| !job.state.terminal()).count();
    result_json(json!({
        "active": active,
        "jobs": jobs.iter().map(|job| json!({
            "id": job.id,
            "question": job.question,
            "state": job.state,
            "terminal": job.state.terminal(),
            "round": job.round,
            "searches": job.searches,
            "sources_read": job.sources.len(),
            "distinct_domains": job.domains().len(),
            "elapsed_seconds": job.elapsed().as_secs(),
            "stop_reason": job.stop_reason,
            "cancel_requested": job.cancel_requested,
        })).collect::<Vec<_>>(),
        "hint": "Call status to wait for progress, report for evidence, or cancel to stop a job after its current backend call.",
    }))
}

fn before_prompt(_payload: Value) -> Value {
    Value::String(
        "[Deep research]\n\
         For a broad, multi-faceted or high-confidence web research question, call \
         `plugin__deep-research__start` once (give explicit `aspects` when coverage matters). \
         It repeatedly searches and reads sources in the background; it is not the one-shot \
         `free-search` research tool. Then call `plugin__deep-research__status` with \
         `wait_secs=45` until `terminal` is true, and call `report` (follow `next_offset` pages).\n\
         Only `state=complete` means every aspect has relevant fetched evidence, minimum source/domain \
         and round budgets were met, and the core answer has cross-source coverage. `partial` \
         means answer with named gaps; snippets and unread candidates are never evidence.\n\
         For one specific fact or a URL, use `plugin__free-search__search`/`fetch` instead. Do not \
         start duplicate jobs or tell the user research is complete while it is still running."
            .to_string(),
    )
}

fn panel(req: PanelRequest) -> Value {
    let jobs: Vec<_> = store()
        .list()
        .into_iter()
        .filter(|job| {
            job.session_id.as_deref() == Some(req.session_id.as_str())
                && job.workspace.as_deref() == Some(req.workspace.as_str())
        })
        .collect();
    json!({
        "jobs": jobs.iter().map(|job| json!({
            "id": job.id,
            "question": job.question,
            "state": job.state,
            "round": job.round,
            "searches": job.searches,
            "sources_read": job.sources.len(),
            "domains": job.domains().len(),
            "coverage_met": job.coverage().met,
            "aspects": job.aspects.iter().map(|a| json!({
                "id": a.id, "sources": a.source_ids.len(), "attempts": a.attempts
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "active": jobs.iter().filter(|j| !j.state.terminal()).count(),
        "config": {
            "backend_command": settings().command_line(),
            "call_timeout_secs": settings().call_timeout_secs,
            "start_timeout_secs": settings().start_timeout_secs,
            "max_active_jobs": settings().max_active_jobs,
            "max_output_bytes": settings().max_output_bytes,
            "cache_dir": settings().cache_dir,
            "configured_env_keys": settings().env.keys().collect::<Vec<_>>(),
        },
        "integration": {
            "backend": "free-search-mcp",
            "calls": ["search", "fetch_batch", "fetch (with force_refresh)"],
            "cache": "Uses the upstream default SEARCH_MCP_CACHE_DIR unless cache_dir overrides it, matching kc-free-search",
            "job_storage": "process-local; collect reports before reloading the plugin",
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
            help: "Per-request deadline for a single backend search or fetch call.".into(),
            min: Some(1.0),
            max: Some(300.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "start_timeout_secs".into(),
            label: "Backend startup timeout (seconds)".into(),
            kind: PluginSettingKind::Number,
            default: json!(config::DEFAULT_START_TIMEOUT_SECS),
            help: "How long a cold uvx backend start may take before failing.".into(),
            min: Some(1.0),
            max: Some(300.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "max_output_bytes".into(),
            label: "Max inline output bytes".into(),
            kind: PluginSettingKind::Number,
            default: json!(config::DEFAULT_MAX_OUTPUT_BYTES),
            help: "Default byte ceiling for report pages and tool results.".into(),
            min: Some(1000.0),
            max: Some(20_000.0),
            options: Vec::new(),
        },
        PluginSetting {
            key: "max_active_jobs".into(),
            label: "Max concurrent research jobs".into(),
            kind: PluginSettingKind::Number,
            default: json!(2),
            help: "Maximum simultaneous research jobs (each holds one backend process).".into(),
            min: Some(1.0),
            max: Some(4.0),
            options: Vec::new(),
        },
    ]
}

fn build_plugin() -> sdk::Plugin {
    let mut builder = PluginBuilder::new("deep-research", env!("CARGO_PKG_VERSION"))
        .description(
            "Sustained background web research on free-search-mcp: gap-driven searches, source reading, \
             evidence coverage, cross-source coverage, explicit gaps, and paginated cited reports.",
        )
        .permission(permissions::SHELL)
        .permission(permissions::NETWORK);
    for setting in declared_settings() {
        builder = builder.setting(setting);
    }
    builder
        .tool_with_class(
            "start",
            "Start a sustained background research job. Supply the full question and optional explicit aspects. \
             The worker searches, fetches pages, rejects irrelevant ones, chases relevant citations and targets gaps \
             until coverage is met or a budget/stall limit is reached. Returns an id; call status(wait_secs=45), \
             not start again. Completion requires real fetched sources, aspects, domains and corroboration—not snippets.",
            json!({
                "type":"object",
                "properties": {
                    "question":{"type":"string"},
                    "aspects":{"type":"array","items":{"type":"string"},"maxItems":12},
                    "max_rounds":{"type":"integer","minimum":1,"maximum":40,"default":12},
                    "max_searches":{"type":"integer","minimum":1,"maximum":120,"default":36},
                    "max_sources":{"type":"integer","minimum":1,"maximum":60,"default":24},
                    "min_sources":{"type":"integer","minimum":1,"maximum":60,"default":8},
                    "min_domains":{"type":"integer","minimum":1,"maximum":20,"default":3},
                    "min_rounds":{"type":"integer","minimum":1,"maximum":40,"default":3},
                    "max_minutes":{"type":"integer","minimum":1,"maximum":180,"default":20},
                    "results_per_search":{"type":"integer","minimum":3,"maximum":20,"default":8},
                    "force_refresh":{"type":"boolean","default":false},
                    "freshness":{"enum":["day","week","month","year"]},
                    "category":{"type":"string"},
                    "engines":{"type":"array","items":{"type":"string"}},
                    "include_domains":{"type":"array","items":{"type":"string"}},
                    "exclude_domains":{"type":"array","items":{"type":"string"}}
                },
                "required":["question"]
            }),
            Some("read_only"),
            start,
        )
        .tool_with_class(
            "status",
            "Read a research job or wait up to 45s for its next progress snapshot (default 30). Keep calling until \
             terminal=true; state=partial/failed is not successful completion. Activity shows actual queries and reads.",
            json!({"type":"object","properties":{
                "id":{"type":"string"},"wait_secs":{"type":"integer","minimum":0,"maximum":45,"default":30}
            },"required":["id"]}),
            Some("read_only"),
            status,
        )
        .tool_with_class(
            "report",
            "Return a job's cited evidence dossier in markdown or json. Includes per-aspect excerpts from fetched pages, \
             provenance, unresolved gaps, failures and the query audit; unread candidates are excluded. If has_more is true, \
             call again with next_offset. It is evidence for synthesis, not permission to invent unsupported claims.",
            json!({"type":"object","properties":{
                "id":{"type":"string"},"format":{"enum":["markdown","json"],"default":"markdown"},
                "offset":{"type":"integer","minimum":0,"default":0},
                "max_bytes":{"type":"integer","minimum":1000,"maximum":20000,"default":20000}
            },"required":["id"]}),
            Some("read_only"),
            report,
        )
        .tool_with_class(
            "list",
            "List this plugin process's research jobs and their progress. Use the returned id with status/report/cancel. \
             Jobs are process-local and do not survive a plugin reload.",
            json!({"type":"object","properties":{}}),
            Some("read_only"),
            list,
        )
        .tool_with_class(
            "cancel",
            "Request cancellation of an active job. The current free-search call finishes or times out first, then the job \
             becomes cancelled and retains evidence already collected. Poll status to observe the terminal state.",
            json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}),
            Some("read_only"),
            cancel,
        )
        .hook(hooks::BEFORE_PROMPT, before_prompt)
        .panel_with("status", "Deep research", Some("search"), 5_000, panel)
        .build()
}

fn main() -> std::io::Result<()> {
    let plugin = build_plugin();
    plugin.run()?;
    store().shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_stable_and_distinguishes_deep_from_one_shot() {
        let first = before_prompt(Value::Null);
        assert_eq!(first, before_prompt(json!({"session_id":"other"})));
        let text = first.as_str().unwrap();
        assert!(text.contains("wait_secs=45"));
        assert!(text.contains("state=complete"));
        assert!(text.contains("snippets"));
        assert!(text.len() < 4_096);
    }

    #[test]
    fn tool_surface_is_stable_and_bounded() {
        let plugin = build_plugin();
        assert_eq!(plugin.manifest().name, "deep-research");
        assert_eq!(plugin.manifest().protocol, "1");
        let listed = plugin
            .handle_request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
            .unwrap();
        let listed_tools = listed["result"]["tools"].as_array().unwrap();
        let tools: Vec<_> = listed_tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(tools, ["cancel", "list", "report", "start", "status"]);
        assert!(listed_tools
            .iter()
            .all(|t| t["description"].as_str().unwrap().len() < 1_000));
        let start = listed_tools.iter().find(|t| t["name"] == "start").unwrap();
        assert_eq!(start["parameters"]["required"], json!(["question"]));
    }

    #[test]
    fn jobs_are_scoped_to_session_and_workspace() {
        let job = Job::new(
            "r".into(),
            "Rust".into(),
            vec![],
            model::ResearchOptions::default(),
            Some("session-a".into()),
            Some("/work/a".into()),
        );
        let request = |session: &str, workspace: &str| ToolRequest {
            name: "status".into(),
            arguments: json!({"id":"r"}),
            context: Some(protocol::ToolCallContext {
                session_id: session.into(),
                workspace: workspace.into(),
                ..Default::default()
            }),
        };
        assert!(visible_to(&job, &request("session-a", "/work/a")));
        assert!(!visible_to(&job, &request("session-b", "/work/a")));
        assert!(!visible_to(&job, &request("session-a", "/work/b")));
    }

    #[test]
    fn shipped_manifest_grants_process_and_network_access() {
        let manifest = include_str!("../plugin.toml");
        let permissions = manifest
            .lines()
            .find(|l| l.starts_with("permissions = "))
            .unwrap();
        assert!(permissions.contains("\"shell\""));
        assert!(permissions.contains("\"network\""));
        assert!(
            !manifest
                .lines()
                .any(|line| line.trim_start().starts_with("read_only_tools")),
            "a data-dir manifest cannot grant itself approval exemptions"
        );
    }

    #[test]
    fn declared_settings_pass_host_validation() {
        let plugin = build_plugin();
        let settings = &plugin.manifest().settings;
        assert_eq!(settings.len(), 6);
        sdk::validate_settings(settings).expect("declared settings must be valid");
    }
}
