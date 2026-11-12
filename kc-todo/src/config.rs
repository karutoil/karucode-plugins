//! Settings and on-disk storage for the `kc-todo` plugin.
//!
//! # Where lists live
//!
//! One JSON file **per session**, inside the workspace the session belongs to:
//!
//! ```text
//! <workspace>/.karucode/todo/sessions/<session-id>.json
//! ```
//!
//! Per session, not per project: two conversations about the same repository are
//! two different pieces of work, and a single list made each one inherit the
//! other's items. The panel and every tool are scoped by `context.session_id`,
//! which the host sends with each `tools/call` and each `panels/read`.
//!
//! `.karucode/` is the directory karucode already uses for composer attachments
//! (`POST /api/workspace/upload` writes `.karucode/attachments/<session>/`), and
//! keeping the list inside the workspace means the workspace-confined file tools
//! can read it, `git` can ignore it, and a project that is deleted takes its
//! lists with it.
//!
//! The path is **not** a security boundary: the plugin runs with the server's OS
//! permissions and can write anywhere. It is a convention that keeps the list
//! next to the code it describes.
//!
//! ## Upgrading from the per-project layout
//!
//! Before this change there was one shared `<workspace>/.karucode/todo/list.json`.
//! The first session to read after the upgrade inherits a *copy* of it and owns
//! its own file from then on; the legacy file is left in place so an existing
//! session does not appear to lose its work and another session can inherit it
//! too. Delete that file when you no longer want new sessions seeded with it.
//!
//! # Which workspace and session are "the" ones
//!
//! `before_prompt` and `panels/read` both carry the workspace and session id, and
//! since the plugin-panel revision so does every `tools/call`
//! (`context.session_id` / `context.workspace`). The plugin uses the forwarded
//! context first and remembers the most recent `before_prompt` only as a fallback
//! for an older host; every tool also accepts explicit `workspace` and `session`
//! arguments.
//!
//! # Settings
//!
//! Everything optional, read from `KARUCODE_PLUGIN_SETTINGS` (the `settings`
//! table of the `[[plugins]]` entry):
//!
//! | key | default | meaning |
//! | --- | --- | --- |
//! | `max_in_progress` | `1` | concurrent `in_progress` items before `allow_parallel` is needed |
//! | `carry_over_reminder` | `true` | put unfinished work in the next prompt |
//! | `compact_from` | `24` | item count where the prompt block switches to a summary |
//! | `sessions_dir` | `.karucode/todo/sessions` | where per-session lists are stored, workspace-relative |
//! | `list_path` | `.karucode/todo/list.json` | the legacy shared list, read once to seed a session |
//! | `workspace` | none | pin the workspace when the host does not send call context |
//! | `session` | none | pin the session when the host does not send call context |

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT_LIST_PATH: &str = ".karucode/todo/list.json";
/// Default directory for one list per session.
pub const DEFAULT_SESSIONS_DIR: &str = ".karucode/todo/sessions";

static SETTINGS: OnceLock<Value> = OnceLock::new();

/// The parsed `KARUCODE_PLUGIN_SETTINGS` object, read once per process.
///
/// Parsed leniently on purpose: a malformed settings table must not stop the
/// plugin from loading, because every setting has a working default. The first
/// call that needs a value logs nothing (stdout is the protocol, stderr is the
/// log) and simply falls back.
pub fn settings() -> &'static Value {
    SETTINGS.get_or_init(|| {
        std::env::var("KARUCODE_PLUGIN_SETTINGS")
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .filter(Value::is_object)
            .unwrap_or(Value::Null)
    })
}

fn setting<'a>(key: &str) -> Option<&'a Value> {
    settings().get(key)
}

/// Concurrent `in_progress` ceiling, minimum 1.
pub fn max_in_progress() -> usize {
    setting("max_in_progress")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(1)
}

