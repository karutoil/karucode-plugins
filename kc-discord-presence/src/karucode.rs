//! What karucode is doing right now, read from karucode's own database.
//!
//! **Why the database and not the HTTP API.** The API is the supported contract,
//! but every route needs a credential: with web sign-in or `KARUCODE_AUTH_TOKEN`
//! on, a plugin must be handed a bearer token (an application API key) before it
//! can read anything. The database needs nothing but a path, and the host already
//! gives the plugin `HOME`, so `~/.local/share/karucode/karucode.db` resolves with
//! no setup at all. That is the difference between "paste a webhook URL" and
//! "paste a webhook URL and mint a key and keep it alive".
//!
//! The cost is coupling to a schema this plugin does not own, so every query here
//! is defensive: a missing table, a renamed column or a locked database degrades
//! the card and lands in [`Snapshot::problems`] instead of failing a publish.
//! `discord_status` and the panel surface those problems verbatim.
//!
//! The connection is opened read-only with `query_only` and a busy timeout, which
//! is safe against the server's concurrent WAL writers (the same approach
//! `kc-image-handoff` takes for the provider table).

use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Outcomes older than this are not "recent".
pub const OUTCOME_WINDOW_MS: i64 = 60 * 60 * 1000;

/// One session, as much as the card needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionRef {
    pub id: String,
    pub title: String,
    pub project: Option<String>,
    pub project_id: Option<String>,
    pub workspace: String,
    pub model: Option<String>,
    pub control: bool,
    pub updated_at: i64,
}

/// A run that is in flight.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunRef {
    pub run_id: String,
    pub started_at: i64,
    pub model: String,
    pub prompt: String,
    pub session: SessionRef,
}

/// A subagent that is live right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentRef {
    pub name: String,
    pub role: String,
    pub note: String,
    pub session_id: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApprovalRef {
    pub pending: i64,
    pub newest_title: String,
    pub newest_session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcomes {
    pub completed: i64,
    pub failed: i64,
    pub cancelled: i64,
    pub limit_reached: i64,
}

impl Outcomes {
    pub fn total(&self) -> i64 {
        self.completed + self.failed + self.cancelled + self.limit_reached
    }
}

/// Everything one publish is rendered from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub generated_at: i64,
    pub db_path: String,
    pub readable: bool,
    pub running: Vec<RunRef>,
    pub recent: Vec<SessionRef>,
    pub approvals: ApprovalRef,
    pub agents_running: i64,
    pub agents_paused: i64,
    pub agents: Vec<AgentRef>,
    pub outcomes: Outcomes,
    /// The newest finished run: its status and when it ended. This is what makes
    /// a red card honest ("the last thing that happened failed") instead of
    /// guessing from an hour's totals.
    pub last_finished: Option<(String, i64)>,
    pub problems: Vec<String>,
}

impl Snapshot {
    /// The session the card is about: the most recently started run, else the
    /// most recently touched session, else nothing.
    pub fn focus(&self) -> Option<(&SessionRef, Option<&RunRef>)> {
        if let Some(run) = self.running.first() {
            return Some((&run.session, Some(run)));
        }
        self.recent.first().map(|session| (session, None))
    }

    /// Sessions other than the focus, for the "also running" line.
    pub fn also_running(&self, focus: Option<&str>) -> Vec<&RunRef> {
        self.running
            .iter()
            .filter(|run| Some(run.session.id.as_str()) != focus)
            .collect()
    }

    /// The newest finished run, when it ended recently enough to colour the card.
    pub fn last_failed(&self, now: i64) -> bool {
        match &self.last_finished {
            Some((status, finished_at)) => {
                status == "failed" && now.saturating_sub(*finished_at) <= OUTCOME_WINDOW_MS
            }
            None => false,
        }
    }
}

/// The database the server uses, from the data dir this plugin resolved.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("karucode.db")
}

/// Read one snapshot. Never fails: an unreadable database is a snapshot with
/// `readable: false` and a problem attached.
pub fn snapshot(data_dir: &Path, now: i64, max_sessions: usize) -> Snapshot {
    let path = db_path(data_dir);
    let mut snap = Snapshot {
        generated_at: now,
        db_path: path.display().to_string(),
        ..Default::default()
    };
    let conn = match open_read_only(&path) {
        Ok(conn) => conn,
        Err(e) => {
            snap.problems.push(e);
            return snap;
        }
    };
    snap.readable = true;
    let limit = max_sessions.clamp(1, 8) as i64;

    match running_runs(&conn, limit) {
        Ok(runs) => snap.running = runs,
        Err(e) => snap.problems.push(format!("running runs: {e}")),
    }
    match recent_sessions(&conn, limit) {
        Ok(sessions) => snap.recent = sessions,
        Err(e) => snap.problems.push(format!("recent sessions: {e}")),
    }
    match approvals(&conn) {
        Ok(approvals) => snap.approvals = approvals,
        Err(e) => snap.problems.push(format!("approvals: {e}")),
    }
    match agents(&conn) {
        Ok((running, paused, live)) => {
            snap.agents_running = running;
            snap.agents_paused = paused;
            snap.agents = live;
        }
        Err(e) => snap.problems.push(format!("agents: {e}")),
    }
    match outcomes(&conn, now - OUTCOME_WINDOW_MS) {
        Ok(outcomes) => snap.outcomes = outcomes,
        Err(e) => snap.problems.push(format!("recent outcomes: {e}")),
    }
    match last_finished(&conn) {
        Ok(last) => snap.last_finished = last,
        Err(e) => snap.problems.push(format!("last finished run: {e}")),
    }
    snap
}

