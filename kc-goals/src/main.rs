//! `kc-goals` - a goal an agent cannot quietly drop.
//!
//! karucode ends a run when the model returns a turn with no tool calls, and
//! nothing in the harness can veto that: a model that decides it is done at 40 %
//! is, as far as the runtime is concerned, done. This plugin is the outside party
//! that disagrees.
//!
//! Three layers, cheapest first:
//!
//! 1. **Orientation** ([`render::PROMPT_BLOCK`]) - a byte-stable system-prompt
//!    block, emitted only while a goal is open, saying that stopping early does
//!    not end the goal. It costs nothing per call because it never changes.
//! 2. **A definition of done** ([`model`]) - criteria that need evidence, a
//!    journal that survives a context window, and a budget. `goal_complete`
//!    refuses while anything is unmet, so "done" is a claim with receipts.
//! 3. **Continuation** ([`worker`] + [`engine`]) - `on_run_end` queues an intent,
//!    the engine decides from the rulebook, and the decision is delivered as an
//!    ordinary `POST /api/sessions/:id/messages`. A reconcile tick makes the goal
//!    survive a dropped hook, a plugin restart, a server restart, or a POST that
//!    failed.
//!
//! Why a plugin and not a core change: the runtime has no hook that may append to
//! the conversation or veto a stop. It does have `on_run_end`, an HTTP API that
//! accepts a message from any client, and this is exactly the "extension, not
//! fork" shape the plugin API exists for. The full reasoning, the failure modes
//! and the roadmap are in `PLAN.md`; the operator-facing half is `README.md`.

mod check;
mod config;
mod engine;
mod http;
mod model;
// Vendored from `karucode plugin init`: it ships the whole SDK surface, most of
// which a given plugin does not call. Refreshed with `karucode plugin sdk .`,
// so it is not ours to prune.
#[allow(dead_code)]
mod protocol;
mod render;
#[allow(dead_code)]
mod sdk;
mod store;
mod tools;
mod worker;

use std::sync::Arc;

use serde_json::{json, Value};

use crate::sdk::{hooks, PanelRequest, PluginBuilder};
use crate::worker::{App, Intent};

fn main() -> std::io::Result<()> {
    let settings = config::Settings::from_settings(&config::settings());
    let app = App::new(settings.clone());
    if let Some(error) = app.error() {
        eprintln!("kc-goals: the engine cannot reach the server: {error}");
    }

    // The engine only starts when it can do something. `enabled = false` leaves
    // the process serving the tools and the panel, which is how an operator
    // inspects or parks the plugin without uninstalling it.
    if settings.enabled && settings.auto_continue {
        worker::spawn(Arc::clone(&app));
    }

    let hook_app = Arc::clone(&app);
    let run_app = Arc::clone(&app);
    let panel_app = Arc::clone(&app);

    let builder = PluginBuilder::new("kc-goals", env!("CARGO_PKG_VERSION"))
        .description(
            "A goal, a definition of done, and an engine that resumes a stopped run until the \
             goal is met, the budget is spent, or a human stops it.",
        )
        // Goals live in the workspace; the engine talks to the local server.
        .permission(sdk::permissions::FILES_READ)
        .permission(sdk::permissions::FILES_WRITE)
        .permission(sdk::permissions::NETWORK)
        // Only used by `goal_check`, and only when the operator sets
        // `verify_commands = true`. Declared because it is true whenever the
        // feature is on: a criterion's command runs with this plugin's own OS
        // rights, which are the server user's.
        .permission(sdk::permissions::SHELL)
        .notifications()
        // Layer 1: the orientation block. Byte-stable, and only while a goal is
        // open, so a session without one pays nothing and a session with one pays
        // for exactly two cache invalidations (start and end).
        .hook(hooks::BEFORE_PROMPT, move |payload| {
            prompt_block(&hook_app, &payload)
        })
        // Trigger: record an intent and wake the engine. No HTTP here - the hook
        // runs inside the run's teardown, and it must return inside the host's
        // hook budget even if the server is wedged.
        .hook(hooks::ON_RUN_END, move |payload| {
            let session = payload
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let run_id = payload
                .get("run_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if session.is_empty() || run_id.is_empty() {
                return Value::Null;
            }
            run_app.wake_with(Intent {
                session_id: session,
                run_id,
                status,
            });
            Value::Null
        })
        .panel_with(
            "goals",
            "Goals",
            Some("check"),
            5_000,
            move |req: PanelRequest| panel(&panel_app, req),
        );

    tools::register(builder, Arc::clone(&app)).run()
}