/// Whether an ended run's open work is carried into the next prompt.
pub fn carry_over_reminder() -> bool {
    setting("carry_over_reminder")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Item count above which the prompt block is summarised instead of listed in
/// full. The block is re-sent on every model call, so it is part of the cached
/// prefix: keeping it bounded keeps the cache warm.
pub fn compact_from() -> usize {
    setting("compact_from")
        .and_then(Value::as_u64)
        .map(|n| n.max(4) as usize)
        .unwrap_or(24)
}

/// Workspace-relative path of the list file.
///
/// This is the **legacy** single-file location. Lists are now stored per
/// session under [`sessions_dir`]; this path is only read to seed the first
/// session's list after an upgrade (see [`read_for`]).
pub fn list_path() -> String {
    setting("list_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_LIST_PATH)
        .to_string()
}

/// Workspace-relative directory holding one list per session.
pub fn sessions_dir() -> String {
    setting("sessions_dir")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_SESSIONS_DIR)
        .to_string()
}

/// Reduce a session id to something safe to use as a file name.
///
/// Session ids arrive from the host, but they also arrive from a tool argument
/// (`session: "..."`) that a model writes, so this must not be trusted as a path
/// component. Anything outside `[A-Za-z0-9_-]` becomes `_`, which keeps
/// `../../etc/passwd` inside the directory and still produces a stable name for
/// a normal `s_ab12` id.
///
/// An empty or unusable id yields `None`; the caller then reports that it cannot
/// tell which session the call belongs to rather than silently sharing a file.
pub fn sanitize_session(session: &str) -> Option<String> {
    let trimmed = session.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
        // A long id is truncated rather than allowed to produce a file name that
        // some filesystem rejects; 120 keeps the full name plus `.json` well under
        // the usual 255-byte limit.
        if out.len() >= 120 {
            break;
        }
    }
    // `.` and `..` would escape or collide; the mapping above turns `.` into `_`,
    // so this is belt-and-braces for an id that was already only dots.
    let cleaned = out.trim_matches('_');
    if cleaned.is_empty() {
        return None;
    }
    Some(out)
}

/// Absolute path of one session's list.
pub fn session_file(workspace: &str, session: &str) -> Result<PathBuf, StoreError> {
    let root = Path::new(workspace);
    let dir = sessions_dir();
    let joined_dir = root.join(&dir);
    for candidate in [&dir, &list_path()] {
        if Path::new(candidate)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(StoreError::Corrupt {
                path: joined_dir,
                detail: format!("a configured list path ({candidate}) may not contain `..`"),
            });
        }
    }
    let Some(name) = sanitize_session(session) else {
        return Err(StoreError::NoSession);
    };
    Ok(joined_dir.join(format!("{name}.json")))
}

/// A session pinned by settings, if the operator pinned one.
///
/// Only useful for a deployment that runs a single session; the forwarded
/// `context.session_id` normally answers this.
pub fn pinned_session() -> Option<String> {
    setting("session")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// A workspace pinned by settings, if the operator pinned one.
pub fn pinned_workspace() -> Option<String> {
    setting("workspace")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// Why reading or writing the list failed, in terms of what the caller can do.
#[derive(Debug)]
pub enum StoreError {
    /// The file exists but is not a list this build understands.
    Corrupt { path: PathBuf, detail: String },
    /// The filesystem said no.
    Io { path: PathBuf, detail: String },
    /// The list passed in is newer than this build.
    Schema { found: u32 },
    /// No session id was available, and lists are per session now.
    NoSession,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt { path, detail } => write!(
                f,
                "the TODO list at `{}` is not readable: {detail}. \
                 Fix or delete that file and call `list` again.",
                path.display()
            ),
            Self::Io { path, detail } => write!(
                f,
                "could not access the TODO list at `{}`: {detail}",
                path.display()
            ),
            Self::Schema { found } => write!(
                f,
                "the TODO list was written by a newer kc-todo plugin (schema {found}); \
                 upgrade the plugin rather than overwriting the list"
            ),
            Self::NoSession => write!(
                f,
                "this plugin cannot tell which session the call belongs to, and TODO \
                 lists are per session. Pass `session` explicitly (the id from the \
                 session list), or set `settings.session` in the plugin's config \
                 entry if you only ever run one."
            ),
        }
    }
}

/// The highest schema this build writes and reads.
pub const SCHEMA: u32 = 1;

/// Read one session's list.
///
/// A missing file is an empty list, not an error: the first call in a fresh
/// session must succeed.
///
/// **Legacy migration.** Before lists were per session, one list lived at
/// `<workspace>/.karucode/todo/list.json` and every session in the project shared
/// it. On the first read for a session whose own file does not exist yet, that
/// legacy file is copied in, so an upgrade does not appear to lose the work. It
/// is a *copy*: the legacy file is left untouched so a second session started
/// right after the upgrade also inherits it rather than starting empty. Delete it
/// to stop new sessions inheriting it.
pub fn read_for(workspace: &str, session: &str) -> Result<crate::model::List, StoreError> {
    let path = session_file(workspace, session)?;
    match parse_at(&path)? {
        Some(list) => Ok(list),
        None => match parse_at(&list_file(workspace)?)? {
            Some(legacy) => {
                // Seed this session from the shared list and persist it, so the
                // next read is the fast path and the session owns its copy.
                let mut seeded = legacy;
                seeded.version = seeded.version.saturating_add(1);
                seeded.updated_ms = now_ms();
                let _ = write_for(workspace, session, &seeded);
                Ok(seeded)
            }
            None => Ok(crate::model::List::default()),
        },
    }
}