fn open_read_only(path: &Path) -> Result<Connection, String> {
    if !path.exists() {
        return Err(format!(
            "karucode's database is not at {} - if the server runs with a custom --data-dir, set \
             `data_dir` in this plugin's settings",
            path.display()
        ));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| format!("cannot set a busy timeout on {}: {e}", path.display()))?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|e| format!("cannot mark {} read-only: {e}", path.display()))?;
    Ok(conn)
}

fn running_runs(conn: &Connection, limit: i64) -> Result<Vec<RunRef>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.started_at, r.model, r.prompt,
                s.id, s.title, s.project_id, s.workspace, s.model, s.control, s.updated_at,
                COALESCE(p.name, '')
         FROM runs r
         JOIN sessions s ON s.id = r.session_id
         LEFT JOIN projects p ON p.id = s.project_id
         WHERE r.status = 'running' AND s.ephemeral = 0
         ORDER BY r.started_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |row| {
        let project = row.get::<_, String>(11)?;
        Ok(RunRef {
            run_id: row.get(0)?,
            started_at: row.get(1)?,
            model: row.get(2)?,
            prompt: row.get(3)?,
            session: SessionRef {
                id: row.get(4)?,
                title: row.get(5)?,
                project_id: row.get(6)?,
                workspace: row.get(7)?,
                model: row.get(8)?,
                control: row.get::<_, i64>(9)? != 0,
                updated_at: row.get(10)?,
                project: Some(project).filter(|p| !p.is_empty()),
            },
        })
    })?;
    rows.collect()
}

fn recent_sessions(conn: &Connection, limit: i64) -> Result<Vec<SessionRef>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.title, s.project_id, s.workspace, s.model, s.control, s.updated_at,
                COALESCE(p.name, '')
         FROM sessions s
         LEFT JOIN projects p ON p.id = s.project_id
         WHERE s.ephemeral = 0
         ORDER BY s.updated_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |row| {
        let project = row.get::<_, String>(7)?;
        Ok(SessionRef {
            id: row.get(0)?,
            title: row.get(1)?,
            project_id: row.get(2)?,
            workspace: row.get(3)?,
            model: row.get(4)?,
            control: row.get::<_, i64>(5)? != 0,
            updated_at: row.get(6)?,
            project: Some(project).filter(|p| !p.is_empty()),
        })
    })?;
    rows.collect()
}

fn approvals(conn: &Connection) -> Result<ApprovalRef, rusqlite::Error> {
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM approvals WHERE status = 'pending'",
        [],
        |row| row.get(0),
    )?;
    let newest = conn
        .query_row(
            "SELECT title, session_id FROM approvals WHERE status = 'pending'
             ORDER BY created_at DESC LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok();
    Ok(match newest {
        Some((title, session)) => ApprovalRef {
            pending,
            newest_title: title,
            newest_session: session,
        },
        None => ApprovalRef {
            pending,
            ..Default::default()
        },
    })
}

fn agents(conn: &Connection) -> Result<(i64, i64, Vec<AgentRef>), rusqlite::Error> {
    let mut counts = conn.prepare(
        "SELECT status, COUNT(*) FROM agents WHERE status IN ('running', 'paused') GROUP BY status",
    )?;
    let mut running = 0;
    let mut paused = 0;
    for row in counts.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (status, count) = row?;
        match status.as_str() {
            "running" => running = count,
            "paused" => paused = count,
            _ => {}
        }
    }
    let mut live = conn.prepare(
        "SELECT name, role, note, session_id, updated_at FROM agents
         WHERE status = 'running' ORDER BY updated_at DESC LIMIT 3",
    )?;
    let rows = live.query_map([], |row| {
        Ok(AgentRef {
            name: row.get(0)?,
            role: row.get(1)?,
            note: row.get(2)?,
            session_id: row.get(3)?,
            updated_at: row.get(4)?,
        })
    })?;
    let live: Vec<AgentRef> = rows.collect::<Result<_, _>>()?;
    Ok((running, paused, live))
}

