//! Evidence-first Markdown/JSON reports with explicit pagination.

use crate::model::{Job, RunState};
use serde_json::{json, Value};

pub struct RenderedReport {
    pub content: String,
    pub meta: Value,
}

pub fn render(
    job: &Job,
    format: &str,
    offset: usize,
    max_bytes: usize,
) -> Result<RenderedReport, String> {
    if !["markdown", "json"].contains(&format) {
        return Err("format must be markdown or json".into());
    }
    if !(1_000..=20_000).contains(&max_bytes) {
        return Err("max_bytes must be between 1000 and 20000".into());
    }
    let full = if format == "json" {
        serde_json::to_string_pretty(&job.evidence_json()).map_err(|e| e.to_string())?
    } else {
        markdown(job)
    };
    if offset > full.len() {
        return Err(format!(
            "offset {offset} is past the report length ({} bytes)",
            full.len()
        ));
    }
    if !full.is_char_boundary(offset) {
        return Err("offset must be on a UTF-8 character boundary; use next_offset exactly".into());
    }
    let remaining = &full[offset..];
    let has_more = remaining.len() > max_bytes;
    let mut end = remaining.len().min(max_bytes);
    let mut suffix = String::new();
    if has_more && format == "markdown" {
        // Reserve room for the cursor note; max_bytes is a real output bound, not
        // merely the page-prose bound followed by a surprise extra paragraph.
        loop {
            let next_offset = offset + end;
            let next_suffix = format!(
                "\n\n[Report page ends here. Call report again with offset={next_offset}; no content is silently discarded.]"
            );
            let allowed = max_bytes.saturating_sub(next_suffix.len());
            let mut next_end = remaining.len().min(allowed);
            while next_end > 0 && !remaining.is_char_boundary(next_end) {
                next_end -= 1;
            }
            suffix = next_suffix;
            if next_end == end {
                break;
            }
            end = next_end;
        }
    } else {
        while end > 0 && !remaining.is_char_boundary(end) {
            end -= 1;
        }
    }
    let next_offset = offset + end;
    let content = format!("{}{}", &remaining[..end], suffix);
    Ok(RenderedReport {
        content,
        meta: json!({
            "id": job.id,
            "state": job.state,
            "terminal": job.state.terminal(),
            "format": format,
            "offset": offset,
            "next_offset": if has_more { Some(next_offset) } else { None },
            "has_more": has_more,
            "total_bytes": full.len(),
        }),
    })
}