/// The `before_prompt` answer: the block while this workspace has an open goal,
/// `null` otherwise.
fn prompt_block(app: &Arc<App>, payload: &Value) -> Value {
    if !app.cfg.enabled || !app.cfg.prompt {
        return Value::Null;
    }
    let workspace = payload
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or("");
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if workspace.is_empty() {
        return Value::Null;
    }
    // This runs on the plugin's request thread before every model call, so it is
    // one small file read and nothing else.
    if !session.is_empty() {
        app.store.remember(session, workspace);
    }
    let has_open_goal = app.store.list(workspace).iter().any(|(_, f)| f.has_open_goal());
    if has_open_goal {
        Value::String(render::PROMPT_BLOCK.to_string())
    } else {
        Value::Null
    }
}

fn panel(app: &Arc<App>, req: PanelRequest) -> Value {
    let now = model::now_ms();
    if req.workspace.is_empty() {
        return json!({
            "error": "no workspace for this panel read; open the tab from a project",
            "goals": [],
        });
    }
    // The host resolves `?project=` and falls back to the *default* workspace, so a
    // tab opened without it would show the wrong project's goals. The session knows
    // where it lives; trust it over the query string.
    let workspace = worker::session_workspace(app, &req.session_id)
        .filter(|own| !own.is_empty())
        .unwrap_or_else(|| req.workspace.clone());
    let files = app.store.list(&workspace);
    let mut data = render::panel(
        &req.session_id,
        &workspace,
        &files,
        now,
        &app.cfg,
        &app.log_snapshot(),
    );
    if let Some(object) = data.as_object_mut() {
        object.insert(
            "error".into(),
            match app.error() {
                Some(error) => Value::String(error),
                None => Value::Null,
            },
        );
        object.insert(
            "requested_workspace".into(),
            Value::String(req.workspace.clone()),
        );
        object.insert(
            "goals_dir".into(),
            Value::String(store::Store::goals_dir(&workspace).display().to_string()),
        );
        object.insert(
            "state_dir".into(),
            Value::String(app.store.state_dir().display().to_string()),
        );
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_prompt_block_is_absent_without_an_open_goal() {
        let root = model::temp_dir("kc-goals-main");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let cfg = config::Settings {
            state_dir: root.join("state").to_string_lossy().to_string(),
            ..config::Settings::default()
        };
        let app = worker::App::with_transport(cfg, Box::new(NoServer));

        let payload = json!({"session_id": "s_1", "workspace": workspace.to_string_lossy()});
        assert_eq!(prompt_block(&app, &payload), Value::Null);

        // With a goal, the block appears and is byte-identical on every call.
        app.store
            .update(&workspace.to_string_lossy(), "s_1", |file| {
                file.goals.push(model::Goal::new(
                    "g1",
                    "t",
                    "s",
                    vec![model::Criterion::new("c1", "x")],
                    model::Budget::default(),
                    model::Source::User,
                    1_000,
                ));
                file.refresh_active();
                Ok(())
            })
            .unwrap();
        let first = prompt_block(&app, &payload);
        let second = prompt_block(&app, &payload);
        assert_eq!(first, second);
        assert_eq!(first, Value::String(render::PROMPT_BLOCK.to_string()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_prompt_block_can_be_turned_off() {
        let root = model::temp_dir("kc-goals-main");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = config::Settings {
            prompt: false,
            state_dir: root.join("state").to_string_lossy().to_string(),
            ..config::Settings::default()
        };
        let app = worker::App::with_transport(cfg, Box::new(NoServer));
        let payload = json!({"session_id": "s_1", "workspace": root.to_string_lossy()});
        assert_eq!(prompt_block(&app, &payload), Value::Null);
        let _ = std::fs::remove_dir_all(root);
    }

    struct NoServer;
    impl worker::Transport for NoServer {
        fn session(&self, _id: &str) -> Result<worker::SessionView, String> {
            Err("no server".into())
        }
        fn post(
            &self,
            _id: &str,
            _text: &str,
            _mode: worker::PostMode,
        ) -> Result<worker::Post, String> {
            Err("no server".into())
        }
    }

    #[test]
    fn the_panel_says_so_when_it_has_no_workspace() {
        let root = model::temp_dir("kc-goals-main");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = config::Settings {
            state_dir: root.join("state").to_string_lossy().to_string(),
            ..config::Settings::default()
        };
        let app = worker::App::with_transport(cfg, Box::new(NoServer));
        let data = panel(
            &app,
            PanelRequest {
                panel: "goals".into(),
                session_id: "s_1".into(),
                project: None,
                workspace: String::new(),
            },
        );
        assert!(data["error"].as_str().unwrap().contains("no workspace"));
        let _ = std::fs::remove_dir_all(root);
    }

    struct OwnWorkspace;
    impl worker::Transport for OwnWorkspace {
        fn session(&self, _id: &str) -> Result<worker::SessionView, String> {
            Ok(worker::SessionView {
                exists: true,
                workspace: "/own/ws".into(),
                ..worker::SessionView::default()
            })
        }
        fn post(
            &self,
            _id: &str,
            _text: &str,
            _mode: worker::PostMode,
        ) -> Result<worker::Post, String> {
            Err("read only in this test".into())
        }
    }

    #[test]
    fn the_panel_trusts_the_sessions_workspace_over_the_query_string() {
        // The host falls back to the default workspace when `?project=` is absent,
        // so a tab opened from the session list would otherwise show another
        // project's goals.
        let root = model::temp_dir("kc-goals-panel3");
        let cfg = config::Settings {
            state_dir: root.join("state").to_string_lossy().to_string(),
            ..config::Settings::default()
        };
        let app = worker::App::with_transport(cfg, Box::new(OwnWorkspace));
        let data = panel(
            &app,
            PanelRequest {
                panel: "goals".into(),
                session_id: "s_1".into(),
                project: None,
                workspace: "/default/workspace".into(),
            },
        );
        assert_eq!(data["workspace"], "/own/ws");
        assert_eq!(data["requested_workspace"], "/default/workspace");
        assert_eq!(data["goals_dir"], "/own/ws/.karucode/goals");
        assert!(data["error"].is_null());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_panel_lists_the_goal_files_it_can_find() {
        let root = model::temp_dir("kc-goals-main");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let cfg = config::Settings {
            state_dir: root.join("state").to_string_lossy().to_string(),
            ..config::Settings::default()
        };
        let app = worker::App::with_transport(cfg, Box::new(NoServer));
        let ws = workspace.to_string_lossy().to_string();
        app.store.update(&ws, "s_1", |file| {
            file.goals.push(model::Goal::new(
                "g1",
                "Ship it",
                "Ship it",
                vec![model::Criterion::new("c1", "builds")],
                model::Budget::default(),
                model::Source::User,
                1_000,
            ));
            file.refresh_active();
            Ok(())
        })
        .unwrap();
        let data = panel(
            &app,
            PanelRequest {
                panel: "goals".into(),
                session_id: "s_1".into(),
                project: None,
                workspace: ws.clone(),
            },
        );
        assert_eq!(data["goals"][0]["goal"], "g1");
        assert_eq!(data["goals_dir"].as_str().unwrap(), &format!("{ws}/.karucode/goals"));
        assert!(data["error"].is_null());
        let _ = std::fs::remove_dir_all(root);
    }
}
