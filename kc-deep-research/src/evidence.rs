//! Text evidence, relevance, URLs and citation chasing.
//!
//! This is deliberately conservative: a fetched page must overlap the actual
//! question and the aspect's own concepts. Search ranking and snippets do not
//! participate in acceptance. Accepted excerpts are page text only.

use crate::model::{Aspect, Evidence};
use std::collections::BTreeSet;

const STOPWORDS: &[&str] = &[
    "about",
    "after",
    "against",
    "also",
    "and",
    "are",
    "because",
    "been",
    "before",
    "being",
    "between",
    "both",
    "but",
    "can",
    "could",
    "does",
    "each",
    "for",
    "from",
    "had",
    "has",
    "have",
    "how",
    "into",
    "its",
    "just",
    "more",
    "most",
    "not",
    "our",
    "out",
    "over",
    "should",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "under",
    "use",
    "using",
    "very",
    "was",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "why",
    "will",
    "with",
    "would",
    "you",
    "your",
    "research",
    "question",
    "answer",
    "direct",
    "current",
    "latest",
    "recent",
    "status",
    "development",
    "an",
    "as",
    "at",
    "be",
    "by",
    "do",
    "if",
    "in",
    "is",
    "it",
    "of",
    "on",
    "or",
    "so",
    "to",
    "up",
    "we",
    "us",
];

pub fn keywords(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 2)
        .filter(|word| !STOPWORDS.contains(word))
        .map(stem)
        .filter(|word| !word.is_empty())
        .collect()
}

fn stem(word: &str) -> String {
    if word.len() > 5 && word.ends_with("ies") {
        return format!("{}y", &word[..word.len() - 3]);
    }
    if word.len() > 4 && word.ends_with('s') && !word.ends_with("ss") {
        return word[..word.len() - 1].to_string();
    }
    word.to_string()
}

fn overlap(content: &BTreeSet<String>, wanted: &BTreeSet<String>) -> usize {
    wanted.iter().filter(|word| content.contains(*word)).count()
}

/// A stable-within-this-build duplicate marker, not a cryptographic or security
/// hash. Mirrors usually change navigation but repeat the same normalized prose.
pub fn content_fingerprint(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let normalized = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn core_relevant(question: &str, title: &str, text: &str) -> bool {
    let question_words = keywords(question);
    if question_words.is_empty() {
        return false;
    }
    let document = keywords(&format!("{title}\n{text}"));
    let hits = overlap(&document, &question_words);
    let needed = match question_words.len() {
        0 => 0,
        1 => 1,
        2..=4 => 2,
        5..=8 => 3,
        _ => 4,
    };
    hits >= needed
}

/// Return the aspects this page genuinely covers and one targeted excerpt for
/// each. The full fetched document is not retained, keeping job state bounded.
pub fn assess(
    question: &str,
    aspects: &[Aspect],
    title: &str,
    text: &str,
) -> (Vec<String>, Vec<Evidence>) {
    if text.chars().count() < 400 || !core_relevant(question, title, text) {
        return (Vec::new(), Vec::new());
    }
    let question_words = keywords(question);
    let document = keywords(&format!("{title}\n{text}"));
    let sentences = sentences(text);
    let mut ids = Vec::new();
    let mut evidence = Vec::new();
    for aspect in aspects {
        let mut aspect_words = keywords(&aspect.label);
        for word in &question_words {
            aspect_words.remove(word);
        }
        let aspect_hits = overlap(&document, &aspect_words);
        if !aspect_words.is_empty() && aspect_hits == 0 {
            continue;
        }
        let chosen = best_excerpt(&sentences, &question_words, &aspect_words)
            .or_else(|| sentences.first().cloned())
            .unwrap_or_else(|| clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), 420));
        if chosen.chars().count() < 30 {
            continue;
        }
        ids.push(aspect.id.clone());
        evidence.push(Evidence {
            aspect_id: aspect.id.clone(),
            excerpt: chosen,
        });
    }
    (ids, evidence)
}

