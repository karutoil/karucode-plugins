//! Iterative research loop: plan → search → read → assess → find gaps → repeat.
//!
//! The observer is called at every operation boundary. In the plugin it publishes
//! a snapshot and returns whether cancellation was requested; in tests it is a
//! deterministic control hook. No network wait holds the job-store lock.

use crate::evidence::{assess, content_fingerprint, domain_of, normalise_url, outbound_links};
use crate::gateway::{FetchPage, ResearchGateway, SearchHit};
use crate::model::{Candidate, FetchFailure, Job, QueryRecord, RunState, Source};
use crate::plan::{next_query, MAX_QUERY_ATTEMPTS};
use std::time::Duration;

const STALLED_ROUND_LIMIT: usize = 3;
const MAX_PENDING_CANDIDATES: usize = 200;
const MAX_CONSECUTIVE_CALL_ERRORS: usize = 3;

pub fn execute<G, F>(mut job: Job, gateway: &mut G, mut observe: F) -> Job
where
    G: ResearchGateway,
    F: FnMut(&Job) -> bool,
{
    job.state = RunState::Running;
    job.record(format!(
        "Started: {} aspects, requiring {} read sources across {} domains and at least {} rounds",
        job.aspects.len(),
        job.options.min_sources,
        job.options.min_domains,
        job.options.min_rounds
    ));
    if observe(&job) {
        return cancelled(job, "Cancelled before the first search");
    }

    let mut consecutive_errors = 0;
    loop {
        if terminal_for_budget(&mut job) {
            break;
        }
        job.round += 1;
        let sources_at_round_start = job.sources.len();
        job.record(format!("Round {}: selecting gap-driven queries", job.round));
        if observe(&job) {
            return cancelled(job, "Cancelled between rounds");
        }

        let aspect_indices = aspects_to_search(&job);
        let mut queries_this_round = 0;
        for index in aspect_indices {
            if queries_this_round >= job.options.queries_per_round
                || job.searches >= job.options.max_searches
                || deadline_exceeded(&job)
            {
                break;
            }
            let Some(query) = next_query(&job.aspects[index], &job.question) else {
                continue;
            };
            job.aspects[index].attempts += 1;
            job.searches += 1;
            queries_this_round += 1;
            let aspect_id = job.aspects[index].id.clone();
            job.record(format!("Search {} for {aspect_id}: {query}", job.searches));
            // Publish the attempted query before waiting on the network, so status
            // shows what the worker is doing rather than the previous round only.
            if observe(&job) {
                return cancelled(job, "Cancelled before a search request");
            }
            let searched = gateway.search(&query, &job.options);
            let mut record = QueryRecord {
                round: job.round,
                aspect_id: aspect_id.clone(),
                query: query.clone(),
                hits: 0,
                new_candidates: 0,
                error: None,
            };
            match searched {
                Ok(response) => {
                    consecutive_errors = 0;
                    record.hits = response.hits.len();
                    let mut added = 0;
                    for SearchHit { title, url } in response.hits {
                        added += usize::from(add_candidate(
                            &mut job,
                            url,
                            title,
                            vec![aspect_id.clone()],
                            format!("search: {query}"),
                        ));
                    }
                    record.new_candidates = added;
                    job.record(format!(
                        "Search returned {} hits ({} new URLs; engines: {}{})",
                        record.hits,
                        added,
                        response.engines.join(","),
                        if response.cached { ", cache hit" } else { "" }
                    ));
                }
                Err(error) => {
                    consecutive_errors += 1;
                    record.error = Some(error.clone());
                    job.record(format!("Search failed: {error}"));
                }
            }
            job.queries.push(record);
            if observe(&job) {
                return cancelled(job, "Cancelled after a search response");
            }
            if consecutive_errors >= MAX_CONSECUTIVE_CALL_ERRORS {
                return failed_after_errors(job);
            }
        }

        let selected = take_candidates(&mut job);
        if !selected.is_empty() {
            let urls: Vec<String> = selected.iter().map(|c| c.url.clone()).collect();
            job.record(format!(
                "Reading {} candidate URLs with {}",
                urls.len(),
                if job.options.force_refresh {
                    "fresh single-URL fetch calls"
                } else {
                    "fetch_batch"
                }
            ));
            if observe(&job) {
                return cancelled(job, "Cancellation requested before source fetching");
            }
            match gateway.fetch(&urls, &job.options) {
                Ok(outcomes) => {
                    consecutive_errors = 0;
                    for outcome in outcomes {
                        match outcome.result {
                            Ok(page) => accept_page(&mut job, page, &outcome.requested_url),
                            Err(error) => {
                                job.failures.push(FetchFailure {
                                    round: job.round,
                                    url: outcome.requested_url.clone(),
                                    error: error.clone(),
                                });
                                job.record(format!(
                                    "Could not read {}: {error}",
                                    outcome.requested_url
                                ));
                            }
                        }
                    }
                }
                Err(error_) => {
                    consecutive_errors += 1;
                    for url in urls {
                        job.failures.push(FetchFailure {
                            round: job.round,
                            url: url.clone(),
                            error: error_.clone(),
                        });
                    }
                    job.record(format!("Source batch failed: {error_}"));
                }
            }
            if observe(&job) {
                return cancelled(job, "Cancellation requested after source fetching");
            }
            if consecutive_errors >= MAX_CONSECUTIVE_CALL_ERRORS {
                return failed_after_errors(job);
            }
        }

        let new_sources = job.sources.len().saturating_sub(sources_at_round_start);
        job.record(format!(
            "Round {} complete: {} new read sources; {} total across {} domains",
            job.round,
            new_sources,
            job.sources.len(),
            job.domains().len()
        ));

        if job.coverage().met {
            job.state = RunState::Complete;
            job.stop_reason = Some(format!(
                "Coverage satisfied after {} rounds and {} searches: {} fetched sources across {} domains; every aspect has evidence and the core has distinct domain/content coverage",
                job.round,
                job.searches,
                job.sources.len(),
                job.domains().len()
            ));
            job.record(job.stop_reason.clone().unwrap());
            observe(&job);
            return job;
        }

        if terminal_for_budget(&mut job) {
            observe(&job);
            return job;
        }

        if new_sources == 0 {
            // One no-progress round is normal (a dead URL); three mean the current
            // evidence frontier is not improving. By then every uncovered aspect
            // has had at least two genuinely different searches when possible.
            let all_uncovered_retried = job
                .aspects
                .iter()
                .filter(|a| a.source_ids.is_empty())
                .all(|a| a.attempts >= 2 || a.attempts >= MAX_QUERY_ATTEMPTS);
            let previous = error_activities_in_recent_rounds(&job);
            let stalled = previous + 1;
            if stalled >= STALLED_ROUND_LIMIT && all_uncovered_retried {
                return partial(job, format!(
                    "Stopped after {stalled} consecutive rounds with no new relevant sources; unresolved coverage gaps remain"
                ));
            }
        }

        if queries_this_round == 0 && selected.is_empty() {
            return partial(
                job,
                "Stopped: all query variants and discovered source candidates are exhausted".into(),
            );
        }
    }
    job
}

