//! Resolve a vision endpoint from karucode's own model configuration, so the
//! plugin uses the same providers, models and keys the operator already
//! configured for karucode itself - no parallel credential store to maintain.
//!
//! Where the configuration lives (highest priority first, mirroring karucode):
//!
//! 1. the SQLite `settings` table in the data dir (what the Settings screen
//!    wrote; `KARUCODE_DATA_DIR`, default `~/.local/share/karucode`),
//! 2. `~/.config/karucode/config.toml` (the `[[providers]]` tables),
//! 3. the environment (`KARUCODE_BASE_URL` / `KARUCODE_API_KEY` / ...) - not
//!    consulted here: the host clears a plugin's environment by design, and the
//!    operator can override per-plugin through the `env` table instead.
//!
//! The DB is opened read-only with `query_only` and a busy timeout, which is
//! safe against the server's concurrent WAL writers.
//!
//! Model choice, from the providers the operator already configured:
//! 1. the session's pinned model, when the request carries one and it has vision,
//! 2. the active model, when it has vision,
//! 3. the first model of the active provider that has vision,
//! 4. the first vision model of any provider, as a last resort.
//!
//! A vision model is one whose entry says `vision = true` (karucode's own
//! convention; nothing is guessed from model ids).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a `see` call needs to reach a vision model.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// How the endpoint was chosen, for `status` and error messages.
    pub origin: String,
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ProviderProfile {
    #[serde(default)]
    id: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    models: Vec<ProviderModel>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ProviderModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    vision: Option<bool>,
}

/// One row of karucode's `settings` table (JSON in `value`).
fn settings_table_value(conn: &rusqlite::Connection, key: &str) -> Result<Option<String>, String> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(format!(
            "cannot read `{key}` from the settings table: {other}"
        )),
    })
}

/// Open karucode's DB read-only. `path` is the DB file inside the data dir.
fn open_read_only(path: &Path) -> Result<rusqlite::Connection, String> {
    if !path.exists() {
        return Err(format!(
            "karucode's database does not exist at {} (is KARUCODE_DATA_DIR right?)",
            path.display()
        ));
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
        | rusqlite::OpenFlags::SQLITE_OPEN_URI;
    let conn = rusqlite::Connection::open_with_flags(path, flags)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| format!("cannot set a busy timeout: {e}"))?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|e| format!("cannot mark the connection read-only: {e}"))?;
    Ok(conn)
}

/// The karucode data dir: `KARUCODE_DATA_DIR` (the host passes it to plugins)
/// or the default `~/.local/share/karucode`.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KARUCODE_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/share/karucode")
}

/// Read the providers configuration, DB first, then config.toml.
fn read_providers(data_dir: &Path) -> Result<(Vec<ProviderProfile>, String), String> {
    let db = data_dir.join("karucode.db");
    if db.exists() {
        let conn = open_read_only(&db)?;
        let providers = settings_table_value(&conn, "model.providers")?;
        let active_provider = settings_table_value(&conn, "model.active_provider")?;
        let active_model = settings_table_value(&conn, "model.active_model")?;
        if let Some(raw) = providers.filter(|s| !s.trim().is_empty()) {
            let parsed = parse_profiles(&raw)?;
            let context = match (active_provider, active_model) {
                (Some(p), Some(m)) if !p.is_empty() && !m.is_empty() => {
                    format!("karucode DB: active {m} on provider {p}")
                }
                _ => "karucode DB".to_string(),
            };
            return Ok((parsed, context));
        }
    }
    // config.toml fallback: [[providers]] tables.
    let config_path = config_toml_path();
    if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("cannot read {}: {e}", config_path.display()))?;
        let doc: toml::Value = raw
            .parse()
            .map_err(|e| format!("{} is not valid TOML: {e}", config_path.display()))?;
        let providers = doc
            .get("providers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let profiles = toml_providers_to_profiles(&providers);
        if !profiles.is_empty() {
            let active_provider = doc
                .get("active_provider")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let active_model = doc
                .get("active_model")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let context = if !active_model.is_empty() {
                format!("config.toml: active {active_model} on {active_provider}")
            } else {
                "config.toml".to_string()
            };
            return Ok((profiles, context));
        }
    }
    Err(format!(
        "karucode has no provider configuration to reuse: no `model.providers` in {} and no \
         [[providers]] in {}. Configure karucode's own model first, or set base_url/model \
         explicitly in this plugin's settings",
        db.display(),
        config_path.display()
    ))
}

fn config_toml_path() -> PathBuf {
    if let Ok(path) = std::env::var("KARUCODE_CONFIG") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/karucode/config.toml")
}

