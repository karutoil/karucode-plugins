//! Durable-in-memory job state and the evidence/coverage model.
//!
//! A search hit is a [`Candidate`], never evidence. It becomes a [`Source`] only
//! after fetch_batch returns page text and the relevance checks accept that text
//! for at least one aspect. This distinction is what stops the engine declaring
//! victory because snippets happened to look plausible.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_MAX_ROUNDS: usize = 12;
pub const DEFAULT_MAX_SEARCHES: usize = 36;
pub const DEFAULT_MAX_SOURCES: usize = 24;
pub const DEFAULT_MIN_SOURCES: usize = 8;
pub const DEFAULT_MIN_DOMAINS: usize = 3;
pub const DEFAULT_MIN_ROUNDS: usize = 3;
pub const DEFAULT_MAX_MINUTES: u64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResearchOptions {
    pub max_rounds: usize,
    pub max_searches: usize,
    pub max_sources: usize,
    pub min_sources: usize,
    pub min_domains: usize,
    pub min_rounds: usize,
    pub max_minutes: u64,
    pub results_per_search: usize,
    /// Bypass both search and page caches when fresh retrieval matters more than
    /// speed. False deliberately shares free-search's ordinary cache.
    pub force_refresh: bool,
    pub freshness: Option<String>,
    pub category: Option<String>,
    pub engines: Vec<String>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    /// Internal breadth controls are settings, not model knobs: three searches and
    /// one batch of up to four fetches per round make progress observable without
    /// turning a round into an unbounded operation.
    #[serde(skip)]
    pub queries_per_round: usize,
    #[serde(skip)]
    pub fetches_per_round: usize,
}

impl Default for ResearchOptions {
    fn default() -> Self {
        Self {
            max_rounds: DEFAULT_MAX_ROUNDS,
            max_searches: DEFAULT_MAX_SEARCHES,
            max_sources: DEFAULT_MAX_SOURCES,
            min_sources: DEFAULT_MIN_SOURCES,
            min_domains: DEFAULT_MIN_DOMAINS,
            min_rounds: DEFAULT_MIN_ROUNDS,
            max_minutes: DEFAULT_MAX_MINUTES,
            results_per_search: 8,
            force_refresh: false,
            freshness: None,
            category: None,
            engines: Vec::new(),
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            queries_per_round: 3,
            fetches_per_round: 4,
        }
    }
}

impl ResearchOptions {
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let mut o = Self::default();
        o.max_rounds = bounded_usize(args, "max_rounds", o.max_rounds, 1, 40)?;
        o.max_searches = bounded_usize(args, "max_searches", o.max_searches, 1, 120)?;
        o.max_sources = bounded_usize(args, "max_sources", o.max_sources, 1, 60)?;
        o.min_sources = bounded_usize(args, "min_sources", o.min_sources, 1, 60)?;
        o.min_domains = bounded_usize(args, "min_domains", o.min_domains, 1, 20)?;
        o.min_rounds = bounded_usize(args, "min_rounds", o.min_rounds, 1, 40)?;
        o.max_minutes = bounded_usize(args, "max_minutes", o.max_minutes as usize, 1, 180)? as u64;
        o.results_per_search = bounded_usize(args, "results_per_search", 8, 3, 20)?;
        o.force_refresh = match args.get("force_refresh") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err("force_refresh must be a boolean".into()),
        };
        o.freshness = optional_string(args, "freshness")?;
        if let Some(freshness) = o.freshness.as_deref() {
            if !["day", "week", "month", "year"].contains(&freshness) {
                return Err("freshness must be one of day, week, month, year".into());
            }
        }
        o.category = optional_string(args, "category")?;
        o.engines = string_list(args, "engines")?;
        o.include_domains = string_list(args, "include_domains")?;
        o.exclude_domains = string_list(args, "exclude_domains")?;
        if o.min_sources > o.max_sources {
            return Err(format!(
                "min_sources ({}) cannot exceed max_sources ({})",
                o.min_sources, o.max_sources
            ));
        }
        if o.min_rounds > o.max_rounds {
            return Err(format!(
                "min_rounds ({}) cannot exceed max_rounds ({})",
                o.min_rounds, o.max_rounds
            ));
        }
        if o.min_domains > o.min_sources {
            return Err(format!(
                "min_domains ({}) cannot exceed min_sources ({})",
                o.min_domains, o.min_sources
            ));
        }
        Ok(o)
    }
}