fn error_activities_in_recent_rounds(job: &Job) -> usize {
    // Count trailing zero-source round markers rather than storing another piece
    // of mutable state. The marker includes the exact phrase emitted above.
    job.activity
        .iter()
        .rev()
        .filter(|activity| {
            activity.message.starts_with("Round ") && activity.message.contains(" complete:")
        })
        .take_while(|activity| activity.message.contains(": 0 new read sources"))
        .count()
        .saturating_sub(1)
}

fn accept_page(job: &mut Job, page: FetchPage, requested_url: &str) {
    let Some(url) = normalise_url(&page.url).or_else(|| normalise_url(requested_url)) else {
        job.rejected_pages += 1;
        job.record(format!(
            "Rejected {requested_url}: fetch returned an invalid HTTP(S) URL"
        ));
        return;
    };
    let (aspect_ids, evidence) = assess(&job.question, &job.aspects, &page.title, &page.content);
    if aspect_ids.is_empty() {
        job.rejected_pages += 1;
        job.record(format!(
            "Read {} but did not count it: too short or insufficient question/aspect relevance",
            url
        ));
        return;
    }
    let links = outbound_links(&page.content, &job.question);
    let id = format!("S{}", job.sources.len() + 1);
    for aspect_id in &aspect_ids {
        if let Some(aspect) = job.aspects.iter_mut().find(|a| &a.id == aspect_id) {
            if !aspect.source_ids.contains(&id) {
                aspect.source_ids.push(id.clone());
            }
        }
    }
    let domain = domain_of(&url);
    job.record(format!(
        "Accepted {id} ({domain}): supports {}; {} evidence excerpts",
        aspect_ids.join(","),
        evidence.len()
    ));
    job.sources.push(Source {
        id,
        url,
        title: page.title,
        domain,
        published_date: page.published_date,
        retrieved_at: page.retrieved_at,
        method: page.method,
        cache_age_seconds: page.cache_age_seconds,
        content_chars: page.content.chars().count(),
        content_hash: content_fingerprint(&page.content),
        truncated: page.truncated,
        aspect_ids: aspect_ids.clone(),
        evidence,
    });
    for (link, label) in links {
        add_candidate(
            job,
            link,
            label,
            aspect_ids.clone(),
            format!("source: {requested_url}"),
        );
    }
}

