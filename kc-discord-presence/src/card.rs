//! Snapshot -> Discord message payload.
//!
//! Everything here is pure: no network, no clock reads, no locks. That is what
//! makes the card testable, and the card is the part a person actually looks at.
//!
//! Discord's limits are hard limits, not hints: a field over 1024 characters, a
//! title over 256 or a whole embed over 6000 is a `400` and the card never
//! appears. Every string that comes from karucode (a session title, a prompt, a
//! tool result) is therefore clamped on the way in, and [`Card::embed`] re-checks
//! the total budget before it is sent.

use crate::config::Effective;
use crate::karucode::Snapshot;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Discord's per-field ceilings.
pub const MAX_TITLE: usize = 256;
pub const MAX_DESCRIPTION: usize = 4096;
pub const MAX_FIELD_NAME: usize = 256;
pub const MAX_FIELD_VALUE: usize = 1024;
pub const MAX_FIELDS: usize = 25;
pub const MAX_FOOTER: usize = 2048;
/// Discord's total embed budget is 6000; keep a little headroom.
pub const MAX_EMBED_TOTAL: usize = 5800;
/// How much of a prompt the card quotes.
pub const MAX_PROMPT: usize = 280;

pub const COLOR_RUNNING: u32 = 0x2ecc71;
pub const COLOR_APPROVAL: u32 = 0xf1c40f;
pub const COLOR_FAILED: u32 = 0xe74c3c;
pub const COLOR_IDLE: u32 = 0x95a5a6;
pub const COLOR_INFO: u32 = 0x5865f2;

/// The last tool call this plugin saw for a session, from `on_tool_result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub tool: String,
    pub at: i64,
    pub ok: bool,
}

/// A notification the runtime emitted, forwarded by `on_notify`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notice {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub session_id: String,
    pub session_title: String,
    pub project_id: String,
    pub source: String,
    pub at: i64,
}

impl Notice {
    /// Parse the host's payload defensively: an older or newer karucode may send
    /// fewer or more fields, and a missing one must not lose the notice.
    pub fn parse(payload: &Value, now: i64) -> Option<Self> {
        let kind = payload.get("kind").and_then(Value::as_str)?.to_string();
        let title = payload
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("karucode")
            .to_string();
        Some(Self {
            id: payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            kind,
            title,
            body: payload
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            session_id: payload
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            session_title: payload
                .get("session_title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            project_id: payload
                .get("project_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            source: payload
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("runtime")
                .to_string(),
            at: now,
        })
    }
}

/// One rendered card, before it becomes JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub state: String,
    pub title: String,
    pub description: String,
    pub color: u32,
    pub fields: Vec<(String, String)>,
    pub footer: String,
    pub timestamp: String,
    pub focus_session: Option<String>,
    pub focus_project: Option<String>,
    pub focus_title: Option<String>,
}

impl Card {
    /// The Discord embed object, with the total budget enforced.
    pub fn embed(&self) -> Value {
        let mut fields: Vec<(String, String)> = self
            .fields
            .iter()
            .filter(|(name, value)| !name.trim().is_empty() && !value.trim().is_empty())
            .take(MAX_FIELDS)
            .map(|(name, value)| {
                (
                    clamp(name, MAX_FIELD_NAME),
                    clamp(value, MAX_FIELD_VALUE),
                )
            })
            .collect();
        let mut title = clamp(&self.title, MAX_TITLE);
        let mut description = clamp(&self.description, MAX_DESCRIPTION);
        let footer = clamp(&self.footer, MAX_FOOTER);

        // Total budget: drop the least important field first (the tail is
        // ordered most- to least-important by `render`), then shorten the
        // description, then the title.
        loop {
            let total = title.chars().count()
                + description.chars().count()
                + footer.chars().count()
                + fields
                    .iter()
                    .map(|(n, v)| n.chars().count() + v.chars().count())
                    .sum::<usize>();
            if total <= MAX_EMBED_TOTAL {
                break;
            }
            if fields.len() > 1 {
                fields.pop();
            } else if description.chars().count() > 200 {
                description = clamp(&description, description.chars().count() - 200);
            } else if title.chars().count() > 40 {
                title = clamp(&title, title.chars().count() - 40);
            } else {
                break;
            }
        }

        let mut embed = json!({
            "title": title,
            "description": description,
            "color": self.color,
            "footer": { "text": footer },
            "timestamp": self.timestamp,
        });
        if !fields.is_empty() {
            embed["fields"] = Value::Array(
                fields
                    .into_iter()
                    .map(|(name, value)| json!({ "name": name, "value": value, "inline": false }))
                    .collect(),
            );
        }
        embed
    }