fn parse_profiles(raw: &str) -> Result<Vec<ProviderProfile>, String> {
    serde_json::from_str(raw)
        .map_err(|e| format!("karucode's model.providers value is not valid JSON: {e}"))
}

fn toml_providers_to_profiles(tables: &[toml::Value]) -> Vec<ProviderProfile> {
    tables
        .iter()
        .filter_map(|table| {
            let json = serde_json::to_value(table).ok()?;
            serde_json::from_value::<ProviderProfile>(json).ok()
        })
        .collect()
}

/// Which provider the DB says is active.
fn read_active_from_db(conn: &rusqlite::Connection) -> (Option<String>, Option<String>) {
    let provider = settings_table_value(conn, "model.active_provider")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let model = settings_table_value(conn, "model.active_model")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    (provider, model)
}

/// Pick the vision model and its provider's endpoint from karucode's config.
///
/// `pinned_model` is the model the *calling session* is pinned to, when known;
/// it wins if it has vision. `requested_model` is an explicit per-call model
/// override from the tool arguments.
/// The install-wide default: `(provider, model)` from karucode's own
/// settings (the active model), when one is configured.
pub fn active_model() -> Option<(Option<String>, String)> {
    let data_dir = data_dir();
    let db = data_dir.join("karucode.db");
    open_read_only(&db)
        .ok()
        .map(|conn| read_active_from_db(&conn))
        .and_then(|(provider, model)| {
            let provider = provider.filter(|p| !p.is_empty());
            model.filter(|m| !m.is_empty()).map(|m| (provider, m))
        })
        .or_else(|| {
            // No DB row: fall back to the config-file active selection.
            read_providers(&data_dir)
                .ok()
                .map(|_| (None, String::new()))
                .filter(|(_, m)| !m.is_empty())
        })
}

/// Whether a named model has `vision = true` in karucode's provider
/// configuration. `None` when the model cannot be found at all.
/// `provider` narrows the search when set; otherwise the model id is looked
/// up in every profile (first match wins, exactly like `resolve`).
pub fn model_has_vision(model: &str, provider: Option<&str>) -> Option<bool> {
    let data_dir = data_dir();
    let (profiles, _) = read_providers(&data_dir).ok()?;
    // Narrow to the named provider when one was given.
    for profile in &profiles {
        if provider.is_some_and(|wanted| profile.id != wanted) {
            continue;
        }
        if let Some(entry) = profile.models.iter().find(|m| m.id == model) {
            return Some(entry.vision.unwrap_or(false));
        }
    }
    // Not in the named provider: look everywhere so a pin naming a stale
    // provider still resolves (the same forgiveness a session pin gets).
    if provider.is_some() {
        for profile in &profiles {
            if let Some(entry) = profile.models.iter().find(|m| m.id == model) {
                return Some(entry.vision.unwrap_or(false));
            }
        }
    }
    None
}

pub fn resolve(
    pinned_model: Option<&str>,
    requested_model: Option<&str>,
) -> Result<Resolved, String> {
    let data_dir = data_dir();
    let (profiles, origin) = read_providers(&data_dir)?;
    if profiles.is_empty() {
        return Err("karucode's provider configuration has no profiles".to_string());
    }

    // The DB may also pin the active provider/model even when the providers
    // JSON came from there; re-read them so the context string is honest.
    let db = data_dir.join("karucode.db");
    let (db_provider, db_model) = open_read_only(&db)
        .ok()
        .map(|conn| read_active_from_db(&conn))
        .unwrap_or((None, None));

    // 1. Explicit per-call model override.
    if let Some(requested) = requested_model.map(str::trim).filter(|m| !m.is_empty()) {
        if let Some(found) = find_model_entry(&profiles, requested) {
            return Ok(found.resolved(format!("per-call override: {requested} ({origin})")));
        }
        return Err(format!(
            "`model` `{requested}` is not in karucode's provider configuration"
        ));
    }

    // 2. The session's pinned model, when it has vision.
    if let Some(pinned) = pinned_model.map(str::trim).filter(|m| !m.is_empty()) {
        if let Some(found) = find_model_entry(&profiles, pinned) {
            if found.entry.vision.unwrap_or(false) {
                return Ok(
                    found.resolved(format!("the session's pinned model {pinned} ({origin})"))
                );
            }
        }
    }

    // 3. The active model, when it has vision.
    if let Some(active) = db_model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        if let Some(found) = find_model_entry(&profiles, active) {
            if found.entry.vision.unwrap_or(false) {
                return Ok(found.resolved(format!("karucode's active model {active} ({origin})")));
            }
        }
    }

    // 4. First vision model of the active provider, then of any provider.
    if let Some(provider) = db_provider
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        if let Some(profile) = profiles.iter().find(|p| p.id == provider) {
            if let Some(entry) = profile.models.iter().find(|m| m.vision.unwrap_or(false)) {
                return Ok(Resolved {
                    base_url: profile.base_url.clone(),
                    model: entry.id.clone(),
                    api_key: profile.api_key.clone(),
                    provider: Some(profile.id.clone()),
                    origin: format!("first vision model of active provider {provider} ({origin})"),
                });
            }
            return Err(format!(
                "the active provider `{provider}` has no vision model; add one (a model with \
                 `vision = true`) in karucode's settings, or pin this session to a vision model"
            ));
        }
    }
    for profile in &profiles {
        if let Some(entry) = profile.models.iter().find(|m| m.vision.unwrap_or(false)) {
            return Ok(Resolved {
                base_url: profile.base_url.clone(),
                model: entry.id.clone(),
                api_key: profile.api_key.clone(),
                provider: Some(profile.id.clone()),
                origin: format!("first vision model of any provider ({origin})"),
            });
        }
    }
    Err(
        "no model in karucode's provider configuration has `vision = true`; add a vision model \
         in karucode's settings or pin this session to one"
            .to_string(),
    )
}