fn add_candidate(
    job: &mut Job,
    raw_url: String,
    title: String,
    aspect_ids: Vec<String>,
    via: String,
) -> bool {
    let Some(url) = normalise_url(&raw_url) else {
        return false;
    };
    if job.seen_urls.contains(&url) || job.candidates.len() >= MAX_PENDING_CANDIDATES {
        return false;
    }
    let domain = domain_of(&url);
    if !job.options.include_domains.is_empty()
        && !job
            .options
            .include_domains
            .iter()
            .any(|allowed| domain_matches(&domain, allowed))
    {
        return false;
    }
    if job
        .options
        .exclude_domains
        .iter()
        .any(|blocked| domain_matches(&domain, blocked))
    {
        return false;
    }
    job.seen_urls.insert(url.clone());
    job.candidates.push(Candidate {
        url,
        title,
        aspect_ids,
        discovered_round: job.round,
        via,
    });
    true
}

fn domain_matches(domain: &str, configured: &str) -> bool {
    let configured = configured.trim().trim_start_matches("www.").to_lowercase();
    domain == configured || domain.ends_with(&format!(".{configured}"))
}

fn aspects_to_search(job: &Job) -> Vec<usize> {
    let mut indices: Vec<usize> = job
        .aspects
        .iter()
        .enumerate()
        .filter(|(_, a)| a.attempts < MAX_QUERY_ATTEMPTS)
        .map(|(index, _)| index)
        .collect();
    let core_needs_corroboration =
        job.core_evidence_groups().len() < 2.min(job.options.min_sources);
    indices.sort_by_key(|index| {
        let aspect = &job.aspects[*index];
        let core_priority = aspect.id == "core" && core_needs_corroboration;
        (
            aspect.source_ids.len(),
            if core_priority { 0 } else { 1 },
            aspect.attempts,
            *index,
        )
    });
    indices
}

fn take_candidates(job: &mut Job) -> Vec<Candidate> {
    let source_domains: std::collections::BTreeSet<String> = job
        .sources
        .iter()
        .map(|source| source.domain.clone())
        .collect();
    let remaining = job.options.max_sources.saturating_sub(job.sources.len());
    let take = remaining
        .min(job.options.fetches_per_round)
        .min(job.candidates.len());
    job.candidates.sort_by(|a, b| {
        let a_coverage = a
            .aspect_ids
            .iter()
            .filter_map(|id| job.aspects.iter().find(|aspect| &aspect.id == id))
            .map(|a| a.source_ids.len())
            .min()
            .unwrap_or(0);
        let b_coverage = b
            .aspect_ids
            .iter()
            .filter_map(|id| job.aspects.iter().find(|aspect| &aspect.id == id))
            .map(|a| a.source_ids.len())
            .min()
            .unwrap_or(0);
        let a_seen_domain = source_domains.contains(domain_of(&a.url).as_str());
        let b_seen_domain = source_domains.contains(domain_of(&b.url).as_str());
        a_coverage
            .cmp(&b_coverage)
            .then_with(|| a_seen_domain.cmp(&b_seen_domain))
            .then_with(|| a.discovered_round.cmp(&b.discovered_round))
            .then_with(|| a.url.cmp(&b.url))
    });
    job.candidates.drain(..take).collect()
}

fn deadline_exceeded(job: &Job) -> bool {
    job.elapsed() >= Duration::from_secs(job.options.max_minutes * 60)
}

fn terminal_for_budget(job: &mut Job) -> bool {
    let reason = if deadline_exceeded(job) {
        Some(format!(
            "Reached the {} minute wall-clock budget",
            job.options.max_minutes
        ))
    } else if job.round >= job.options.max_rounds {
        Some(format!(
            "Reached the {} round budget",
            job.options.max_rounds
        ))
    } else if job.searches >= job.options.max_searches && job.candidates.is_empty() {
        Some(format!(
            "Reached the {} search budget with no pending sources",
            job.options.max_searches
        ))
    } else if job.sources.len() >= job.options.max_sources {
        Some(format!(
            "Reached the {} source budget",
            job.options.max_sources
        ))
    } else {
        None
    };
    if let Some(reason) = reason {
        let reason = format!(
            "{reason}; coverage is not complete ({} sources, {} domains, {} uncovered aspects)",
            job.sources.len(),
            job.domains().len(),
            job.coverage().uncovered.len()
        );
        job.state = RunState::Partial;
        job.stop_reason = Some(reason.clone());
        job.record(reason);
        true
    } else {
        false
    }
}