fn sentences(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        current.push(ch);
        let sentence_end = matches!(ch, '.' | '!' | '?')
            && chars.peek().map(|c| c.is_whitespace()).unwrap_or(true);
        if ch == '\n' || sentence_end {
            push_sentence(&mut result, &current);
            current.clear();
        }
        if current.len() > 2_000 {
            push_sentence(&mut result, &current);
            current.clear();
        }
    }
    push_sentence(&mut result, &current);
    result
}

fn push_sentence(result: &mut Vec<String>, raw: &str) {
    let clean = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean = clean.trim_start_matches(['#', '>', '-', '*', ' ']).trim();
    if clean.chars().count() >= 35 && !clean.starts_with("![") {
        result.push(clip(clean, 420));
    }
}

fn best_excerpt(
    sentences: &[String],
    question: &BTreeSet<String>,
    aspect: &BTreeSet<String>,
) -> Option<String> {
    sentences
        .iter()
        .enumerate()
        .map(|(index, sentence)| {
            let words = keywords(sentence);
            let score = overlap(&words, question) * 2 + overlap(&words, aspect) * 3;
            (index, sentence, score)
        })
        .filter(|(_, _, score)| *score > 0)
        .max_by(|(ia, _, sa), (ib, _, sb)| sa.cmp(sb).then_with(|| ib.cmp(ia)))
        .map(|(_, sentence, _)| sentence.clone())
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    while out.len() > 1 && !out.is_char_boundary(out.len()) {
        out.pop();
    }
    out.push('…');
    out
}

pub fn normalise_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if !(raw.starts_with("https://") || raw.starts_with("http://")) {
        return None;
    }
    let without_fragment = raw.split('#').next().unwrap_or(raw);
    let (base, query) = without_fragment
        .split_once('?')
        .map(|(b, q)| (b, Some(q)))
        .unwrap_or((without_fragment, None));
    let mut url = base.trim_end_matches('/').to_string();
    if url.is_empty() || !url.contains("://") || domain_of(&url).is_empty() {
        return None;
    }
    if let Some(query) = query {
        let kept: Vec<_> = query
            .split('&')
            .filter(|part| {
                let key = part.split('=').next().unwrap_or_default().to_lowercase();
                !(key.starts_with("utm_")
                    || ["fbclid", "gclid", "mc_cid", "mc_eid"].contains(&key.as_str()))
            })
            .collect();
        if !kept.is_empty() {
            url.push('?');
            url.push_str(&kept.join("&"));
        }
    }
    Some(url)
}

pub fn domain_of(url: &str) -> String {
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .map(|h| format!("{h}]"))
            .unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default().to_string()
    };
    let host = host.to_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