    /// The whole message body: who it appears as, and the embed.
    pub fn payload(&self, cfg: &Effective) -> Value {
        let mut payload = json!({
            "username": clamp(&cfg.username, 80),
            "embeds": [self.embed()],
            // A session title containing `@everyone` must not ping the channel.
            "allowed_mentions": { "parse": [] },
        });
        if let Some(avatar) = cfg.avatar_url.as_deref() {
            payload["avatar_url"] = json!(avatar);
        }
        payload
    }

    /// A compact view for `discord_status` and the panel: never the whole card.
    pub fn summary(&self) -> Value {
        json!({
            "state": self.state,
            "title": self.title,
            "session": self.focus_session,
            "project": self.focus_project,
            "session_title": self.focus_title,
            "fields": self.fields.len(),
        })
    }
}

/// Render the live card.
pub fn render(
    snap: &Snapshot,
    activity: &BTreeMap<String, Activity>,
    cfg: &Effective,
    now: i64,
) -> Card {
    let mut fields: Vec<(String, String)> = Vec::new();
    let footer_session;
    let mut state = "idle".to_string();
    let mut color = COLOR_IDLE;
    let title;
    let description;
    let mut focus_session = None;
    let mut focus_project = None;
    let mut focus_title = None;

    if !snap.readable {
        state = "unknown".to_string();
        title = "karucode · no data".to_string();
        description = snap
            .problems
            .first()
            .cloned()
            .unwrap_or_else(|| "karucode's database could not be read.".to_string());
        footer_session = snap.db_path.clone();
    } else if let Some((session, run)) = snap.focus() {
        focus_session = Some(session.id.clone());
        focus_project = session.project.clone();
        focus_title = Some(session.title.clone());
        let project = session
            .project
            .clone()
            .or_else(|| basename(&session.workspace))
            .unwrap_or_else(|| "no project".to_string());
        title = format!("karucode · {}", clamp(&project, 200));
        footer_session = session.id.clone();

        if let Some(run) = run {
            state = if snap.approvals.pending > 0 {
                "approval".to_string()
            } else {
                "running".to_string()
            };
            color = if snap.approvals.pending > 0 {
                COLOR_APPROVAL
            } else {
                COLOR_RUNNING
            };
            let elapsed = human_span(now.saturating_sub(run.started_at));
            let mut desc = format!("**{}**", clamp(&session.title, 300));
            let prompt = one_line(&run.prompt);
            if !prompt.is_empty() {
                desc.push_str(&format!("\n> {}", clamp(&prompt, MAX_PROMPT)));
            }
            description = desc;
            fields.push((
                "Status".to_string(),
                if snap.approvals.pending > 0 {
                    format!("Running · {elapsed} · waiting for approval")
                } else {
                    format!("Running · {elapsed}")
                },
            ));
            let model = if run.model.trim().is_empty() {
                session.model.clone()
            } else {
                Some(run.model.clone())
            };
            if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
                fields.push(("Model".to_string(), clamp(&model, 120)));
            }
            if let Some(activity) = activity.get(&session.id) {
                let age = human_span(now.saturating_sub(activity.at));
                fields.push((
                    "Activity".to_string(),
                    format!(
                        "`{}`{} · {} ago",
                        clamp(&activity.tool, 120),
                        if activity.ok { "" } else { " (failed)" },
                        age
                    ),
                ));
            }
            if session.control {
                fields.push((
                    "Kind".to_string(),
                    "control chat (no project)".to_string(),
                ));
            }
        } else {
            let age = human_span(now.saturating_sub(session.updated_at));
            description = format!(
                "Nothing running.\n**Last session** · {}\n> last activity {age} ago",
                clamp(&session.title, 300)
            );
            fields.push(("Status".to_string(), format!("Idle · last activity {age} ago")));
            if let Some(model) = session.model.clone().filter(|m| !m.trim().is_empty()) {
                fields.push(("Model".to_string(), clamp(&model, 120)));
            }
            if snap.last_failed(now) {
                color = COLOR_FAILED;
                state = "failed".to_string();
            }
        }
    } else {
        title = "karucode · idle".to_string();
        description = "No sessions yet.".to_string();
        footer_session = "karucode".to_string();
    }