fn markdown(job: &Job) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Deep research evidence report\n\n**Question:** {}\n\n",
        one_line(&job.question)
    ));
    out.push_str(&format!(
        "**Status:** {:?}  \n**Rounds:** {}  **Searches:** {}  **Sources read:** {}  **Distinct host domains:** {}  \n",
        job.state,
        job.round,
        job.searches,
        job.sources.len(),
        job.domains().len()
    ));
    if let Some(reason) = &job.stop_reason {
        out.push_str(&format!("**Stopping reason:** {}\n\n", one_line(reason)));
    }
    out.push_str(
        "> Evidence rule: quoted excerpts below come from fetched page text, never search snippets. \
         This is a source dossier for synthesis, not proof that a generated claim is true. Opening the \
         linked source is still necessary for a high-stakes or disputed claim. Relevance is lexical, and \
     host/content diversity does not establish factual or organizational independence.\n\n",
    );
    if job.state == RunState::Running || job.state == RunState::Queued {
        out.push_str("> **This run is still active.** Evidence below is a snapshot; unresolved aspects may still be being researched.\n\n");
    } else if job.state != RunState::Complete {
        out.push_str("> **Do not describe this run as complete.** Read the unresolved aspects and stopping reason before answering.\n\n");
    }

    out.push_str("## Coverage\n\n");
    out.push_str(
        "| Aspect | Query attempts | Read sources | State |\n| --- | ---: | ---: | --- |\n",
    );
    for aspect in &job.aspects {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            table_cell(&aspect.label),
            aspect.attempts,
            aspect.source_ids.len(),
            if aspect.source_ids.is_empty() {
                "**UNRESOLVED**"
            } else {
                "covered"
            }
        ));
    }
    let coverage = job.coverage();
    out.push_str(&format!(
        "\nCore cross-source coverage: {} distinct host/content groups ({}). Overall coverage predicate: {}.\n\n",
        job.core_evidence_groups().len(),
        if coverage.corroboration_met {
            "met"
        } else {
            "NOT met"
        },
        if coverage.met { "met" } else { "NOT met" }
    ));

    out.push_str("## Evidence by aspect\n\n");
    for aspect in &job.aspects {
        out.push_str(&format!(
            "### {} (`{}`)\n\n",
            one_line(&aspect.label),
            aspect.id
        ));
        if aspect.source_ids.is_empty() {
            out.push_str("**Unresolved:** no fetched source passed both the question and aspect relevance checks.\n\n");
            continue;
        }
        for source_id in &aspect.source_ids {
            let Some(source) = job.sources.iter().find(|s| &s.id == source_id) else {
                continue;
            };
            out.push_str(&format!(
                "**[{}] {}** — `{}`  \n",
                source.id,
                one_line(&source.title),
                source.domain
            ));
            if let Some(published) = &source.published_date {
                out.push_str(&format!(
                    "Published/date supplied by source: {}  \n",
                    one_line(published)
                ));
            }
            if let Some(retrieved) = &source.retrieved_at {
                out.push_str(&format!("Retrieved: {}  \n", one_line(retrieved)));
            }
            for evidence in source.evidence.iter().filter(|e| e.aspect_id == aspect.id) {
                out.push_str(&format!("> {}\n\n", evidence.excerpt));
            }
            out.push_str(&format!("Source: <{}>\n\n", source.url));
        }
    }

    out.push_str("## Source quality ledger\n\n");
    if job.sources.is_empty() {
        out.push_str("No sources passed the read-and-relevance gate.\n\n");
    }
    for source in &job.sources {
        out.push_str(&format!(
            "- **[{}] [{}]({})** — {} characters fetched; method {}; aspects: {}; content fingerprint `{}`; cache age {}{}\n",
            source.id,
            source.title.replace(['[', ']'], ""),
            source.url,
            source.content_chars,
            source.method.as_deref().unwrap_or("unknown"),
            source.aspect_ids.join(", "),
            source.content_hash,
            source.cache_age_seconds.map(|age| format!("{age}s")).unwrap_or_else(|| "not supplied/fresh".into()),
            if source.truncated {
                "; **fetch marked truncated**"
            } else {
                ""
            }
        ));
    }

    out.push_str("\n## Fetch failures and rejected material\n\n");
    if job.failures.is_empty() {
        out.push_str("No fetch_batch failures recorded.\n");
    } else {
        for failure in &job.failures {
            out.push_str(&format!(
                "- Round {} — <{}>: {}\n",
                failure.round,
                failure.url,
                one_line(&failure.error)
            ));
        }
    }
    out.push_str(&format!(
        "\n{} fetched pages were read but rejected as too short or insufficiently relevant. Use `free-search` fetch/read_doc for targeted manual follow-up where needed.\n",
        job.rejected_pages
    ));

    out.push_str("\n## Search audit\n\n");
    if job.queries.is_empty() {
        out.push_str("No search was performed.\n");
    }
    for query in &job.queries {
        out.push_str(&format!(
            "- Round {} / `{}`: `{}` — {} hits, {} new candidates{}",
            query.round,
            query.aspect_id,
            query.query.replace('`', "'"),
            query.hits,
            query.new_candidates,
            query
                .error
                .as_ref()
                .map(|e| format!("; ERROR: {}", one_line(e)))
                .unwrap_or_default()
        ));
        out.push('\n');
    }
    out.push_str("\nTreat any uncovered aspect or failed fetch as a limitation of the research, not as evidence that the answer is negative.\n");
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn table_cell(text: &str) -> String {
    one_line(text).replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Aspect, Evidence, ResearchOptions, Source};

    fn job() -> Job {
        let aspects = vec![Aspect {
            id: "core".into(),
            label: "Core".into(),
            seed_query: "x".into(),
            attempts: 1,
            source_ids: vec!["S1".into()],
        }];
        let mut job = Job::new(
            "r".into(),
            "x".into(),
            aspects,
            ResearchOptions::default(),
            None,
            None,
        );
        job.state = RunState::Complete;
        job.sources.push(Source {
            id: "S1".into(),
            url: "https://example.com/x".into(),
            title: "Read page".into(),
            domain: "example.com".into(),
            published_date: Some("2026-01-01".into()),
            retrieved_at: None,
            method: Some("http".into()),
            cache_age_seconds: Some(5),
            content_chars: 500,
            content_hash: "abc123".into(),
            truncated: false,
            aspect_ids: vec!["core".into()],
            evidence: vec![Evidence {
                aspect_id: "core".into(),
                excerpt: "Fetched prose, not a snippet.".into(),
            }],
        });
        job
    }

    #[test]
    fn json_excludes_unread_candidates_and_includes_provenance() {
        let text = serde_json::to_string(&job().evidence_json()).unwrap();
        assert!(text.contains("Read page"));
        assert!(text.contains("2026-01-01"));
        assert!(job().evidence_json().get("candidates").is_none());
        assert!(!text.contains("pending_candidates"));
    }

    #[test]
    fn markdown_is_an_evidence_dossier_not_a_success_boilerplate() {
        let text = markdown(&job());
        assert!(text.contains("Fetched prose, not a snippet."));
        assert!(text.contains("https://example.com/x"));
        assert!(text.contains("Coverage"));
    }

    #[test]
    fn pagination_respects_boundaries_and_reports_a_cursor() {
        let report = render(&job(), "markdown", 0, 1_000).unwrap();
        assert!(report.meta["has_more"] == true);
        let next = report.meta["next_offset"].as_u64().unwrap() as usize;
        assert!(next > 0 && next < 1_000);
        assert!(report.content.len() <= 1_000);
        let remainder = render(&job(), "markdown", next, 20_000).unwrap();
        assert!(!remainder.content.contains("Deep research evidence report"));
        assert_eq!(remainder.meta["offset"], next);
    }
}