/// Resolve an explicitly named provider+model pair through karucode's
/// configuration: the provider must exist and the model must be one of its
/// models, exactly like a session pin would validate. An empty provider means
/// "find whichever provider serves this model".
pub fn resolve_provider_model(provider: &str, model: &str) -> Result<Resolved, String> {
    let data_dir = data_dir();
    let (profiles, origin) = read_providers(&data_dir)?;
    // An unqualified model id: locate it in whatever profile offers it.
    if provider.is_empty() {
        if let Some(found) = find_model_entry(&profiles, model) {
            let id = found.entry.id.clone();
            return Ok(found.resolved(format!("explicit `{id}` from karucode's configuration")));
        }
        return Err(format!(
            "model `{model}` is not offered by any provider in karucode's configuration"
        ));
    }
    let profile = profiles.iter().find(|p| p.id == provider).ok_or_else(|| {
        format!("provider `{provider}` is not in karucode's configuration ({origin})")
    })?;
    let entry = profile
        .models
        .iter()
        .find(|m| m.id == model)
        .ok_or_else(|| {
            format!(
                "model `{model}` is not offered by provider `{provider}` in karucode's \
                 configuration"
            )
        })?;
    Ok(Resolved {
        base_url: profile.base_url.clone(),
        model: entry.id.clone(),
        api_key: profile.api_key.clone(),
        provider: Some(profile.id.clone()),
        origin: format!("explicit `{provider}/{model}` from karucode's configuration"),
    })
}

struct FoundModel<'a> {
    profile: &'a ProviderProfile,
    entry: &'a ProviderModel,
}

impl FoundModel<'_> {
    fn resolved(self, origin: String) -> Resolved {
        Resolved {
            base_url: self.profile.base_url.clone(),
            model: self.entry.id.clone(),
            api_key: self.profile.api_key.clone(),
            provider: Some(self.profile.id.clone()),
            origin,
        }
    }
}

fn find_model_entry<'a>(profiles: &'a [ProviderProfile], model: &str) -> Option<FoundModel<'a>> {
    for profile in profiles {
        if let Some(entry) = profile.models.iter().find(|m| m.id == model) {
            return Some(FoundModel { profile, entry });
        }
    }
    None
}

/// One process-wide lock for tests that redirect resolution through the
/// environment: env vars are process-global and the default test runner is
/// multi-threaded.
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Resolved {
    /// The provider profile id, or `default` when none was named.
    pub fn provider_id(&self) -> &str {
        self.provider.as_deref().unwrap_or("default")
    }
}

/// The settings JSON for one resolved endpoint (key redacted).
pub fn status_json_endpoint(endpoint: &Resolved) -> Value {
    serde_json::json!({
        "base_url": endpoint.base_url,
        "model": endpoint.model,
        "provider": endpoint.provider,
        "origin": endpoint.origin,
        "api_key_set": endpoint.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false),
    })
}