fn bounded_usize(
    args: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, String> {
    let Some(value) = args.get(key).filter(|v| !v.is_null()) else {
        return Ok(default);
    };
    let number = value
        .as_u64()
        .ok_or_else(|| format!("{key} must be an integer"))?;
    let number = usize::try_from(number).map_err(|_| format!("{key} is too large"))?;
    if !(min..=max).contains(&number) {
        return Err(format!("{key} must be between {min} and {max}"));
    }
    Ok(number)
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    let Some(value) = args.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = value
        .as_str()
        .ok_or_else(|| format!("{key} must be a string"))?
        .trim();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text.to_string()))
    }
}

fn string_list(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = args.get(key).filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| format!("{key} must be an array of strings"))?;
    let mut result = Vec::new();
    for value in values {
        let text = value
            .as_str()
            .ok_or_else(|| format!("{key} must contain only strings"))?
            .trim();
        if !text.is_empty() && !result.iter().any(|existing| existing == text) {
            result.push(text.to_string());
        }
    }
    Ok(result)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Cancelling,
    Complete,
    Partial,
    Failed,
    Cancelled,
}

impl RunState {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Partial | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aspect {
    pub id: String,
    pub label: String,
    pub seed_query: String,
    pub attempts: usize,
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub url: String,
    pub title: String,
    pub aspect_ids: Vec<String>,
    pub discovered_round: usize,
    pub via: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub aspect_id: String,
    /// Verbatim page prose with whitespace collapsed; never a search snippet or
    /// generated summary. The source URL alongside it is the citation.
    pub excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub url: String,
    pub title: String,
    pub domain: String,
    pub published_date: Option<String>,
    pub retrieved_at: Option<String>,
    pub method: Option<String>,
    pub cache_age_seconds: Option<u64>,
    pub content_chars: usize,
    /// Non-cryptographic fingerprint of normalized parsed content, used only to
    /// keep mirrored text from satisfying the cross-source core requirement twice.
    pub content_hash: String,
    pub truncated: bool,
    pub aspect_ids: Vec<String>,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRecord {
    pub round: usize,
    pub aspect_id: String,
    pub query: String,
    pub hits: usize,
    pub new_candidates: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchFailure {
    pub round: usize,
    pub url: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub round: usize,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub question: String,
    pub session_id: Option<String>,
    pub workspace: Option<String>,
    pub aspects: Vec<Aspect>,
    pub options: ResearchOptions,
    pub state: RunState,
    pub stop_reason: Option<String>,
    pub round: usize,
    pub searches: usize,
    pub sources: Vec<Source>,
    pub candidates: Vec<Candidate>,
    pub seen_urls: BTreeSet<String>,
    pub queries: Vec<QueryRecord>,
    pub failures: Vec<FetchFailure>,
    pub activity: Vec<Activity>,
    pub cancel_requested: bool,
    pub rejected_pages: usize,
    pub revision: u64,
    pub started: Instant,
    pub created_unix_secs: u64,
}

impl Job {
    pub fn new(
        id: String,
        question: String,
        aspects: Vec<Aspect>,
        options: ResearchOptions,
        session_id: Option<String>,
        workspace: Option<String>,
    ) -> Self {
        Self {
            id,
            question,
            session_id,
            workspace,
            aspects,
            options,
            state: RunState::Queued,
            stop_reason: None,
            round: 0,
            searches: 0,
            sources: Vec::new(),
            candidates: Vec::new(),
            seen_urls: BTreeSet::new(),
            queries: Vec::new(),
            failures: Vec::new(),
            activity: Vec::new(),
            cancel_requested: false,
            rejected_pages: 0,
            revision: 0,
            started: Instant::now(),
            created_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn domains(&self) -> BTreeSet<&str> {
        self.sources
            .iter()
            .map(|s| s.domain.as_str())
            .filter(|d| !d.is_empty())
            .collect()
    }

    pub fn core_evidence_groups(&self) -> Vec<(String, String)> {
        let mut domains = BTreeSet::new();
        let mut contents = BTreeSet::new();
        let mut groups = Vec::new();
        for source in self
            .sources
            .iter()
            .filter(|s| s.aspect_ids.iter().any(|a| a == "core"))
            .filter(|s| !s.domain.is_empty())
        {
            // A group consumes both its host and its normalized-content marker:
            // mirrors on new hosts and extra pages on one host cannot inflate it.
            if domains.insert(source.domain.clone()) && contents.insert(source.content_hash.clone())
            {
                groups.push((source.domain.clone(), source.content_hash.clone()));
            }
        }
        groups
    }

    pub fn record(&mut self, message: impl Into<String>) {
        self.activity.push(Activity {
            round: self.round,
            message: message.into(),
        });
        // A long run can keep a useful recent trail without unbounded state.
        if self.activity.len() > 100 {
            self.activity.remove(0);
        }
    }

    pub fn coverage(&self) -> Coverage {
        let uncovered: Vec<String> = self
            .aspects
            .iter()
            .filter(|a| a.source_ids.is_empty())
            .map(|a| a.id.clone())
            .collect();
        let domains = self.domains().len();
        let core_groups = self.core_evidence_groups().len();
        let rounds_met = self.round >= self.options.min_rounds;
        let sources_met = self.sources.len() >= self.options.min_sources;
        let domains_met = domains >= self.options.min_domains;
        let aspects_met = uncovered.is_empty();
        // One host or one mirrored document repeated many times is not cross-source
        // coverage. Require two distinct (host, content fingerprint) groups. This
        // is a diversity guard, not a claim of factual or organizational independence.
        let corroboration_met = core_groups >= 2.min(self.options.min_sources);
        Coverage {
            met: rounds_met && sources_met && domains_met && aspects_met && corroboration_met,
            rounds_met,
            sources_met,
            domains_met,
            aspects_met,
            corroboration_met,
            uncovered,
        }
    }

    pub fn status_json(&self) -> Value {
        let coverage = self.coverage();
        json!({
            "id": self.id,
            "question": self.question,
            "state": self.state,
            "terminal": self.state.terminal(),
            "stop_reason": self.stop_reason,
            "cancel_requested": self.cancel_requested,
            "round": self.round,
            "elapsed_seconds": self.elapsed().as_secs(),
            "created_unix_secs": self.created_unix_secs,
            "searches": self.searches,
            "sources_read": self.sources.len(),
            "distinct_domains": self.domains().len(),
            "core_evidence_groups": self.core_evidence_groups().len(),
            "pending_candidates": self.candidates.len(),
            "rejected_pages": self.rejected_pages,
            "fetch_failures": self.failures.len(),
            "revision": self.revision,
            "requirements": {
                "min_rounds": self.options.min_rounds,
                "min_sources": self.options.min_sources,
                "min_domains": self.options.min_domains,
                "max_rounds": self.options.max_rounds,
                "max_searches": self.options.max_searches,
                "max_sources": self.options.max_sources,
                "max_minutes": self.options.max_minutes,
            },
            "coverage": {
                "met": coverage.met,
                "rounds_met": coverage.rounds_met,
                "sources_met": coverage.sources_met,
                "domains_met": coverage.domains_met,
                "aspects_met": coverage.aspects_met,
                "core_cross_source_met": coverage.corroboration_met,
                "uncovered_aspects": coverage.uncovered,
            },
            "aspects": self.aspects.iter().map(|a| json!({
                "id": a.id,
                "label": a.label,
                "attempts": a.attempts,
                "sources": a.source_ids.len(),
                "covered": !a.source_ids.is_empty(),
            })).collect::<Vec<_>>(),
            "recent_activity": self.activity.iter().rev().take(8).rev().collect::<Vec<_>>(),
            "next": if self.state.terminal() {
                "Call report with this id; use its read-source evidence and unresolved gaps."
            } else {
                "Call status with this id and wait_secs=45. Do not start a duplicate job."
            },
        })
    }

    /// Machine report. Candidates are deliberately omitted: unread search hits
    /// must not leak into an evidence API and later be mistaken for read sources.
    pub fn evidence_json(&self) -> Value {
        let coverage = self.coverage();
        json!({
            "id": self.id,
            "question": self.question,
            "state": self.state,
            "terminal": self.state.terminal(),
            "stop_reason": self.stop_reason,
            "elapsed_seconds": self.elapsed().as_secs(),
            "rounds": self.round,
            "searches": self.searches,
            "requirements": self.options,
            "coverage": {
                "met": coverage.met,
                "uncovered_aspects": coverage.uncovered,
                "sources_read": self.sources.len(),
                "distinct_domains": self.domains().len(),
                "core_evidence_groups": self.core_evidence_groups().len(),
            },
            "aspects": self.aspects,
            "sources": self.sources,
            "fetch_failures": self.failures,
            "search_audit": self.queries,
            "activity": self.activity,
            "evidence_rule": "Every whitespace-normalized excerpt comes from fetched parsed text. Search snippets and unread candidates are excluded; lexical relevance and domain/content diversity do not prove factual independence.",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub met: bool,
    pub rounds_met: bool,
    pub sources_met: bool,
    pub domains_met: bool,
    pub aspects_met: bool,
    pub corroboration_met: bool,
    pub uncovered: Vec<String>,
}

#[derive(Default)]
struct StoreInner {
    jobs: BTreeMap<String, Job>,
    revision: u64,
}

/// Process-local job registry. The condvar lets status wait for real progress
/// rather than encouraging the agent to issue a tight sequence of empty polls.
pub struct Store {
    inner: Mutex<StoreInner>,
    changed: Condvar,
}

impl Store {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StoreInner::default()),
            changed: Condvar::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .get(id)
            .cloned()
    }

    pub fn insert(&self, job: Job, max_active: usize) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let active = inner.jobs.values().filter(|j| !j.state.terminal()).count();
        if active >= max_active {
            return Err(format!(
                "there are already {active} active research jobs (limit {max_active}); wait for one, cancel it, or list jobs"
            ));
        }
        if inner.jobs.len() >= 64 {
            let oldest = inner
                .jobs
                .values()
                .filter(|j| j.state.terminal())
                .min_by_key(|j| j.created_unix_secs)
                .map(|j| j.id.clone());
            if let Some(oldest) = oldest {
                inner.jobs.remove(&oldest);
            } else {
                return Err("the 64-job in-memory registry is full; reload the plugin after collecting reports".into());
            }
        }
        inner.jobs.insert(job.id.clone(), job);
        inner.revision += 1;
        self.changed.notify_all();
        Ok(())
    }

    /// Publish worker progress while preserving a cancellation that may have
    /// arrived while the backend call was in flight.
    pub fn publish(&self, job: &Job) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(current) = inner.jobs.get_mut(&job.id) {
            let cancel_requested = current.cancel_requested || job.cancel_requested;
            let next_revision = current.revision + 1;
            *current = job.clone();
            current.cancel_requested = cancel_requested;
            if cancel_requested && !current.state.terminal() {
                current.state = RunState::Cancelling;
            }
            current.revision = next_revision;
            inner.revision += 1;
            self.changed.notify_all();
        }
    }

    pub fn cancel(&self, id: &str) -> Option<Job> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let changed = {
            let job = inner.jobs.get_mut(id)?;
            if !job.state.terminal() {
                job.cancel_requested = true;
                job.state = RunState::Cancelling;
                job.record(
                    "Cancellation requested; the current backend call will finish or time out first",
                );
                job.revision += 1;
                true
            } else {
                false
            }
        };
        if changed {
            inner.revision += 1;
            self.changed.notify_all();
        }
        Some(inner.jobs.get(id)?.clone())
    }

    pub fn is_cancel_requested(&self, id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .get(id)
            .map(|j| j.cancel_requested)
            .unwrap_or(true)
    }

    pub fn wait_for_progress(&self, id: &str, observed: u64, wait: Duration) -> Option<Job> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + wait;
        loop {
            let job = inner.jobs.get(id)?;
            if job.revision != observed || job.state.terminal() {
                return Some(job.clone());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Some(job.clone());
            }
            let (next, _) = self
                .changed
                .wait_timeout(inner, remaining)
                .unwrap_or_else(|e| e.into_inner());
            inner = next;
        }
    }

    pub fn list(&self) -> Vec<Job> {
        let mut jobs: Vec<_> = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .values()
            .cloned()
            .collect();
        jobs.sort_by_key(|j| j.created_unix_secs);
        jobs.reverse();
        jobs
    }

    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for job in inner.jobs.values_mut() {
            if !job.state.terminal() {
                job.cancel_requested = true;
            }
        }
        inner.revision += 1;
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_reject_impossible_or_invalid_budgets() {
        let args = json!({"max_sources": 2, "min_sources": 3});
        assert!(ResearchOptions::from_args(&args)
            .unwrap_err()
            .contains("min_sources"));
        let args = json!({"max_rounds": 2, "min_rounds": 3});
        assert!(ResearchOptions::from_args(&args)
            .unwrap_err()
            .contains("min_rounds"));
        assert!(ResearchOptions::from_args(&json!({"freshness":"forever"})).is_err());
        assert!(ResearchOptions::from_args(&json!({"engines":"bing"})).is_err());
    }

    #[test]
    fn unread_candidates_and_one_domain_do_not_satisfy_coverage() {
        let options = ResearchOptions {
            min_sources: 2,
            min_domains: 2,
            min_rounds: 1,
            ..ResearchOptions::default()
        };
        let aspects = vec![Aspect {
            id: "core".into(),
            label: "Core".into(),
            seed_query: "x".into(),
            attempts: 0,
            source_ids: vec![],
        }];
        let mut job = Job::new("r".into(), "x".into(), aspects, options, None, None);
        job.candidates.push(Candidate {
            url: "https://one.example".into(),
            title: "Looks relevant".into(),
            aspect_ids: vec!["core".into()],
            discovered_round: 1,
            via: "search".into(),
        });
        job.round = 1;
        assert!(!job.coverage().met, "an unread candidate is not evidence");
        job.sources.push(Source {
            id: "S1".into(),
            url: "https://one.example".into(),
            title: "One".into(),
            domain: "one.example".into(),
            published_date: None,
            retrieved_at: None,
            method: None,
            cache_age_seconds: None,
            content_chars: 1000,
            content_hash: "one".into(),
            truncated: false,
            aspect_ids: vec!["core".into()],
            evidence: vec![],
        });
        job.aspects[0].source_ids.push("S1".into());
        assert!(
            !job.coverage().met,
            "core corroboration still needs a second domain"
        );
    }

    #[test]
    fn status_wait_wakes_on_terminal_progress_and_terminal_cancel_is_idempotent() {
        let store = std::sync::Arc::new(Store::new());
        let mut job = Job::new(
            "r".into(),
            "Rust".into(),
            vec![],
            ResearchOptions::default(),
            None,
            None,
        );
        job.state = RunState::Running;
        store.insert(job.clone(), 1).unwrap();
        let revision = store.get("r").unwrap().revision;
        let worker_store = std::sync::Arc::clone(&store);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let mut job = worker_store.get("r").unwrap();
            job.state = RunState::Partial;
            worker_store.publish(&job);
            worker_store.cancel("r").unwrap()
        });
        let snapshot = store
            .wait_for_progress("r", revision, Duration::from_secs(2))
            .unwrap();
        assert_eq!(snapshot.state, RunState::Partial);
        assert!(worker.join().unwrap().state == RunState::Partial);
        assert!(!store.get("r").unwrap().cancel_requested);
    }

    #[test]
    fn cancellation_survives_an_in_flight_progress_publish() {
        let store = Store::new();
        let job = Job::new(
            "r".into(),
            "x".into(),
            vec![],
            ResearchOptions::default(),
            None,
            None,
        );
        store.insert(job.clone(), 1).unwrap();
        let cancelled_revision = store.cancel("r").unwrap().revision;
        let mut worker_copy = job;
        worker_copy.state = RunState::Running;
        store.publish(&worker_copy);
        let first = store.get("r").unwrap();
        assert!(first.cancel_requested);
        assert_eq!(first.state, RunState::Cancelling);
        assert!(first.revision > cancelled_revision);
        store.publish(&worker_copy);
        assert!(store.get("r").unwrap().revision > first.revision);
    }
}
