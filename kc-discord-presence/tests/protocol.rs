//! End to end: the real binary, the real JSON-RPC protocol, a fake Discord.
//!
//! Nothing here is mocked at the plugin's boundary - the test spawns
//! `target/.../discord-presence`, speaks the same newline-delimited JSON the host
//! speaks, points the plugin at a temporary karucode database and a scriptable
//! stand-in for `discord.com`, and then reads back the requests Discord would
//! have received.
//!
//! What it proves, in order: the manifest and tools are advertised; an
//! unconfigured plugin says so instead of failing; `discord_configure` validates
//! and publishes the first card; the card carries real state from the database;
//! `discord_publish` edits that same message; a hook's tool result becomes the
//! card's activity; a notification becomes its own message; the panel and the
//! button answer; and shutting down leaves an honest offline card.

mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};
use support::{FakeDiscord, FakeDiscordIpc};

fn tempdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "kc-discord-presence-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// The columns this plugin reads, with one live run in `karucode-plugins`.
fn fixture_db(dir: &Path) {
    let conn = rusqlite::Connection::open(dir.join("karucode.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL DEFAULT '');
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', workspace TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, project_id TEXT, model TEXT,
            control INTEGER NOT NULL DEFAULT 0, ephemeral INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE runs (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, status TEXT NOT NULL,
            model TEXT NOT NULL DEFAULT '', prompt TEXT NOT NULL DEFAULT '', error TEXT,
            started_at INTEGER NOT NULL, finished_at INTEGER, instance_id TEXT NOT NULL DEFAULT '');
        CREATE TABLE agents (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, parent_run_id TEXT NOT NULL DEFAULT '',
            parent_agent_id TEXT, name TEXT NOT NULL DEFAULT '', role TEXT NOT NULL DEFAULT '',
            task TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
            iterations INTEGER NOT NULL DEFAULT 0, summary TEXT NOT NULL DEFAULT '', error TEXT,
            owned_paths TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
            finished_at INTEGER, instance_id TEXT NOT NULL DEFAULT '',
            background INTEGER NOT NULL DEFAULT 0, note TEXT NOT NULL DEFAULT '', progress REAL);
        CREATE TABLE approvals (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, run_id TEXT NOT NULL, agent_id TEXT,
            kind TEXT NOT NULL, title TEXT NOT NULL DEFAULT '', detail TEXT NOT NULL DEFAULT '',
            payload TEXT, status TEXT NOT NULL, decision TEXT, created_at INTEGER NOT NULL,
            resolved_at INTEGER);
        INSERT INTO projects (id, name, path) VALUES ('p_1', 'karucode-plugins', '/w');
        INSERT INTO sessions (id, title, workspace, created_at, updated_at, project_id, model)
            VALUES ('s_live', 'build the discord plugin', '/w', 1000, 2000, 'p_1', 'deepseek-v4.1-flash');
        INSERT INTO runs (id, session_id, status, model, prompt, started_at)
            VALUES ('r_1', 's_live', 'running', 'deepseek-v4.1-flash', 'build the discord plugin', 5000);
        "#,
    )
    .unwrap();
}

struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
    rpc_settings: Value,
}

impl Plugin {
    fn start(settings: &Value) -> Self {
        Self::start_with_env_and_rpc(settings, &[], json!({}))
    }

    fn start_with_env(settings: &Value, env: &[(&str, &str)]) -> Self {
        Self::start_with_env_and_rpc(settings, env, json!({}))
    }

    fn start_with_env_and_rpc(settings: &Value, env: &[(&str, &str)], rpc_settings: Value) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_discord-presence"));
        command.env("KARUCODE_PLUGIN_SETTINGS", settings.to_string());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the plugin binary");
        let stdout = child.stdout.take().expect("stdout");
        let stdin = child.stdin.take().expect("stdin");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
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
            stdin: Some(stdin),
            lines: rx,
            next_id: 1,
            rpc_settings,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string();
        {
            let stdin = self.stdin.as_mut().expect("stdin is open");
            writeln!(stdin, "{line}").expect("write a request");
            stdin.flush().expect("flush");
        }
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("no answer to `{method}`"));
            let value: Value = serde_json::from_str(&line).expect("the plugin wrote JSON");
            if value.get("method").and_then(Value::as_str) == Some("plugin/settings/get") {
                if let Some(req_id) = value.get("id") {
                    let reply = json!({"jsonrpc": "2.0", "id": req_id, "result": {"settings": self.rpc_settings}})
                        .to_string();
                    let stdin = self.stdin.as_mut().expect("stdin is open");
                    writeln!(stdin, "{reply}").expect("write settings reply");
                    stdin.flush().expect("flush");
                }
                continue;
            }
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.request(method, params);
        assert!(
            response.get("error").is_none(),
            "`{method}` failed: {response}"
        );
        response.get("result").cloned().unwrap_or(Value::Null)
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.ok("tools/call", json!({"name": name, "arguments": arguments}))
    }

    /// Close stdin the way the host does when it stops a plugin, then wait for
    /// the process to leave.
    fn stop(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(_) => break,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                None => {
                    let _ = self.child.kill();
                    panic!("the plugin did not exit after stdin closed");
                }
            }
        }
    }
}

