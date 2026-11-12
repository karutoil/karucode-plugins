//! Where goals live: `<workspace>/.karucode/goals/<session>.json`, one file per
//! session.
//!
//! Two properties are load-bearing:
//!
//! * **The file is the truth.** The plugin keeps no authoritative state in
//!   memory, so a plugin restart, a hot reload or a rebuilt binary loses nothing.
//!   The session -> workspace index in `state_dir` is a *hint* about where to
//!   look; goals are found by reading the directory.
//! * **A read-modify-write is atomic.** Writes go to a temp file and are renamed
//!   over the target, and every mutation happens under one process-wide lock, so
//!   a tool call and the worker cannot interleave a stale read into a save.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::model::GoalFile;

/// `http://127.0.0.1:8787` -> `127_0_0_1_8787`: a file-name-safe form of the
/// server address, so two servers sharing a data dir cannot read each other's
/// session index.
pub fn slug(server: &str) -> String {
    let cleaned: String = server
        .trim()
        .trim_start_matches("http://")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

#[derive(Debug)]
pub struct Store {
    state_dir: PathBuf,
    /// The server this plugin talks to, slugged into the index file name.
    ///
    /// Two servers can run the same plugin binary out of the same data directory
    /// (a test harness and the real one, or two ports during an upgrade). A
    /// single shared index made each server's engine reconcile the *other*
    /// server's sessions, ask its own port about them, get a 404, and block live
    /// goals. The index is therefore per server; the goal files themselves are
    /// per workspace and need no namespacing.
    server: String,
    /// Serialises every load-modify-save. The worker and a tool handler both
    /// mutate goal files, and a lost update here would lose a criterion.
    lock: Mutex<()>,
}

impl Store {
    pub fn new(state_dir: impl Into<PathBuf>, server: impl AsRef<str>) -> Self {
        Self {
            state_dir: state_dir.into(),
            server: slug(server.as_ref()),
            lock: Mutex::new(()),
        }
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// `.karucode/goals` inside a workspace.
    pub fn goals_dir(workspace: &str) -> PathBuf {
        Path::new(workspace).join(".karucode").join("goals")
    }

    pub fn goal_path(workspace: &str, session_id: &str) -> PathBuf {
        Self::goals_dir(workspace).join(format!("{session_id}.json"))
    }

    /// A session id is used as a file name, so it may only contain characters a
    /// file name can hold. Anything else is refused loudly rather than silently
    /// rewritten into a path that belongs to another session.
    pub fn valid_session_id(session_id: &str) -> bool {
        !session_id.is_empty()
            && session_id.len() <= 128
            && session_id != "."
            && session_id != ".."
            && session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    }

    /// Read one session's goals. A missing file is an empty document, not an
    /// error: that is the normal state of a session that has no goal.
    pub fn load(&self, workspace: &str, session_id: &str) -> Result<GoalFile, String> {
        if !Self::valid_session_id(session_id) {
            return Err(format!(
                "`{session_id}` is not a usable session id (letters, digits, `-`, `_`, `.`)"
            ));
        }
        let path = Self::goal_path(workspace, session_id);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(GoalFile::new(session_id))
            }
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let mut file: GoalFile = serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} is not valid goal JSON ({e}); fix the file by hand or delete it to start \
                 over - this plugin will not overwrite it",
                path.display()
            )
        })?;
        if file.session_id.is_empty() {
            file.session_id = session_id.to_string();
        }
        Ok(file)
    }

    pub fn save(&self, workspace: &str, file: &GoalFile) -> Result<(), String> {
        if !Self::valid_session_id(&file.session_id) {
            return Err(format!("`{}` is not a usable session id", file.session_id));
        }
        let dir = Self::goals_dir(workspace);
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join(format!("{}.json", file.session_id));
        let temp = dir.join(format!(".{}.json.tmp", file.session_id));
        let text = serde_json::to_string_pretty(file)
            .map_err(|e| format!("could not encode the goal document: {e}"))?;
        fs::write(&temp, text).map_err(|e| format!("cannot write {}: {e}", temp.display()))?;
        fs::rename(&temp, &path).map_err(|e| {
            let _ = fs::remove_file(&temp);
            format!("cannot replace {}: {e}", path.display())
        })?;
        Ok(())
    }

    /// Load, mutate, save - the only way a caller should change a goal.
    ///
    /// The closure returns `Err` to abort without writing, so a refused
    /// `goal_complete` leaves the file byte-identical.
    pub fn update<T>(
        &self,
        workspace: &str,
        session_id: &str,
        change: impl FnOnce(&mut GoalFile) -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.load(workspace, session_id)?;
        let out = change(&mut file)?;
        self.save(workspace, &file)?;
        Ok(out)
    }

    /// Every goal document in a workspace, ordered by session id. Corrupt files
    /// are skipped: one unreadable document must not blind the panel or the
    /// reconciler to the rest.
    pub fn list(&self, workspace: &str) -> Vec<(String, GoalFile)> {
        let dir = Self::goals_dir(workspace);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with('.') || !Self::valid_session_id(stem) {
                continue;
            }
            if let Ok(file) = self.load(workspace, stem) {
                out.push((stem.to_string(), file));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    // ---- the session -> workspace index -------------------------------------

    fn index_path(&self) -> PathBuf {
        self.state_dir.join(format!("sessions.{}.json", self.server))
    }

    fn read_index(&self) -> BTreeMap<String, String> {
        fs::read_to_string(self.index_path())
            .ok()
            .and_then(|text| serde_json::from_str::<BTreeMap<String, String>>(&text).ok())
            .unwrap_or_default()
    }

    /// Remember where a session lives. Best effort: the index is an optimisation
    /// for the reconciler, and a read-only data dir must not break a tool.
    pub fn remember(&self, session_id: &str, workspace: &str) {
        if !Self::valid_session_id(session_id) || workspace.is_empty() {
            return;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut index = self.read_index();
        if index.get(session_id).map(String::as_str) == Some(workspace) {
            return;
        }
        index.insert(session_id.to_string(), workspace.to_string());
        let _ = fs::create_dir_all(&self.state_dir);
        if let Ok(text) = serde_json::to_string_pretty(&index) {
            let _ = fs::write(self.index_path(), text);
        }
    }

    pub fn sessions(&self) -> BTreeMap<String, String> {
        self.read_index()
    }

    pub fn forget(&self, session_id: &str) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut index = self.read_index();
        if index.remove(session_id).is_some() {
            if let Ok(text) = serde_json::to_string_pretty(&index) {
                let _ = fs::write(self.index_path(), text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Budget, Criterion, Source};

    fn temp_store() -> (PathBuf, PathBuf, Store) {
        let root = crate::model::temp_dir("kc-goals-store");
        let workspace = root.join("ws");
        let state = root.join("state");
        fs::create_dir_all(&workspace).unwrap();
        (root, workspace.clone(), Store::new(state, "http://127.0.0.1:8787"))
    }

    fn with_goal(store: &Store, workspace: &Path, session: &str) {
        store
            .update(workspace.to_str().unwrap(), session, |file| {
                let id = file.next_goal_id();
                file.goals.push(crate::model::Goal::new(
                    id,
                    "Ship it",
                    "Ship it",
                    vec![Criterion::new("c1", "builds")],
                    Budget::default(),
                    Source::User,
                    1_000,
                ));
                file.refresh_active();
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_missing_file_is_an_empty_document() {
        let (root, ws, store) = temp_store();
        let file = store.load(ws.to_str().unwrap(), "s_1").unwrap();
        assert!(file.goals.is_empty());
        assert_eq!(file.session_id, "s_1");
        // Nothing was created by reading.
        assert!(!Store::goals_dir(ws.to_str().unwrap()).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_goal_round_trips_through_the_file() {
        let (root, ws, store) = temp_store();
        with_goal(&store, &ws, "s_1");
        let file = store.load(ws.to_str().unwrap(), "s_1").unwrap();
        assert_eq!(file.goals.len(), 1);
        assert_eq!(file.active_goal.as_deref(), Some("g1"));
        let text = fs::read_to_string(Store::goal_path(ws.to_str().unwrap(), "s_1")).unwrap();
        assert!(text.contains("Ship it"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_persists_and_returns_the_closures_value() {
        let (root, ws, store) = temp_store();
        let score = store
            .update(ws.to_str().unwrap(), "s_1", |file| {
                file.goals.push(crate::model::Goal::new(
                    "g1",
                    "t",
                    "s",
                    vec![Criterion::new("c1", "builds")],
                    Budget::default(),
                    Source::User,
                    1_000,
                ));
                file.refresh_active();
                Ok(file.active().unwrap().score())
            })
            .unwrap();
        assert_eq!(score, "0/1");
        let again = store.load(ws.to_str().unwrap(), "s_1").unwrap();
        assert_eq!(again.goals.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_refused_update_writes_nothing() {
        let (root, ws, store) = temp_store();
        with_goal(&store, &ws, "s_1");
        let before = fs::read_to_string(Store::goal_path(ws.to_str().unwrap(), "s_1")).unwrap();
        let err = store
            .update(ws.to_str().unwrap(), "s_1", |file| {
                if let Some(goal) = file.active_mut() {
                    goal.title = "clobbered".into();
                }
                Err::<(), _>("no".to_string())
            })
            .unwrap_err();
        assert_eq!(err, "no");
        let after = fs::read_to_string(Store::goal_path(ws.to_str().unwrap(), "s_1")).unwrap();
        assert_eq!(before, after, "a failed update must not touch the file");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_corrupt_file_is_an_error_that_names_the_path_and_is_never_overwritten() {
        let (root, ws, store) = temp_store();
        let dir = Store::goals_dir(ws.to_str().unwrap());
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s_bad.json");
        fs::write(&path, "{not json").unwrap();
        let err = store.load(ws.to_str().unwrap(), "s_bad").unwrap_err();
        assert!(err.contains("s_bad.json"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
        // The panel's listing skips it rather than failing entirely.
        with_goal(&store, &ws, "s_ok");
        let listed = store.list(ws.to_str().unwrap());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "s_ok");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_dangerous_session_id_is_refused() {
        let (root, ws, store) = temp_store();
        assert!(!Store::valid_session_id("../etc/passwd"));
        assert!(!Store::valid_session_id("a/b"));
        assert!(!Store::valid_session_id(""));
        assert!(store.load(ws.to_str().unwrap(), "../x").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_index_remembers_and_forgets_sessions() {
        let (root, _ws, store) = temp_store();
        store.remember("s_1", "/w/one");
        store.remember("s_2", "/w/two");
        let index = store.sessions();
        assert_eq!(index.get("s_1").map(String::as_str), Some("/w/one"));
        assert_eq!(index.get("s_2").map(String::as_str), Some("/w/two"));
        store.forget("s_1");
        assert!(!store.sessions().contains_key("s_1"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_bad_session_id_is_not_indexed() {
        let (root, _ws, store) = temp_store();
        store.remember("../escape", "/w");
        assert!(store.sessions().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_servers_sharing_a_state_dir_keep_separate_indexes() {
        // The bug this prevents: a test harness and the real server running the
        // same plugin binary out of one data dir, each reconciling the other's
        // sessions and blocking goals that belong to a different port.
        let root = crate::model::temp_dir("kc-goals-store-split");
        let state = root.join("state");
        let a = Store::new(&state, "http://127.0.0.1:8787");
        let b = Store::new(&state, "http://127.0.0.1:9999");
        a.remember("s_a", "/w/a");
        b.remember("s_b", "/w/b");
        assert_eq!(a.sessions().keys().collect::<Vec<_>>(), vec!["s_a"]);
        assert_eq!(b.sessions().keys().collect::<Vec<_>>(), vec!["s_b"]);
        assert!(a.index_path().to_string_lossy().contains("8787"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_server_address_slugs_into_a_file_name() {
        assert_eq!(slug("http://127.0.0.1:8787"), "127_0_0_1_8787");
        assert_eq!(slug(""), "default");
        assert_eq!(slug("http://[::1]:80"), "___1__80");
    }

    #[test]
    fn no_leftover_temp_files_after_a_save() {
        let (root, ws, store) = temp_store();
        with_goal(&store, &ws, "s_1");
        let dir = Store::goals_dir(ws.to_str().unwrap());
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = fs::remove_dir_all(root);
    }
}