/// Parse a list file, treating absent/empty as `Ok(None)`.
fn parse_at(path: &Path) -> Result<Option<crate::model::List>, StoreError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(StoreError::Io {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })
        }
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let list: crate::model::List = serde_json::from_str(&raw).map_err(|e| StoreError::Corrupt {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    if list.schema > SCHEMA {
        return Err(StoreError::Schema { found: list.schema });
    }
    Ok(Some(list))
}

/// Write one session's list atomically.
///
/// Written to a sibling `.tmp` and renamed, so a crash mid-write cannot leave a
/// truncated list behind: the reader either sees the old file or the new one.
pub fn write_for(
    workspace: &str,
    session: &str,
    list: &crate::model::List,
) -> Result<PathBuf, StoreError> {
    let path = session_file(workspace, session)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| StoreError::Io {
            path: parent.to_path_buf(),
            detail: e.to_string(),
        })?;
    }
    let mut body = serde_json::to_vec_pretty(list).map_err(|e| StoreError::Corrupt {
        path: path.clone(),
        detail: format!("could not serialise the list: {e}"),
    })?;
    body.push(b'\n');
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    };
    write().map_err(|e| StoreError::Io {
        // Report the destination, not the temporary file: that is the path the
        // caller can act on.
        path: path.clone(),
        detail: e.to_string(),
    })?;
    Ok(path)
}

/// Absolute path of the legacy (project-wide) list file, after confinement.
///
/// Kept so the old single-list layout can be read once during migration. New
/// code writes through [`write_for`].
pub fn list_file(workspace: &str) -> Result<PathBuf, StoreError> {
    let root = Path::new(workspace);
    let relative = list_path();
    let joined = root.join(&relative);
    // Reject `..` anywhere in the *settings* path: a legitimate list path never
    // needs it.
    if Path::new(&relative)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(StoreError::Corrupt {
            path: joined,
            detail: format!("`list_path` ({relative}) may not contain `..`"),
        });
    }
    Ok(joined)
}