#[test]
fn the_whole_plugin_works_against_a_fake_discord() {
    let fake = FakeDiscord::start();
    let dir = tempdir("flow");
    fixture_db(&dir);
    let settings = json!({"api_base": fake.base(), "data_dir": dir.display().to_string()});
    let mut plugin = Plugin::start(&settings);

    // 1. The handshake advertises what the host needs to expose.
    let manifest = plugin.ok("initialize", json!({"protocol": "1"}));
    assert_eq!(manifest["name"], "discord-presence");
    assert_eq!(manifest["protocol"], "1");
    let panels = manifest["panels"].as_array().expect("panels");
    assert_eq!(panels[0]["id"], "board");
    let ui = manifest["ui"].as_array().expect("ui");
    assert_eq!(ui[0]["id"], "push");
    assert_eq!(ui[0]["slot"], "inspector.header");
    let hooks = manifest["hooks"].as_array().expect("hooks");
    for hook in ["on_tool_result", "on_run_end", "on_notify"] {
        assert!(hooks.iter().any(|h| h == hook), "missing hook {hook}");
    }
    let tools = plugin.ok("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "discord_configure",
            "discord_notify",
            "discord_publish",
            "discord_status"
        ]
    );

    // 2. Before setup it says what to do, and sends nothing.
    let status = plugin.tool("discord_status", json!({}));
    assert_eq!(status["ok"], true);
    let status: Value = serde_json::from_str(status["content"].as_str().unwrap()).unwrap();
    assert_eq!(status["configured"], false);
    assert_eq!(status["presence"]["enabled"], false);
    assert_eq!(status["board"]["enabled"], false);
    assert_eq!(fake.count(), 0);

    // 3. Setup: the pasted URL is validated and the first card is published.
    let configured = plugin.tool(
        "discord_configure",
        json!({"webhook_url": fake.url("123", "tok"), "refresh_secs": 30}),
    );
    assert_eq!(configured["ok"], true, "{configured}");
    let configured: Value =
        serde_json::from_str(configured["content"].as_str().unwrap()).unwrap();
    assert_eq!(configured["configured"], true);
    let card = &configured["card"];
    assert_eq!(card["connected"], true);
    assert_eq!(card["message_id"], "m_default");
    assert_eq!(card["refresh_secs"], 30);
    assert!(!card["webhook"].as_str().unwrap().contains("tok"));

    let first = fake.last().expect("the first card");
    assert_eq!(first.method, "POST");
    assert!(first.query.contains("wait=true"));
    let embed = &first.body.as_ref().unwrap()["embeds"][0];
    assert!(
        embed["title"].as_str().unwrap().contains("karucode-plugins"),
        "the card must name the project from the database: {embed}"
    );
    assert!(embed["description"]
        .as_str()
        .unwrap()
        .contains("build the discord plugin"));
    assert_eq!(first.body.as_ref().unwrap()["allowed_mentions"]["parse"], json!([]));

    // 4. A live tool call becomes the card's activity, and publishing edits the
    //    message that already exists rather than posting another.
    plugin.ok(
        "hooks/run",
        json!({"hook": "on_tool_result", "payload": {
            "session_id": "s_live", "tool": "plugin__kg__raw", "ok": true
        }}),
    );
    let published = plugin.tool("discord_publish", json!({}));
    assert_eq!(published["ok"], true, "{published}");
    let published: Value =
        serde_json::from_str(published["content"].as_str().unwrap()).unwrap();
    assert_eq!(published["card"]["message_id"], "m_default");
    let edit = fake.last().unwrap();
    assert_eq!(edit.method, "PATCH");
    assert!(edit.route().ends_with("/messages/m_default"));
    let fields = edit.body.as_ref().unwrap()["embeds"][0]["fields"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        fields
            .iter()
            .any(|f| f["name"] == "Activity"
                && f["value"].as_str().unwrap().contains("plugin__kg__raw")),
        "{fields:?}"
    );

    // 5. A notification the operator asked for becomes its own message.
    let before = fake.count();
    plugin.ok(
        "hooks/run",
        json!({"hook": "on_notify", "payload": {
            "id": "approval:a_1", "kind": "needs_input", "title": "Approval needed",
            "body": "write_file docs/plan.md", "session_id": "s_live",
            "session_title": "build the discord plugin", "project_id": "p_1",
            "source": "runtime"
        }}),
    );
    assert!(
        fake.wait_for(before + 1, Duration::from_secs(10)),
        "the notification was never posted"
    );
    let notice = fake.last().unwrap();
    assert_eq!(notice.method, "POST");
    let embed = &notice.body.as_ref().unwrap()["embeds"][0];
    assert!(embed["title"].as_str().unwrap().contains("Approval needed"));
    assert_eq!(embed["color"], json!(0xf1c40f));

    // 6. An ad-hoc message from the agent.
    let before = fake.count();
    let notified = plugin.tool(
        "discord_notify",
        json!({"title": "Tests pass", "body": "85 unit tests, 1 end to end", "kind": "run_finished"}),
    );
    assert_eq!(notified["ok"], true, "{notified}");
    assert!(fake.wait_for(before + 1, Duration::from_secs(10)));
    let ad_hoc = fake.last().unwrap();
    assert!(ad_hoc.body.as_ref().unwrap()["embeds"][0]["title"]
        .as_str()
        .unwrap()
        .contains("Tests pass"));

    // 7. The inspector panel and the header button.
    let panel = plugin.ok(
        "panels/read",
        json!({"panel": "board", "session_id": "s_live", "project": "p_1", "workspace": "/w"}),
    );
    assert_eq!(panel["title"], "Discord");
    let data = &panel["data"];
    assert_eq!(data["view"], "stack");
    assert!(data["views"].as_array().unwrap().len() >= 2);
    assert!(data.to_string().contains("karucode-plugins"));
    let action = plugin.ok(
        "ui/action",
        json!({
            "action": "push", "contribution": {"id": "push", "kind": "button", "label": "Push to Discord"},
            "session_id": "s_live", "workspace": "/w"
        }),
    );
    assert_eq!(action["ok"], true, "{action}");
    assert!(action["refresh"].as_array().unwrap().iter().any(|p| p == "board"));

    // 8. Stopping leaves an honest card rather than a stale "running".
    let before = fake.count();
    plugin.stop();
    assert!(fake.wait_for(before + 1, Duration::from_secs(10)));
    let offline = fake.last().unwrap();
    assert_eq!(offline.method, "PATCH");
    assert!(offline.body.as_ref().unwrap()["embeds"][0]["title"]
        .as_str()
        .unwrap()
        .contains("offline"));
}

