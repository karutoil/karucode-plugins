//! The agent-facing surface: what the model sees and how a result is rendered.
//!
//! # Why the upstream descriptions are not reused verbatim
//!
//! free-search-mcp's own docstrings are excellent and enormous: `search` alone is
//! 7.4 kB. That is the right size for a server whose client is a chat app with a
//! human reading the tool list. In an agentic loop it is wrong twice over: the
//! text is paid for on every request in the tool schema, and it teaches a
//! *conversational* reading ("Best for / Not recommended for") when what the loop
//! needs is a decision procedure — which tool, in which order, and what counts as
//! finished.
//!
//! So each description below is rewritten to about a fifth of the length, states
//! the one mistake that matters for that tool, and the originals are kept where
//! they still fit (the full schema, every enum and default, comes from the
//! backend unchanged — see `schema_for`).
//!
//! # Why the rules live here and not only in the prompt
//!
//! `before_prompt` (see `main.rs`) states the discipline once. This module is what
//! makes it *checkable*: the tool that finds URLs says in its own description that
//! a snippet is not a fact, and the tool that reads a page says in its own
//! description that it is the step which turns a lead into a fact. A prompt is a
//! reminder; a description is context that cannot be forgotten mid-run.

use serde_json::{json, Value};

/// What a tool needs from the harness, and why.
///
/// Every plugin tool is exposed as `external` whatever it declares, so this is
/// documentation for the operator (and it is what `settings.read_only_tools`
/// keys off), not a security control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Reaches the network but changes nothing the user owns.
    ReadOnly,
    /// Writes a file the user can find afterwards.
    Mutating,
}

/// One curated tool: the name the backend uses, and what the agent is told.
pub struct ToolSpec {
    pub name: &'static str,
    pub class: Class,
    pub description: &'static str,
}

/// The rules appended to every tool description, so no single one can be read
/// without the discipline it participates in.
pub const DISCIPLINE: &str = "\n\nRule: `search` and `research` find URLs; a snippet is an engine's \
summary and may be years old. Read any date, version, price, rule or number from \
the page itself before you state it.";