/// Milliseconds since the Unix epoch, saturating rather than failing.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable_without_any_settings() {
        // The test process sets no settings, so every accessor must fall back.
        assert_eq!(max_in_progress(), 1);
        assert!(carry_over_reminder());
        assert_eq!(compact_from(), 24);
        assert_eq!(list_path(), DEFAULT_LIST_PATH);
        assert_eq!(pinned_workspace(), None);
    }

    #[test]
    fn a_parent_dir_in_the_configured_path_is_refused() {
        // `list_file` reads the process-wide settings, so exercise the check
        // directly against a path it would produce.
        let bad = Path::new("../../etc/passwd");
        assert!(bad
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)));
    }

    #[test]
    fn a_missing_list_reads_as_empty_rather_than_failing() {
        let dir = std::env::temp_dir().join(format!("kc-todo-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = read_for(&dir.to_string_lossy(), "s_abc").unwrap();
        assert_eq!(list.items.len(), 0);
        assert_eq!(list.schema, SCHEMA);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_then_read_round_trips_and_creates_directories() {
        let dir = std::env::temp_dir().join(format!("kc-todo-rw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy().to_string();

        let mut list = crate::model::List::default();
        list.add("Ship it", "", None, 1).unwrap();
        let path = write_for(&root, "s_one", &list).unwrap();
        assert!(path.exists());
        assert!(path.starts_with(&dir));
        assert!(path.to_string_lossy().contains("s_one"));

        let back = read_for(&root, "s_one").unwrap();
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].title, "Ship it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of this layout: two sessions in one project do not share
    /// a list, so a second conversation never inherits the first one's items.
    #[test]
    fn two_sessions_in_one_workspace_keep_separate_lists() {
        let dir = std::env::temp_dir().join(format!("kc-todo-split-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy().to_string();

        let mut a = crate::model::List::default();
        a.add("Session A work", "", None, 1).unwrap();
        write_for(&root, "s_a", &a).unwrap();

        let mut b = crate::model::List::default();
        b.add("Session B work", "", None, 1).unwrap();
        write_for(&root, "s_b", &b).unwrap();

        assert_eq!(
            read_for(&root, "s_a").unwrap().items[0].title,
            "Session A work"
        );
        assert_eq!(
            read_for(&root, "s_b").unwrap().items[0].title,
            "Session B work"
        );
        // A third session starts empty rather than inheriting either.
        assert_eq!(read_for(&root, "s_c").unwrap().items.len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Upgrading must not look like the work vanished: the first session to read
    /// after the change inherits a copy of the old shared list.
    #[test]
    fn the_legacy_shared_list_seeds_a_new_session_once() {
        let dir = std::env::temp_dir().join(format!("kc-todo-legacy-{}", std::process::id()));
        let legacy = dir.join(DEFAULT_LIST_PATH);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let mut old = crate::model::List::default();
        old.add("Work from before the upgrade", "", None, 1)
            .unwrap();
        std::fs::write(&legacy, serde_json::to_vec(&old).unwrap()).unwrap();
        let root = dir.to_string_lossy().to_string();

        let seeded = read_for(&root, "s_first").unwrap();
        assert_eq!(seeded.items.len(), 1);
        assert_eq!(seeded.items[0].title, "Work from before the upgrade");
        // It was persisted, so the session owns its copy from now on.
        assert!(session_file(&root, "s_first").unwrap().exists());
        // The legacy file is untouched, so a second session can inherit it too.
        assert!(legacy.exists());
        assert_eq!(read_for(&root, "s_second").unwrap().items.len(), 1);

        // Once a session has its own file, the legacy file is not consulted.
        let mut own = seeded;
        own.clear(2);
        write_for(&root, "s_first", &own).unwrap();
        assert_eq!(read_for(&root, "s_first").unwrap().items.len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_session_file_reports_the_path_and_the_fix() {
        let dir = std::env::temp_dir().join(format!("kc-todo-bad-{}", std::process::id()));
        let root = dir.to_string_lossy().to_string();
        let file = session_file(&root, "s_bad").unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{not json").unwrap();

        let error = read_for(&root, "s_bad").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("is not readable"), "{text}");
        assert!(text.contains("Fix or delete"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_newer_schema_is_refused_instead_of_overwritten() {
        let dir = std::env::temp_dir().join(format!("kc-todo-new-{}", std::process::id()));
        let file = dir.join(DEFAULT_LIST_PATH);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, br#"{"schema":99,"items":[]}"#).unwrap();

        match read_for(&dir.to_string_lossy(), "s_x") {
            Err(StoreError::Schema { found }) => assert_eq!(found, 99),
            other => panic!("expected a schema error, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A session id reaches the filesystem, and a model can write the `session`
    /// argument, so it must not be usable to escape the directory.
    #[test]
    fn a_session_id_cannot_escape_the_sessions_directory() {
        let dir = std::env::temp_dir().join(format!("kc-todo-esc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy().to_string();
        let sessions = dir.join(sessions_dir());

        // Ids that map to a path component stay inside the directory…
        for hostile in ["../../etc/passwd", "/etc/passwd", "a/../../b"] {
            let path = session_file(&root, hostile).unwrap();
            assert!(
                path.starts_with(&sessions),
                "{hostile} resolved outside the sessions dir: {}",
                path.display()
            );
            // And it is a plain file name, not a nested path.
            assert_eq!(path.parent(), Some(sessions.as_path()));
        }
        // …and an id with nothing usable is refused rather than shared.
        for unusable in ["   ", "..", ".", ""] {
            assert!(
                matches!(session_file(&root, unusable), Err(StoreError::NoSession)),
                "{unusable:?} should be refused"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_ids_map_to_stable_file_names() {
        assert_eq!(sanitize_session("s_ab12").as_deref(), Some("s_ab12"));
        assert_eq!(sanitize_session(" s_ab12 ").as_deref(), Some("s_ab12"));
        assert_eq!(sanitize_session("a/b").as_deref(), Some("a_b"));
        assert_eq!(sanitize_session("a.b").as_deref(), Some("a_b"));
        assert_eq!(sanitize_session(""), None);
        // Long ids are truncated, not rejected.
        let long = "x".repeat(300);
        assert_eq!(sanitize_session(&long).unwrap().len(), 120);
    }
}