#[test]
fn the_profile_presence_reaches_a_discord_client() {
    // The surface this plugin exists for: "Playing <app>" on the operator's
    // profile, with the project and session on the two detail lines. No webhook
    // is configured here at all - the presence alone is a complete setup.
    let ipc = FakeDiscordIpc::start();
    let dir = tempdir("presence");
    fixture_db(&dir);
    let settings = json!({"data_dir": dir.display().to_string()});
    let runtime = ipc.runtime_dir().to_str().unwrap().to_string();
    let mut plugin = Plugin::start_with_env(&settings, &[("XDG_RUNTIME_DIR", &runtime)]);

    let configured = plugin.tool(
        "discord_configure",
        json!({"application_id": "123456789012345678"}),
    );
    assert_eq!(configured["ok"], true, "{configured}");
    let configured: Value =
        serde_json::from_str(configured["content"].as_str().unwrap()).unwrap();
    assert_eq!(configured["presence"]["connected"], true, "{configured}");
    assert_eq!(
        configured["presence"]["activity"]["details"],
        "karucode-plugins"
    );
    assert!(configured["card"].is_null(), "no webhook was configured");

    // The client received the handshake and the activity, under the app id.
    assert!(
        ipc.wait_for(2, Duration::from_secs(10)),
        "the presence never reached the client"
    );
    let frames = ipc.frames();
    assert_eq!(frames[0].opcode, 0);
    assert_eq!(frames[0].body["client_id"], "123456789012345678");
    assert_eq!(frames[0].body["v"], 1);
    let activity = frames
        .iter()
        .find(|f| f.opcode == 1)
        .expect("an activity frame");
    assert_eq!(activity.body["cmd"], "SET_ACTIVITY");
    assert_eq!(
        activity.body["args"]["activity"]["details"],
        "karucode-plugins"
    );
    // The fixture has no pending approval, so the profile line is the session.
    assert_eq!(
        activity.body["args"]["activity"]["state"],
        "build the discord plugin"
    );
    assert!(activity.body["args"]["activity"]["timestamps"]["start"]
        .as_i64()
        .unwrap()
        > 0);
    assert!(activity.body["args"]["pid"].as_u64().unwrap() > 0);

    // Status and panel agree, and the panel leads with the presence.
    let status = plugin.tool("discord_status", json!({}));
    let status: Value = serde_json::from_str(status["content"].as_str().unwrap()).unwrap();
    assert_eq!(status["presence"]["connected"], true);
    assert_eq!(status["presence"]["application_id"], "123456789012345678");
    assert_eq!(status["configured"], true);
    let panel = plugin.ok(
        "panels/read",
        json!({"panel": "board", "session_id": "s_live", "workspace": "/w"}),
    );
    let data = &panel["data"];
    assert_eq!(data["views"][0]["label"], "Presence");
    assert_eq!(data["views"][0]["value"], "live");
    assert!(data["views"][0]["detail"].as_str().unwrap().contains("discord-ipc-0"));

    // Stopping takes the presence down instead of leaving "Playing" stuck.
    let before = ipc.count();
    plugin.stop();
    assert!(ipc.wait_for(before + 1, Duration::from_secs(10)));
    let frames = ipc.frames();
    let last_activity = frames.iter().rfind(|f| f.opcode == 1)
        .expect("a final activity frame");
    assert_eq!(last_activity.body["args"]["activity"], json!(null));
    assert!(
        frames.iter().any(|f| f.opcode == 2),
        "the connection should be closed with the protocol's goodbye"
    );
}