    if snap.approvals.pending > 0 {
        let mut value = format!("{} pending", snap.approvals.pending);
        if !snap.approvals.newest_title.trim().is_empty() {
            value.push_str(&format!(" — {}", clamp(&snap.approvals.newest_title, 200)));
        }
        fields.push(("Approvals".to_string(), value));
        if state == "idle" {
            state = "approval".to_string();
            color = COLOR_APPROVAL;
        }
    }

    if snap.agents_running > 0 || snap.agents_paused > 0 {
        let mut value = format!("{} running", snap.agents_running);
        if snap.agents_paused > 0 {
            value.push_str(&format!(", {} paused", snap.agents_paused));
        }
        if let Some(agent) = snap.agents.first() {
            let note = one_line(&agent.note);
            if !note.is_empty() {
                value.push_str(&format!(
                    " — {}: {}",
                    clamp(&agent.name, 60),
                    clamp(&note, 200)
                ));
            }
        }
        fields.push(("Agents".to_string(), value));
    }

    let also = snap.also_running(focus_session.as_deref());
    if !also.is_empty() {
        let lines: Vec<String> = also
            .iter()
            .take(cfg.max_sessions.saturating_sub(1).max(1))
            .map(|run| {
                let project = run
                    .session
                    .project
                    .clone()
                    .or_else(|| basename(&run.session.workspace))
                    .unwrap_or_else(|| "no project".to_string());
                format!(
                    "**{}** · {} · {}",
                    clamp(&project, 80),
                    clamp(&run.session.title, 120),
                    human_span(now.saturating_sub(run.started_at))
                )
            })
            .collect();
        fields.push(("Also running".to_string(), lines.join("\n")));
    }

    if cfg.include_recent && snap.outcomes.total() > 0 {
        let mut parts = Vec::new();
        if snap.outcomes.completed > 0 {
            parts.push(format!("{} completed", snap.outcomes.completed));
        }
        if snap.outcomes.failed > 0 {
            parts.push(format!("{} failed", snap.outcomes.failed));
        }
        if snap.outcomes.limit_reached > 0 {
            parts.push(format!("{} hit a limit", snap.outcomes.limit_reached));
        }
        if snap.outcomes.cancelled > 0 {
            parts.push(format!("{} cancelled", snap.outcomes.cancelled));
        }
        fields.push((
            "Last hour".to_string(),
            format!("{} runs: {}", snap.outcomes.total(), parts.join(" · ")),
        ));
    }