/// The curated surface, in the order the model should reach for it.
///
/// Ordering is deliberate: finding a URL, then reading it, then the shapes that
/// only some questions need. `download` is last because it is the only tool with
/// a side effect outside the conversation.
pub const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "search",
        class: Class::ReadOnly,
        description: "Find URLs. Runs a keyless multi-engine web search and returns a ranked, \
deduplicated list of links with snippets. Use it when you do not know the URL yet, or when \
the answer is probably newer than your training data. Add `freshness` for anything \
time-sensitive, `include_domains` to stay on sources you trust, and `category` to reach a \
source that indexes a kind of thing directly (a package registry, a CVE, a filing, a \
forecast) instead of a general index. Output is Markdown with a header naming the engines \
that answered, how many results could be dated, and how old the cache entry is.",
    },
    ToolSpec {
        name: "fetch",
        class: Class::ReadOnly,
        description: "Read one URL. Returns reader-mode Markdown for a page, parsed text for a \
PDF/DOCX/XLSX/PPTX/EPUB, or a description (type, size, dimensions, sha256) for an image or \
binary. This is the step that turns a search lead into a fact: everything you state as \
current - a version, a deadline, an amount, an API signature - should come from here, not \
from a snippet. `render=\"browser\"` renders JavaScript-heavy pages; `force_refresh=true` \
bypasses the cache. A cached copy states its age, so check that before relying on it.",
    },
    ToolSpec {
        name: "research",
        class: Class::ReadOnly,
        description: "Search and read in one call. Runs the search, fetches the top `depth` \
results, and returns a Markdown brief with the sources above their full text. Prefer this \
over `search` followed by several `fetch` calls when you want to understand a topic rather \
than locate one page: it is one round trip instead of many, and the text arrives together. \
The cost is context - a depth of 3 is roughly 3-5k tokens - so lower `depth` for a narrow \
question and skip the `fetch` step for a broad one.",
    },
    ToolSpec {
        name: "fetch_batch",
        class: Class::ReadOnly,
        description: "Read several known URLs concurrently (max 20 per call). Per-URL failures \
are reported inline and do not fail the call, so one dead link never costs you the other \
nineteen. Use it instead of a sequence of `fetch` calls whenever you already have the list.",
    },
    ToolSpec {
        name: "read_doc",
        class: Class::ReadOnly,
        description: "Read a document with pagination: PDF, DOCX, XLSX, PPTX, EPUB, CSV, code, \
zip/tar, HTML or plain text. Use it when `fetch` returned a truncated document, when you \
need a specific range of a long PDF, or when a file is too large to read in one call. \
`start` and `length` walk it by character offset.",
    },
    ToolSpec {
        name: "cache_search",
        class: Class::ReadOnly,
        description: "Full-text search over pages this machine already fetched, using a local \
FTS5 index. Use it to re-check something you read earlier in the session without paying for \
the network again, and to find a passage you remember but cannot place. It cannot find \
anything that was never fetched.",
    },
    ToolSpec {
        name: "engines",
        class: Class::ReadOnly,
        description: "List the available search sources, grouped by what they index. Call it \
when you need to know whether a specific source exists (a registry, a court filing system, a \
weather service) before you route a search with `category` or name engines explicitly. With \
no arguments it prints the whole tree, which is long; pass `group` for one branch.",
    },
    ToolSpec {
        name: "compare",
        class: Class::ReadOnly,
        description: "Fetch 2-5 URLs and return per-URL excerpts answered against one question. \
Use it to compare documentation versions, competing claims, or several vendors' terms \
side-by-side in one call instead of reading each page into context separately.",
    },
    ToolSpec {
        name: "extract_structured",
        class: Class::ReadOnly,
        description: "Pull metadata fields from a page: JSON-LD, OpenGraph, Twitter cards and \
microdata. Use it for machine-readable facts a page publishes deliberately - a product price \
and currency, an article's publication date, an event's venue - where reading prose would be \
guesswork. It returns metadata only; use `fetch` for the text. Prose fields are clipped, and \
`trimmed` says when.",
    },
    ToolSpec {
        name: "paper_graph",
        class: Class::ReadOnly,
        description: "Walk one paper's citation graph: its references, the works citing it ranked \
by influence, and Crossref retraction or correction notices. Use it for a literature question \
(`direction=\"references\"` or `\"citations\"`) or to check whether a paper still stands before \
citing it. Takes a DOI, an OpenAlex ID, an exact title or an arXiv reference \
(`arXiv:1706.03762`, `1706.03762`, a `10.48550/arXiv.` DOI).",
    },
    ToolSpec {
        name: "download",
        class: Class::Mutating,
        description: "Save a file from a URL into the backend's downloads directory (files \
auto-delete after 24h) and return its path. Use it only when the user asked for a file on \
disk; to read a document, `fetch` or `read_doc` is better and leaves nothing behind. Clear \
the path with the user before relying on it: it is outside the project directory.",
    },
];

/// Look up a curated spec by backend tool name.
pub fn spec_for(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|t| t.name == name)
}

/// The tools an operator may safely allowlist as read-only.
///
/// This is the source of truth for `plugin.toml`'s `settings.read_only_tools`:
/// declaring `read_only` on a tool does not skip the approval prompt (the host
/// exposes every plugin tool as `external`), but listing it here and in the
/// entry does. Anything with a side effect outside the conversation is excluded
/// by construction, so adding a tool to `TOOLS` cannot silently widen the set of
/// calls that skip the prompt.
pub fn read_only_names() -> Vec<&'static str> {
    TOOLS
        .iter()
        .filter(|t| t.class == Class::ReadOnly)
        .map(|t| t.name)
        .collect()
}

/// Total bytes of the curated descriptions the agent actually receives.
///
/// Reported in the panel beside the upstream total: rewriting this surface is
/// only worth defending if the saving is visible.
pub fn curated_description_bytes() -> usize {
    TOOLS.iter().map(|t| description_for(t).len()).sum()
}