/// Extract relevant Markdown outbound links for the next round. This is citation
/// chasing, not evidence: every link still goes through fetch and assess.
pub fn outbound_links(text: &str, question: &str) -> Vec<(String, String)> {
    let wanted = keywords(question);
    let mut result = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find("](") else { break };
        let label = &rest[..close];
        rest = &rest[close + 2..];
        let Some(end) = rest.find(')') else { break };
        let raw_url = &rest[..end];
        rest = &rest[end + 1..];
        let Some(url) = normalise_url(raw_url) else {
            continue;
        };
        let lower = url.to_lowercase();
        if [
            ".png", ".jpg", ".jpeg", ".gif", ".svg", ".css", ".js", ".zip",
        ]
        .iter()
        .any(|ending| lower.ends_with(ending))
        {
            continue;
        }
        let label_words = keywords(label);
        let relevant = overlap(&label_words, &wanted) > 0
            || [
                "documentation",
                "specification",
                "reference",
                "study",
                "evidence",
                "research",
            ]
            .iter()
            .any(|hint| label.to_lowercase().contains(hint));
        if relevant && !result.iter().any(|(existing, _)| existing == &url) {
            result.push((url, label.trim().chars().take(200).collect()));
        }
        if result.len() == 12 {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aspects() -> Vec<Aspect> {
        vec![
            Aspect {
                id: "core".into(),
                label: "Direct answer".into(),
                seed_query: "Rust ownership".into(),
                attempts: 0,
                source_ids: vec![],
            },
            Aspect {
                id: "limitations".into(),
                label: "Limitations and risks".into(),
                seed_query: "Rust ownership limitations".into(),
                attempts: 0,
                source_ids: vec![],
            },
        ]
    }

    #[test]
    fn snippets_not_needed_and_off_topic_pages_are_rejected() {
        let text = "Go garbage collectors are completely unrelated to this request. ".repeat(20);
        let (ids, evidence) = assess("Rust ownership", &aspects(), "Go garbage collection", &text);
        assert!(ids.is_empty());
        assert!(evidence.is_empty());
    }

    #[test]
    fn acceptance_requires_aspect_coverage_not_just_core_words() {
        let core_only =
            "Rust ownership ensures memory safety through ownership and borrowing rules. "
                .repeat(20);
        let (ids, _) = assess(
            "Rust ownership",
            &aspects(),
            "Rust ownership guide",
            &core_only,
        );
        assert_eq!(ids, vec!["core"]);
        let with_limits = format!("{core_only} Ownership limitations and risks include complex lifetime errors and learning curves.");
        let (ids, evidence) = assess(
            "Rust ownership",
            &aspects(),
            "Rust ownership guide",
            &with_limits,
        );
        assert!(ids.contains(&"limitations".into()));
        let excerpt = evidence
            .iter()
            .find(|e| e.aspect_id == "limitations")
            .unwrap();
        assert!(
            excerpt.excerpt.contains("limitations and risks"),
            "{}",
            excerpt.excerpt
        );
    }

    #[test]
    fn all_explicit_aspects_can_be_covered_by_one_relevant_page() {
        let names = [
            "pricing",
            "security",
            "performance",
            "documentation",
            "community",
        ];
        let mut aspects = vec![Aspect {
            id: "core".into(),
            label: "Direct answer".into(),
            seed_query: "Rust".into(),
            attempts: 0,
            source_ids: vec![],
        }];
        for (index, name) in names.into_iter().enumerate() {
            aspects.push(Aspect {
                id: format!("a{index}"),
                label: name.into(),
                seed_query: format!("Rust {name}"),
                attempts: 0,
                source_ids: vec![],
            });
        }
        let content =
            "Rust has pricing security performance documentation and community evidence. "
                .repeat(20);
        let (ids, evidence) = assess("Rust", &aspects, "Rust reference", &content);
        assert_eq!(ids.len(), 6);
        assert_eq!(evidence.len(), 6);
    }

    #[test]
    fn generic_words_do_not_make_arbitrary_long_text_relevant() {
        let text = "Completely arbitrary content without a topic. ".repeat(20);
        assert!(
            assess("What is it?", &aspects(), "Arbitrary", text.as_str())
                .0
                .is_empty()
        );
    }

    #[test]
    fn urls_dedupe_tracking_and_domains_are_stable() {
        assert_eq!(
            normalise_url("https://www.Example.com/path/?utm_source=x&a=1#part"),
            Some("https://www.Example.com/path?a=1".into())
        );
        assert_eq!(domain_of("https://www.example.com:443/a"), "example.com");
        assert!(normalise_url("file:///etc/passwd").is_none());
    }

    #[test]
    fn only_relevant_outbound_links_become_candidates() {
        let text = "[Rust ownership reference](https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html) and [weather](https://weather.example/)";
        let links = outbound_links(text, "Rust ownership");
        assert_eq!(links.len(), 1);
        assert!(links[0].0.contains("rust-lang"));
    }
}