    if !snap.problems.is_empty() {
        fields.push((
            "Problems".to_string(),
            snap.problems
                .iter()
                .take(2)
                .map(|p| clamp(p, 400))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }

    Card {
        state,
        title,
        description,
        color,
        fields,
        footer: format!(
            "{} · updated {}",
            clamp(&footer_session, 120),
            clock(now)
        ),
        timestamp: iso8601(now),
        focus_session,
        focus_project,
        focus_title,
    }
}

/// The card left behind when the plugin stops, so a stale "running" is not the
/// last thing anyone sees.
pub fn offline(cfg: &Effective, now: i64) -> Card {
    let _ = cfg;
    Card {
        state: "offline".to_string(),
        title: "karucode · offline".to_string(),
        description: "The discord-presence plugin stopped. This card is no longer updating."
            .to_string(),
        color: COLOR_IDLE,
        fields: Vec::new(),
        footer: format!("updated {}", clock(now)),
        timestamp: iso8601(now),
        focus_session: None,
        focus_project: None,
        focus_title: None,
    }
}

/// A notification as its own message.
pub fn notice_payload(notice: &Notice, cfg: &Effective) -> Value {
    let (emoji, color) = match notice.kind.as_str() {
        "needs_input" => ("⏸", COLOR_APPROVAL),
        "error" => ("⚠", COLOR_FAILED),
        "run_finished" => ("✅", COLOR_RUNNING),
        _ => ("ℹ", COLOR_INFO),
    };
    let mut fields: Vec<(String, String)> = Vec::new();
    if !notice.session_title.trim().is_empty() {
        fields.push((
            "Session".to_string(),
            clamp(&notice.session_title, 300),
        ));
    }
    if !notice.session_id.trim().is_empty() {
        fields.push(("Session id".to_string(), clamp(&notice.session_id, 120)));
    }
    if !notice.project_id.trim().is_empty() {
        fields.push(("Project".to_string(), clamp(&notice.project_id, 120)));
    }
    let card = Card {
        state: notice.kind.clone(),
        title: format!("{emoji} {}", clamp(&notice.title, MAX_TITLE - 4)),
        description: if notice.body.trim().is_empty() {
            notice.title.clone()
        } else {
            clamp(&notice.body, MAX_DESCRIPTION)
        },
        color,
        fields,
        footer: format!("karucode · {}", clamp(&notice.source, 80)),
        timestamp: iso8601(notice.at),
        focus_session: Some(notice.session_id.clone()).filter(|s| !s.is_empty()),
        focus_project: None,
        focus_title: Some(notice.session_title.clone()).filter(|s| !s.is_empty()),
    };
    card.payload(cfg)
}

/// Truncate on a character boundary, with an ellipsis. Never splits a word when
/// it can avoid it.
pub fn clamp(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    if let Some(pos) = out.rfind(' ') {
        if pos > (max - 1) / 2 {
            out.truncate(pos);
        }
    }
    out.push('…');
    out
}

/// Strip the small amount of markdown the card uses, for surfaces that draw
/// plain text. The inspector's `list` view renders text, not markdown, so a
/// field value of `**karucode** · …` would otherwise show its asterisks.
pub fn plain(text: &str) -> String {
    text.replace("**", "").replace('`', "").trim().to_string()
}

/// Collapse a multi-line prompt into one quotable line.
pub fn one_line(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() > MAX_PROMPT * 2 {
            break;
        }
    }
    out
}

fn basename(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `4m 12s`, `2h 05m`, `3d 4h`.
pub fn human_span(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("{minutes}m {:02}s", secs % 60);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h {:02}m", minutes % 60);
    }
    format!("{}d {}h", hours / 24, hours % 24)
}

/// `21:04:12` in UTC, for the footer.
pub fn clock(ms: i64) -> String {
    let secs = ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// `2026-11-13T21:04:12Z`, the only timestamp shape Discord accepts.
pub fn iso8601(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's civil-from-days, so this plugin needs no date crate.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::karucode::{ApprovalRef, AgentRef, RunRef, SessionRef};

    fn cfg() -> Effective {
        let mut cfg = Effective::load_from(Some(r#"{"webhook_url":"https://discord.com/api/webhooks/1/t"}"#));
        cfg.username = "karucode".to_string();
        cfg
    }

    fn session(id: &str, title: &str) -> SessionRef {
        SessionRef {
            id: id.to_string(),
            title: title.to_string(),
            project: Some("karucode-plugins".to_string()),
            workspace: "/home/karutoil/karucode-plugins".to_string(),
            model: Some("deepseek-v4.1-flash".to_string()),
            updated_at: 1_000,
            ..Default::default()
        }
    }

    fn running_snapshot() -> Snapshot {
        Snapshot {
            generated_at: 10_000,
            readable: true,
            running: vec![RunRef {
                run_id: "r_1".to_string(),
                started_at: 9_000,
                model: "deepseek-v4.1-flash".to_string(),
                prompt: "build the discord plugin".to_string(),
                session: session("s_live", "build the discord plugin"),
            }],
            recent: vec![session("s_live", "build the discord plugin")],
            ..Default::default()
        }
    }

    #[test]
    fn a_running_run_is_green_and_names_the_project_and_prompt() {
        let card = render(&running_snapshot(), &BTreeMap::new(), &cfg(), 10_000);
        assert_eq!(card.state, "running");
        assert_eq!(card.color, COLOR_RUNNING);
        assert!(card.title.contains("karucode-plugins"));
        assert!(card.description.contains("build the discord plugin"));
        assert!(card.fields.iter().any(|(n, v)| n == "Status" && v.contains("1s")));
        let embed = card.embed();
        assert_eq!(embed["color"], json!(COLOR_RUNNING));
        assert!(embed["fields"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn a_pending_approval_turns_the_card_amber_and_says_what_is_waiting() {
        let mut snap = running_snapshot();
        snap.approvals = ApprovalRef {
            pending: 1,
            newest_title: "write_file docs/plan.md".to_string(),
            newest_session: "s_live".to_string(),
        };
        let card = render(&snap, &BTreeMap::new(), &cfg(), 10_000);
        assert_eq!(card.state, "approval");
        assert_eq!(card.color, COLOR_APPROVAL);
        assert!(card
            .fields
            .iter()
            .any(|(n, v)| n == "Approvals" && v.contains("write_file docs/plan.md")));
    }

    #[test]
    fn live_activity_comes_from_the_hook_and_is_marked_when_it_failed() {
        let mut activity = BTreeMap::new();
        activity.insert(
            "s_live".to_string(),
            Activity {
                tool: "plugin__kg__raw".to_string(),
                at: 9_500,
                ok: false,
            },
        );
        let card = render(&running_snapshot(), &activity, &cfg(), 10_000);
        let value = card
            .fields
            .iter()
            .find(|(n, _)| n == "Activity")
            .map(|(_, v)| v.clone())
            .expect("an activity field");
        assert!(value.contains("plugin__kg__raw"));
        assert!(value.contains("(failed)"));
        assert!(value.contains("500ms") || value.contains("0s"));
    }

    #[test]
    fn an_idle_card_names_the_last_session_and_a_recent_failure_is_red() {
        let mut snap = Snapshot {
            generated_at: 10_000,
            readable: true,
            recent: vec![session("s_old", "older session")],
            last_finished: Some(("failed".to_string(), 9_000)),
            ..Default::default()
        };
        snap.recent[0].updated_at = 9_000;
        let card = render(&snap, &BTreeMap::new(), &cfg(), 10_000);
        assert_eq!(card.state, "failed");
        assert_eq!(card.color, COLOR_FAILED);
        assert!(card.description.contains("older session"));
    }

    #[test]
    fn an_unreadable_database_says_so_instead_of_lying() {
        let snap = Snapshot {
            readable: false,
            problems: vec!["karucode's database is not at /x".to_string()],
            ..Default::default()
        };
        let card = render(&snap, &BTreeMap::new(), &cfg(), 10_000);
        assert_eq!(card.state, "unknown");
        assert!(card.description.contains("is not at"));
    }

    #[test]
    fn discord_limits_are_enforced_on_every_string() {
        let mut snap = running_snapshot();
        snap.running[0].prompt = "word ".repeat(5_000);
        snap.running[0].session.title = "t".repeat(1_000);
        let card = render(&snap, &BTreeMap::new(), &cfg(), 10_000);
        let embed = card.embed();
        assert!(embed["title"].as_str().unwrap().chars().count() <= MAX_TITLE);
        assert!(
            embed["description"].as_str().unwrap().chars().count() <= MAX_DESCRIPTION
        );
        for field in embed["fields"].as_array().unwrap() {
            assert!(field["name"].as_str().unwrap().chars().count() <= MAX_FIELD_NAME);
            assert!(field["value"].as_str().unwrap().chars().count() <= MAX_FIELD_VALUE);
        }
        assert!(embed["footer"]["text"].as_str().unwrap().chars().count() <= MAX_FOOTER);
    }

    #[test]
    fn the_total_embed_budget_is_enforced_by_dropping_the_tail() {
        let mut snap = running_snapshot();
        snap.agents_running = 3;
        snap.agents = (0..3)
            .map(|i| AgentRef {
                name: format!("agent{i}"),
                note: "n".repeat(900),
                ..Default::default()
            })
            .collect();
        snap.outcomes.completed = 4;
        let card = render(&snap, &BTreeMap::new(), &cfg(), 10_000);
        let embed = card.embed();
        let mut total = 0;
        total += embed["title"].as_str().unwrap().chars().count();
        total += embed["description"].as_str().unwrap().chars().count();
        total += embed["footer"]["text"].as_str().unwrap().chars().count();
        for field in embed["fields"].as_array().unwrap() {
            total += field["name"].as_str().unwrap().chars().count();
            total += field["value"].as_str().unwrap().chars().count();
        }
        assert!(total <= MAX_EMBED_TOTAL, "embed was {total} chars");
    }

    #[test]
    fn clamp_never_splits_a_character_and_never_exceeds_the_cap() {
        assert_eq!(clamp("hello", 10), "hello");
        let long = "é".repeat(100);
        let cut = clamp(&long, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
        let words = clamp("alpha beta gamma delta", 12);
        assert!(words.chars().count() <= 12);
        assert!(words.ends_with('…'));
        assert!(!words.contains(" …"));
    }

    #[test]
    fn timestamps_are_the_shape_discord_accepts() {
        // 2026-11-13T23:44:12Z
        assert_eq!(iso8601(1_794_613_452_000), "2026-11-13T23:44:12Z");
        assert_eq!(clock(1_794_613_452_000), "23:44:12 UTC");
        assert_eq!(human_span(0), "0s");
        assert_eq!(human_span(252_000), "4m 12s");
        assert_eq!(human_span(7_500_000), "2h 05m");
        assert_eq!(human_span(273_600_000), "3d 4h");
    }

    #[test]
    fn a_notice_is_a_message_of_its_own_with_no_pings() {
        let notice = Notice {
            id: "approval:a_1".to_string(),
            kind: "needs_input".to_string(),
            title: "Approval needed".to_string(),
            body: "write_file docs/plan.md".to_string(),
            session_id: "s_1".to_string(),
            session_title: "Plan the work".to_string(),
            project_id: "p_1".to_string(),
            source: "runtime".to_string(),
            at: 10_000,
        };
        let payload = notice_payload(&notice, &cfg());
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        assert_eq!(payload["embeds"][0]["color"], json!(COLOR_APPROVAL));
        assert!(payload["embeds"][0]["title"]
            .as_str()
            .unwrap()
            .contains("Approval needed"));
    }

    #[test]
    fn the_panel_gets_plain_text_not_markdown() {
        assert_eq!(plain("**karucode** · PRs"), "karucode · PRs");
        assert_eq!(plain("`plugin__kg__raw` · 12s ago"), "plugin__kg__raw · 12s ago");
        assert_eq!(plain("  plain  "), "plain");
    }

    #[test]
    fn an_offline_card_does_not_claim_anything_is_running() {
        let card = offline(&cfg(), 10_000);
        assert_eq!(card.state, "offline");
        assert_eq!(card.color, COLOR_IDLE);
        assert!(card.fields.is_empty());
    }
}