/// The description handed to the host: the curated text plus the shared rule.
pub fn description_for(spec: &ToolSpec) -> String {
    format!("{}{DISCIPLINE}", spec.description)
}

/// Make a backend schema safe and honest for a karucode tool call.
///
/// The backend's `inputSchema` is a full JSON Schema. karucode passes `parameters`
/// through to the model unchanged, so two things are worth tightening without
/// changing any enum or default:
///
/// * the Pydantic-generated `title` on the object and on every property is noise
///   in a tool schema — it duplicates the key name and costs tokens;
/// * `format` is optional on every tool with a documented default, and a model
///   that omits it means "markdown", so the default is stated where it will be
///   read.
///
/// Everything else (every `enum`, every `anyOf`, `max_results`, `depth`) is the
/// backend's, verbatim: this plugin must never be the reason a backend feature is
/// unreachable from karucode.
pub fn schema_for(mut schema: Value) -> Value {
    strip_titles(&mut schema);
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        if let Some(format) = props.get_mut("format") {
            if format.get("default").is_none() {
                format["default"] = json!("markdown");
            }
            if format.get("description").is_none() {
                format["description"] =
                    json!("`markdown` (default) is compact and token-cheap; `json` returns \
structured data when you need to parse a field.");
            }
        }
    }
    schema
}

/// Drop Pydantic's `title` keys, keeping the structural schema intact.
fn strip_titles(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("title");
            for (_, child) in map.iter_mut() {
                strip_titles(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_titles(item);
            }
        }
        _ => {}
    }
}

/// One MCP content block, already classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Text(String),
    /// An image the result carries as base64. Plugin results cannot forward
    /// pixels to the model (karucode's plugin path decodes with an empty image
    /// list), so this is reported as a description with the reason.
    Image { mime: String, bytes: usize },
    /// A `resource_link` block: a handle the client could read back.
    ResourceLink { uri: String, name: String },
    /// A content type this plugin does not know how to render.
    Other(String),
}

/// Classify one content block.
pub fn block_of(value: &Value) -> Block {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "text" => Block::Text(
            value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "image" => Block::Image {
            mime: value
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string(),
            // base64 inflates by 4/3; the decoded size is what the model cares about.
            bytes: value
                .get("data")
                .and_then(Value::as_str)
                .map(|d| d.len() * 3 / 4)
                .unwrap_or(0),
        },
        "resource_link" => Block::ResourceLink {
            uri: value
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        other => Block::Other(other.to_string()),
    }
}

/// How a rendered result ends, and therefore whether the model should trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The backend reported `isError`.
    ToolFailed,
}

/// Render an MCP `tools/call` result for the model.
///
/// The upstream server is careful to put the useful text in the first `text`
/// block and its `structuredContent` beside it. karucode renders a plugin result
/// as text, so:
///
/// * text blocks are concatenated, in order, exactly as the backend wrote them;
/// * a `resource_link` becomes one line naming the handle, because it is a real
///   thing the user may want (`cache://page/<url>`) and dropping it would lose
///   information the backend took care to provide;
/// * an `image` becomes an explicit note. Silently dropping it would leave the
///   model believing it saw the picture, and pretending to forward it is worse.
pub fn render(result: &Value, max_bytes: usize) -> (Outcome, String) {
    let blocks = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let outcome = if result.get("isError").and_then(Value::as_bool) == Some(true) {
        Outcome::ToolFailed
    } else {
        Outcome::Ok
    };

    let mut text = String::new();
    let mut links: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    for value in &blocks {
        match block_of(value) {
            Block::Text(t) => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&t);
            }
            Block::ResourceLink { uri, name } => {
                let label = if name.is_empty() { uri.clone() } else { name };
                links.push(format!("- {label}: <{uri}>"));
            }
            Block::Image { mime, bytes } => images.push(format!(
                "an image ({mime}, about {} kB) that this plugin cannot forward to you: \
                 karucode's plugin results carry text only. Use `fetch` without `inline` to \
                 get its type, dimensions and sha256 instead.",
                bytes / 1024
            )),
            Block::Other(kind) => {
                links.push(format!("- unsupported content block `{kind}`"));
            }
        }
    }

    // `structuredContent` is the same answer in machine form. It is only worth
    // mentioning when there was no text block at all (a backend change), because
    // otherwise the text already says everything and the JSON would double the
    // token cost for no gain.
    if text.trim().is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            text = serde_json::to_string_pretty(structured).unwrap_or_default();
        }
    }
    if text.trim().is_empty() && links.is_empty() && images.is_empty() {
        text = "(the backend returned an empty result)".to_string();
    }

    let mut out = text;
    for note in images {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&note);
    }
    if !links.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("Handles you can fetch again with `fetch`:\n{}", links.join("\n")));
    }

    let (out, cut) = truncate(&out, max_bytes);
    (outcome, if cut { format!("{out}\n\n{TRUNCATION_HINT}") } else { out })
}