fn partial(mut job: Job, reason: String) -> Job {
    job.state = RunState::Partial;
    job.stop_reason = Some(reason.clone());
    job.record(reason);
    job
}

fn cancelled(mut job: Job, reason: &str) -> Job {
    job.cancel_requested = true;
    job.state = RunState::Cancelled;
    job.stop_reason = Some(reason.to_string());
    job.record(reason);
    job
}

fn failed_after_errors(mut job: Job) -> Job {
    let last = job
        .queries
        .iter()
        .rev()
        .find_map(|q| q.error.clone())
        .or_else(|| job.failures.last().map(|f| f.error.clone()))
        .unwrap_or_else(|| "unknown backend error".into());
    job.state = if job.sources.is_empty() {
        RunState::Failed
    } else {
        RunState::Partial
    };
    job.stop_reason = Some(format!(
        "Stopped after {MAX_CONSECUTIVE_CALL_ERRORS} consecutive backend failures: {last}"
    ));
    job.record(job.stop_reason.clone().unwrap());
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{FetchOutcome, SearchResponse};
    use crate::model::ResearchOptions;
    use crate::plan::build_aspects;

    struct FakeGateway {
        searches: usize,
        queries: Vec<String>,
        fail_search: bool,
        fetch_error: bool,
        short_or_off_topic: bool,
        omit_aspects: bool,
        mirror_content: bool,
    }

    impl FakeGateway {
        fn new() -> Self {
            Self {
                searches: 0,
                queries: Vec::new(),
                fail_search: false,
                fetch_error: false,
                short_or_off_topic: false,
                omit_aspects: false,
                mirror_content: false,
            }
        }
    }

    impl ResearchGateway for FakeGateway {
        fn search(&mut self, query: &str, _: &ResearchOptions) -> Result<SearchResponse, String> {
            self.queries.push(query.to_string());
            self.searches += 1;
            if self.fail_search {
                return Err("scripted backend unavailable".into());
            }
            let number = self.searches;
            Ok(SearchResponse {
                hits: vec![SearchHit {
                    title: format!("Rust ownership source {number}"),
                    url: format!("https://domain{number}.example/rust-ownership"),
                }],
                engines: vec!["fake".into()],
                cached: false,
            })
        }

        fn fetch(
            &mut self,
            urls: &[String],
            _: &ResearchOptions,
        ) -> Result<Vec<FetchOutcome>, String> {
            if self.fetch_error {
                return Err("scripted fetch failed".into());
            }
            Ok(urls
                .iter()
                .map(|url| {
                    let content = if self.short_or_off_topic {
                        "A short off-topic snippet".to_string()
                    } else if self.omit_aspects {
                        "Rust ownership provides ownership and borrowing rules for memory safety. Rust ownership is explained here with practical Rust ownership details. ".repeat(20)
                    } else {
                        "Rust ownership is a memory safety mechanism. Pricing documentation includes a free option and examples with data. Security risks and limitations include unsafe code and supply-chain concerns. ".repeat(20)
                    };
                    let content = if self.mirror_content {
                        content
                    } else {
                        format!("{content}\nUnique parsed document at {url}.")
                    };
                    FetchOutcome {
                        requested_url: url.clone(),
                        result: Ok(FetchPage {
                            url: url.clone(),
                            title: if self.omit_aspects {
                                "Rust ownership core reference".into()
                            } else {
                                "Rust ownership: pricing and security".into()
                            },
                            content,
                            published_date: Some("2026-01-01".into()),
                            retrieved_at: Some("2026-02-01T00:00:00Z".into()),
                            method: Some("http".into()),
                            cache_age_seconds: Some(12),
                            truncated: false,
                        }),
                    }
                })
                .collect())
        }
    }

    fn job(max_rounds: usize) -> Job {
        let options = ResearchOptions {
            max_rounds,
            max_searches: max_rounds * 3,
            max_sources: 20,
            min_sources: 4,
            min_domains: 2,
            min_rounds: 3,
            max_minutes: 20,
            results_per_search: 5,
            queries_per_round: 2,
            fetches_per_round: 4,
            ..ResearchOptions::default()
        };
        let aspects = build_aspects("Rust ownership", &["pricing".into(), "security".into()]);
        Job::new(
            "r1".into(),
            "Rust ownership".into(),
            aspects,
            options,
            None,
            None,
        )
    }

    #[test]
    fn iterates_until_real_coverage_and_never_counts_snippets() {
        let mut gateway = FakeGateway::new();
        let finished = execute(job(8), &mut gateway, |_| false);
        assert_eq!(
            finished.state,
            RunState::Complete,
            "{:?}",
            finished.stop_reason
        );
        assert_eq!(finished.round, 3);
        assert_eq!(finished.sources.len(), 6);
        assert_eq!(finished.searches, 6);
        assert!(finished.coverage().met);
        assert_eq!(gateway.queries[0], "Rust ownership");
        assert_ne!(gateway.queries[0], gateway.queries[1]);
        assert!(finished.sources.iter().all(|s| !s.evidence.is_empty()));
        assert!(finished.evidence_json().get("candidates").is_none());
    }

    #[test]
    fn relevant_but_incomplete_aspects_end_partial_not_complete() {
        let mut gateway = FakeGateway::new();
        gateway.omit_aspects = true;
        let finished = execute(job(5), &mut gateway, |_| false);
        assert_eq!(finished.state, RunState::Partial);
        assert!(!finished.coverage().aspects_met);
        assert!(finished.sources.len() >= 4);
        assert!(finished.aspects.iter().any(|a| a.source_ids.is_empty()));
    }

    #[test]
    fn unavailable_pages_never_become_evidence_and_stall_out() {
        let mut gateway = FakeGateway::new();
        gateway.short_or_off_topic = true;
        let finished = execute(job(8), &mut gateway, |_| false);
        assert_eq!(finished.state, RunState::Partial);
        assert_eq!(finished.sources.len(), 0);
        assert!(finished.rejected_pages >= 1);
        assert!(finished
            .stop_reason
            .unwrap()
            .contains("no new relevant sources"));
    }

    #[test]
    fn fetch_failures_are_audited_and_do_not_fake_coverage() {
        let mut gateway = FakeGateway::new();
        gateway.fetch_error = true;
        let finished = execute(job(5), &mut gateway, |_| false);
        assert_eq!(finished.state, RunState::Partial);
        assert!(finished.sources.is_empty());
        assert!(!finished.failures.is_empty());
        assert!(finished
            .stop_reason
            .unwrap()
            .contains("no new relevant sources"));
    }

    #[test]
    fn repeated_transport_failure_without_sources_is_failed() {
        let mut gateway = FakeGateway::new();
        gateway.fail_search = true;
        let finished = execute(job(10), &mut gateway, |_| false);
        assert_eq!(finished.state, RunState::Failed);
        assert_eq!(finished.searches, 3);
        assert!(finished.stop_reason.unwrap().contains("backend failures"));
    }

    #[test]
    fn cancellation_is_terminal_and_does_not_keep_searching() {
        let mut gateway = FakeGateway::new();
        let finished = execute(job(10), &mut gateway, |job| !job.queries.is_empty());
        assert_eq!(finished.state, RunState::Cancelled);
        assert_eq!(finished.searches, 1);
        assert!(finished.cancel_requested);
    }

    #[test]
    fn mirrored_content_cannot_count_twice_for_core_cross_source_coverage() {
        let mut gateway = FakeGateway::new();
        gateway.mirror_content = true;
        let finished = execute(job(5), &mut gateway, |_| false);
        assert_eq!(finished.state, RunState::Partial);
        assert!(finished.sources.len() >= 4);
        assert!(finished.domains().len() >= 2);
        assert_eq!(finished.core_evidence_groups().len(), 1);
        assert!(!finished.coverage().met);
    }

    #[test]
    fn source_budget_cannot_be_overshot_by_a_batch() {
        let mut job = job(8);
        job.options.min_sources = 4;
        job.options.max_sources = 3;
        job.options.min_rounds = 2;
        job.options.fetches_per_round = 4;
        let mut gateway = FakeGateway::new();
        let finished = execute(job, &mut gateway, |_| false);
        assert!(finished.sources.len() <= 3);
        assert_eq!(
            finished.state,
            RunState::Partial,
            "source cap is below coverage target"
        );
    }

    #[test]
    fn wall_clock_deadline_is_enforced_at_operation_boundaries() {
        let mut job = job(8);
        job.started = std::time::Instant::now() - Duration::from_secs(21 * 60);
        let mut gateway = FakeGateway::new();
        let finished = execute(job, &mut gateway, |_| false);
        assert_eq!(finished.state, RunState::Partial);
        assert_eq!(finished.searches, 0);
        assert!(finished.stop_reason.unwrap().contains("wall-clock"));
    }
}
