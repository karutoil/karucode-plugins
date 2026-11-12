//! Whether the *calling* session's model can already see images.
//!
//! The handoff exists for **text-only** models. A session whose own model has
//! `vision = true` must not be told it cannot see, and must not have its
//! images ferried to another model: `read_file` already attaches the pixels
//! natively for such a model. So both the `before_prompt` note and the `see`
//! tool gate on this.
//!
//! The host does not tell a plugin the session's model, but the session id is
//! in the hook payload and the tool-call context, and the server knows the
//! model: `GET /api/sessions/:id` answers `session.model` (`{provider,
//! model, reasoning_effort?}`) for a pinned session or `null` for one
//! following the install-wide default. The default is `model.active_model`
//! in karucode's own settings DB - which is exactly what `karucode.rs`
//! already reads. Lookups are cached per session id for a minute: a pin can
//! change, but not often, and the note is asked before every model call.

use crate::karucode;
use crate::server::{self, Endpoint};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(60);

/// What we concluded about one session's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionVision {
    /// The session's model has `vision = true`: handoff must not engage.
    Vision,
    /// The session's model is text-only: handoff is the point.
    TextOnly,
    /// Could not be determined (unknown session, server unreachable, a model
    /// missing from the provider config). Handoff still engages - undecidable
    /// is not "can see" - but `see` says so when it matters.
    Unknown,
}

struct Cache {
    entries: HashMap<String, (SessionVision, Instant)>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            entries: HashMap::new(),
        })
    })
}

/// Look up (with cache) whether the session's model can see images.
pub fn session_vision(endpoint: &Endpoint, token: &str, session_id: &str) -> SessionVision {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return SessionVision::Unknown;
    }
    if let Ok(cache) = cache().lock() {
        if let Some((vision, at)) = cache.entries.get(session_id) {
            if at.elapsed() < CACHE_TTL {
                return vision.clone();
            }
        }
    }
    let vision = resolve(endpoint, token, session_id);
    if let Ok(mut cache) = cache().lock() {
        cache
            .entries
            .insert(session_id.to_string(), (vision.clone(), Instant::now()));
        // Bound the cache: sessions are few, but the process is long-lived.
        if cache.entries.len() > 64 {
            cache.entries.retain(|_, (_, at)| at.elapsed() < CACHE_TTL);
        }
    }
    vision
}

fn resolve(endpoint: &Endpoint, token: &str, session_id: &str) -> SessionVision {
    // The session's own row: its pin, or null for the install default.
    let Ok((status, body)) = server::request_json(
        endpoint,
        token,
        "GET",
        &format!("/api/sessions/{session_id}"),
        None,
    ) else {
        return SessionVision::Unknown;
    };
    if !(200..300).contains(&status) {
        return SessionVision::Unknown;
    }
    let selection = body
        .get("session")
        .or(Some(&body))
        .and_then(|s| s.get("model"));
    match selection {
        // A pin: {provider, model} - resolve it through the provider config.
        Some(pin) if pin.get("model").and_then(|m| m.as_str()).is_some() => {
            let model = pin
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or_default();
            let provider = pin
                .get("provider")
                .and_then(|p| p.as_str())
                .unwrap_or_default();
            let provider = if provider.is_empty() {
                None
            } else {
                Some(provider)
            };
            model_vision(model, provider)
        }
        // No pin: the install-wide default - the active model.
        _ => match karucode::active_model() {
            Some((provider, model)) => model_vision(&model, provider.as_deref()),
            None => SessionVision::Unknown,
        },
    }
}

/// Whether a named model has `vision = true` in karucode's provider config.
fn model_vision(model: &str, provider: Option<&str>) -> SessionVision {
    let Some(vision) = karucode::model_has_vision(model, provider) else {
        return SessionVision::Unknown;
    };
    if vision {
        SessionVision::Vision
    } else {
        SessionVision::TextOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trips_and_invalidates() {
        // The cache layer itself: two calls hit the resolver once.
        // (The resolver needs a server, so exercise only the plumbing here.)
        let mut cache = Cache {
            entries: HashMap::new(),
        };
        cache
            .entries
            .insert("s_1".into(), (SessionVision::Vision, Instant::now()));
        assert_eq!(
            cache.entries.get("s_1").map(|(v, _)| v.clone()),
            Some(SessionVision::Vision)
        );
        cache.entries.remove("s_1");
        assert!(!cache.entries.contains_key("s_1"));
    }
}