/// Said instead of the harness's generic note: it names what to do about it.
pub const TRUNCATION_HINT: &str = "[This result was truncated by the free-search \
plugin before karucode's own limit. Nothing was lost that a narrower call cannot \
recover: `fetch` one URL instead of a batch, lower `depth` for `research`, or ask \
for `format=\"json\"` and read one field.]";

/// Cut `text` to `max_bytes` on a character boundary.
pub fn truncate(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_curated_tool_is_one_the_backend_advertises() {
        // The 11 tools free-search-mcp 0.17.0 registers. If upstream adds one, this
        // test is the reminder to write a description for it rather than let an
        // uncurated tool appear with its 2 kB docstring.
        let upstream = [
            "search",
            "fetch",
            "fetch_batch",
            "read_doc",
            "research",
            "paper_graph",
            "cache_search",
            "engines",
            "compare",
            "extract_structured",
            "download",
        ];
        assert_eq!(TOOLS.len(), upstream.len());
        for name in upstream {
            assert!(spec_for(name).is_some(), "no curated spec for `{name}`");
        }
    }

    #[test]
    fn curated_descriptions_are_substantially_shorter_than_upstreams() {
        // The point of rewriting them: `search` is 7.4 kB upstream.
        for spec in TOOLS {
            let text = description_for(spec);
            assert!(
                text.len() < 1_500,
                "`{}` is {} bytes; keep it under 1500",
                spec.name,
                text.len()
            );
            assert!(text.len() > 120, "`{}` says too little", spec.name);
        }
    }

    #[test]
    fn every_description_carries_the_shared_rule() {
        // A tool must not be readable in isolation from the discipline.
        for spec in TOOLS {
            assert!(
                description_for(spec).contains("a snippet is an engine's summary"),
                "`{}` lacks the discipline rule",
                spec.name
            );
        }
    }

    #[test]
    fn only_download_is_mutating_and_the_allowlist_excludes_it() {
        assert_eq!(spec_for("download").unwrap().class, Class::Mutating);
        let ro = read_only_names();
        assert!(ro.contains(&"search"));
        assert!(ro.contains(&"fetch"));
        assert!(!ro.contains(&"download"));
        assert_eq!(ro.len(), TOOLS.len() - 1);
    }

    #[test]
    fn schemas_keep_every_enum_and_default_and_lose_only_titles() {
        let backend = json!({
            "type": "object",
            "title": "searchArguments",
            "properties": {
                "query": {"type": "string", "title": "Query"},
                "freshness": {
                    "anyOf": [
                        {"type": "string", "enum": ["day", "week", "month", "year"], "title": "F"},
                        {"type": "null"}
                    ],
                    "default": null,
                    "title": "Freshness"
                },
                "max_results": {"type": "integer", "default": 10, "title": "Max Results"},
                "format": {"type": "string", "enum": ["markdown", "json"], "default": "markdown", "title": "Format"}
            },
            "required": ["query"]
        });
        let out = schema_for(backend);
        assert!(out.get("title").is_none());
        let props = out["properties"].as_object().unwrap();
        assert!(props["query"].get("title").is_none());
        // Structural content survives untouched.
        assert_eq!(props["freshness"]["anyOf"][0]["enum"][0], "day");
        assert_eq!(props["max_results"]["default"], 10);
        assert_eq!(out["required"][0], "query");
        // ...and the format default is now documented where it is read.
        assert!(props["format"]["description"].as_str().unwrap().contains("markdown"));
    }

    #[test]
    fn text_blocks_concatenate_in_order() {
        let result = json!({"content": [
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]});
        let (outcome, text) = render(&result, 10_000);
        assert_eq!(outcome, Outcome::Ok);
        assert_eq!(text, "first\n\nsecond");
    }

    #[test]
    fn an_iserror_result_is_reported_as_a_failure_with_its_message() {
        let result = json!({
            "content": [{"type": "text", "text": "Error executing tool search: query must not be empty"}],
            "isError": true
        });
        let (outcome, text) = render(&result, 10_000);
        assert_eq!(outcome, Outcome::ToolFailed);
        assert!(text.contains("query must not be empty"));
    }

    #[test]
    fn a_resource_link_is_kept_as_a_handle() {
        // Dropping it would lose something the backend took care to send:
        // `cache://page/<url>` is how the user re-reads a cached page.
        let result = json!({"content": [
            {"type": "text", "text": "results"},
            {"type": "resource_link", "uri": "cache://search/abc123", "name": "cached search"}
        ]});
        let (_, text) = render(&result, 10_000);
        assert!(text.starts_with("results"), "{text}");
        assert!(text.contains("cache://search/abc123"), "{text}");
        assert!(text.contains("cached search"), "{text}");
    }

    #[test]
    fn an_image_is_explained_rather_than_silently_dropped() {
        // `fetch(inline=true)` returns a real image block. The plugin cannot pass
        // pixels on, and pretending otherwise would make the model describe a
        // picture it never saw.
        let result = json!({"content": [
            {"type": "image", "mimeType": "image/png", "data": "a".repeat(4000)}
        ]});
        let (outcome, text) = render(&result, 10_000);
        assert_eq!(outcome, Outcome::Ok);
        assert!(text.contains("image/png"), "{text}");
        assert!(text.contains("cannot forward"), "{text}");
        assert!(text.contains("without `inline`"), "the remedy must be named: {text}");
    }

    #[test]
    fn structured_content_is_used_only_when_there_is_no_text() {
        // When a text block exists it already says everything; repeating the same
        // answer as JSON would double the token cost.
        let with_text = json!({
            "content": [{"type": "text", "text": "markdown answer"}],
            "structuredContent": {"results": []}
        });
        assert_eq!(render(&with_text, 10_000).1, "markdown answer");

        let only_structured = json!({
            "content": [],
            "structuredContent": {"results": [{"title": "t"}]}
        });
        let text = render(&only_structured, 10_000).1;
        assert!(text.contains("\"title\": \"t\""), "{text}");
    }

    #[test]
    fn an_empty_result_says_so_instead_of_returning_nothing() {
        let (_, text) = render(&json!({"content": []}), 10_000);
        assert_eq!(text, "(the backend returned an empty result)");
    }

    #[test]
    fn truncation_keeps_utf8_intact_and_names_the_remedy() {
        let long = "é".repeat(50);
        let (out, cut) = truncate(&long, 51);
        assert!(cut);
        assert!(out.len() <= 51);
        // 51 bytes is not a char boundary for 2-byte chars; it must not split one.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());

        let result = json!({"content": [{"type": "text", "text": "x".repeat(500)}]});
        let (_, text) = render(&result, 100);
        assert!(text.contains(TRUNCATION_HINT));
        assert!(text.contains("lower `depth`"));
    }

    #[test]
    fn a_result_under_the_budget_is_untouched() {
        let result = json!({"content": [{"type": "text", "text": "short"}]});
        assert_eq!(render(&result, 10_000).1, "short");
    }
}