#[test]
fn a_bad_webhook_is_refused_and_nothing_is_written() {
    let fake = FakeDiscord::start();
    let dir = tempdir("bad");
    fixture_db(&dir);
    let settings = json!({"api_base": fake.base(), "data_dir": dir.display().to_string()});
    let mut plugin = Plugin::start(&settings);

    let refused = plugin.tool("discord_configure", json!({"webhook_url": "https://example.test/x"}));
    assert_eq!(refused["ok"], false);
    assert!(refused["content"].as_str().unwrap().contains("webhooks"));
    assert_eq!(fake.count(), 0);
    assert!(!dir.join("discord-presence/state.json").exists());

    // A URL Discord rejects is a refusal too, and it is Discord's own words.
    fake.reply(401, json!({"message": "Invalid Webhook Token", "code": 50027}));
    let refused = plugin.tool("discord_configure", json!({"webhook_url": fake.url("1", "bad")}));
    assert_eq!(refused["ok"], false);
    assert!(refused["content"].as_str().unwrap().contains("Invalid Webhook Token"));
    assert!(!dir.join("discord-presence/state.json").exists());
    plugin.stop();
}

#[test]
fn a_restart_reuses_the_card_instead_of_posting_a_second_one() {
    let fake = FakeDiscord::start();
    let dir = tempdir("restart");
    fixture_db(&dir);
    let settings = json!({"api_base": fake.base(), "data_dir": dir.display().to_string()});

    {
        let mut plugin = Plugin::start(&settings);
        plugin.tool("discord_configure", json!({"webhook_url": fake.url("9", "tok")}));
        plugin.stop();
    }
    let posts_before = fake
        .requests()
        .iter()
        .filter(|r| r.method == "POST")
        .count();
    assert_eq!(posts_before, 1);

    let mut plugin = Plugin::start(&settings);
    let status = plugin.tool("discord_status", json!({}));
    let status: Value = serde_json::from_str(status["content"].as_str().unwrap()).unwrap();
    assert_eq!(status["configured"], true, "{status}");
    assert_eq!(status["board"]["message_id"], "m_default");
    let published = plugin.tool("discord_publish", json!({}));
    assert_eq!(published["ok"], true, "{published}");
    assert_eq!(fake.last().unwrap().method, "PATCH");
    assert_eq!(
        fake.requests().iter().filter(|r| r.method == "POST").count(),
        1,
        "a restart must not leave a second card behind"
    );
    plugin.stop();
}

#[test]
fn plugin_settings_get_overrides_env_settings() {
    let dir = tempdir("rpc-settings");
    fixture_db(&dir);
    let env_settings = json!({
        "data_dir": dir.display().to_string(),
        "refresh_secs": 30,
        "username": "from-env"
    });
    let rpc_settings = json!({
        "refresh_secs": 45.0,
        "username": "from-ui",
        "max_sessions": 5
    });
    let mut plugin = Plugin::start_with_env_and_rpc(&env_settings, &[], rpc_settings);
    let status = plugin.tool("discord_status", json!({}));
    let status: Value = serde_json::from_str(status["content"].as_str().unwrap()).unwrap();
    assert_eq!(status["settings"]["refresh_secs"], 45);
    assert_eq!(status["settings"]["username"], "from-ui");
    assert_eq!(status["settings"]["max_sessions"], 5);
    plugin.stop();
}