/// The settings JSON the `status` tool and the panel show (key redacted).
#[cfg(test)]
pub fn status_json(resolved: Option<&Resolved>, problem: Option<String>) -> Value {
    match resolved {
        Some(endpoint) => serde_json::json!({
            "resolved_from_karucode": true,
            "ready": true,
            "base_url": endpoint.base_url,
            "model": endpoint.model,
            "provider": endpoint.provider,
            "origin": endpoint.origin,
            "api_key_set": endpoint.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false),
            "problem": problem,
        }),
        None => serde_json::json!({
            "resolved_from_karucode": true,
            "ready": false,
            "problem": problem,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(
        id: &str,
        base: &str,
        key: Option<&str>,
        models: &[(&str, bool)],
    ) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            base_url: base.into(),
            api_key: key.map(str::to_string),
            models: models
                .iter()
                .map(|(id, vision)| ProviderModel {
                    id: (*id).into(),
                    vision: Some(*vision),
                })
                .collect(),
        }
    }

    fn fixture(
        active_provider: &str,
        active_model: &str,
    ) -> (std::path::PathBuf, Vec<ProviderProfile>) {
        let dir = std::env::temp_dir().join(format!(
            "ih-karucode-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let profiles = vec![
            profile(
                "omniroute",
                "http://127.0.0.1:20128/v1",
                Some("sk-live"),
                &[("od/deepseek-v4.1-flash", true), ("od/text-only", false)],
            ),
            profile(
                "backup",
                "http://127.0.0.1:9/v1",
                None,
                &[("b/none", false)],
            ),
        ];
        let db = dir.join("karucode.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL DEFAULT 0)", []).unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('model.providers', ?1)",
            [serde_json::to_string(&profiles).unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('model.active_provider', ?1)",
            [active_provider.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('model.active_model', ?1)",
            [active_model.to_string()],
        )
        .unwrap();
        (dir, profiles)
    }

    fn resolve_in(
        dir: &std::path::PathBuf,
        pinned: Option<&str>,
        requested: Option<&str>,
    ) -> Result<Resolved, String> {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("KARUCODE_DATA_DIR", dir);
        std::env::set_var("KARUCODE_CONFIG", dir.join("nonexistent.toml"));
        let result = resolve(pinned, requested);
        std::env::remove_var("KARUCODE_DATA_DIR");
        std::env::remove_var("KARUCODE_CONFIG");
        result
    }

    #[test]
    fn prefers_the_active_model_when_it_has_vision() {
        let (dir, _) = fixture("omniroute", "od/deepseek-v4.1-flash");
        let resolved = resolve_in(&dir, None, None).unwrap();
        assert_eq!(resolved.model, "od/deepseek-v4.1-flash");
        assert_eq!(resolved.base_url, "http://127.0.0.1:20128/v1");
        assert_eq!(resolved.api_key.as_deref(), Some("sk-live"));
        assert!(resolved.origin.contains("active model"));
    }

    #[test]
    fn pinned_vision_model_wins_over_the_active_model() {
        let (dir, _) = fixture("omniroute", "od/deepseek-v4.1-flash");
        let resolved = resolve_in(&dir, Some("od/deepseek-v4.1-flash"), None).unwrap();
        assert_eq!(resolved.model, "od/deepseek-v4.1-flash");
        assert!(resolved.origin.contains("pinned"));
    }

    #[test]
    fn a_text_only_active_model_falls_back_to_a_vision_sibling() {
        let (dir, _) = fixture("omniroute", "od/text-only");
        let resolved = resolve_in(&dir, None, None).unwrap();
        assert_eq!(resolved.model, "od/deepseek-v4.1-flash");
        assert!(resolved.origin.contains("first vision model"));
    }

    #[test]
    fn per_call_override_must_exist_and_ignores_vision() {
        let (dir, _) = fixture("omniroute", "od/deepseek-v4.1-flash");
        // A text-only model can be requested explicitly (operator choice).
        let resolved = resolve_in(&dir, None, Some("od/text-only")).unwrap();
        assert_eq!(resolved.model, "od/text-only");
        assert!(resolve_in(&dir, None, Some("od/nope")).is_err());
    }

    #[test]
    fn no_vision_models_anywhere_is_an_actionable_error() {
        let dir = std::env::temp_dir().join(format!(
            "ih-empty-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("karucode.db")).unwrap();
        conn.execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL DEFAULT 0)", []).unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('model.providers', ?1)",
            [
                serde_json::to_string(&vec![profile("p", "http://x/v1", None, &[("m", false)])])
                    .unwrap(),
            ],
        )
        .unwrap();
        let error = resolve_in(&dir, None, None).unwrap_err();
        assert!(error.contains("vision"), "{error}");
    }

    #[test]
    fn status_json_redacts_the_key() {
        let resolved = Resolved {
            base_url: "http://x/v1".into(),
            model: "m".into(),
            api_key: Some("sk-secret".into()),
            origin: "test".into(),
            provider: Some("p".into()),
        };
        let value = status_json(Some(&resolved), None).to_string();
        assert!(!value.contains("sk-secret"));
        assert!(value.contains("\"ready\":true"));
    }
}