fn outcomes(conn: &Connection, since: i64) -> Result<Outcomes, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT status, COUNT(*) FROM runs WHERE started_at >= ?1 GROUP BY status",
    )?;
    let mut outcomes = Outcomes::default();
    for row in stmt.query_map([since], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (status, count) = row?;
        match status.as_str() {
            "completed" => outcomes.completed = count,
            "failed" => outcomes.failed = count,
            "cancelled" => outcomes.cancelled = count,
            "limit_reached" => outcomes.limit_reached = count,
            _ => {}
        }
    }
    Ok(outcomes)
}

fn last_finished(conn: &Connection) -> Result<Option<(String, i64)>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT status, COALESCE(finished_at, started_at) FROM runs
         WHERE status != 'running' ORDER BY COALESCE(finished_at, started_at) DESC LIMIT 1",
    )?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
        None => Ok(None),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A database with the columns this plugin reads, so the queries are
    /// exercised for real rather than mocked.
    pub(crate) fn fixture_db(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("karucode.db");
        let conn = Connection::open(&path).unwrap();
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
            "#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, path) VALUES ('p_1', 'karucode-plugins', '/w')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, title, workspace, created_at, updated_at, project_id, model)
             VALUES ('s_live', 'build the discord plugin', '/w', 1000, 2000, 'p_1', 'm-1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, title, workspace, created_at, updated_at, project_id)
             VALUES ('s_old', 'older session', '/w', 1000, 1500, 'p_1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, title, workspace, created_at, updated_at, ephemeral)
             VALUES ('s_helper', 'plugin helper', '/w', 1000, 3000, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (id, session_id, status, model, prompt, started_at)
             VALUES ('r_1', 's_live', 'running', 'm-1', 'build the discord plugin', 5000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (id, session_id, status, model, prompt, started_at, finished_at)
             VALUES ('r_0', 's_old', 'failed', 'm-1', 'older session', 4000, 4500)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (id, session_id, name, role, status, created_at, updated_at, note)
             VALUES ('a_1', 's_live', 'reviewer', 'reviewer', 'running', 5000, 6000, 'reading src/lib.rs')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approvals (id, session_id, run_id, kind, title, status, created_at)
             VALUES ('ap_1', 's_live', 'r_1', 'tool', 'write_file docs/plan.md', 'pending', 7000)",
            [],
        )
        .unwrap();
        path
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "kc-discord-presence-db-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn reads_a_live_snapshot_and_skips_ephemeral_sessions() {
        let dir = tempdir();
        fixture_db(&dir);
        let snap = snapshot(&dir, 10_000, 3);
        assert!(snap.readable, "{:?}", snap.problems);
        assert_eq!(snap.running.len(), 1);
        let run = &snap.running[0];
        assert_eq!(run.session.id, "s_live");
        assert_eq!(run.session.project.as_deref(), Some("karucode-plugins"));
        assert_eq!(run.model, "m-1");
        assert_eq!(snap.approvals.pending, 1);
        assert_eq!(snap.approvals.newest_title, "write_file docs/plan.md");
        assert_eq!(snap.agents_running, 1);
        assert_eq!(snap.agents[0].note, "reading src/lib.rs");
        assert_eq!(snap.outcomes.failed, 1);
        // The ephemeral helper session is never the "recent" answer.
        assert!(snap.recent.iter().all(|s| s.id != "s_helper"));
        assert_eq!(snap.recent[0].id, "s_live");
    }

    #[test]
    fn a_missing_database_is_a_problem_not_a_panic() {
        let dir = tempdir();
        let snap = snapshot(&dir, 10_000, 3);
        assert!(!snap.readable);
        assert_eq!(snap.running.len(), 0);
        assert!(snap.problems[0].contains("is not at"));
        assert!(snap.problems[0].contains("data_dir"));
    }

    #[test]
    fn a_database_without_the_tables_degrades_query_by_query() {
        let dir = tempdir();
        let path = dir.join("karucode.db");
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE sessions (id TEXT)").unwrap();
        let snap = snapshot(&dir, 10_000, 3);
        assert!(snap.readable);
        assert!(!snap.problems.is_empty());
        assert!(snap.problems.iter().any(|p| p.contains("running runs")));
    }

    #[test]
    fn focus_prefers_a_running_run_and_also_running_excludes_it() {
        let dir = tempdir();
        fixture_db(&dir);
        let mut snap = snapshot(&dir, 10_000, 3);
        let (session, run) = snap.focus().expect("a focus");
        assert_eq!(session.id, "s_live");
        assert!(run.is_some());
        assert!(snap.also_running(Some("s_live")).is_empty());

        snap.running.push(RunRef {
            run_id: "r_2".into(),
            session: SessionRef {
                id: "s_other".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(snap.also_running(Some("s_live")).len(), 1);
    }
}
