//! The narrow free-search-mcp surface used by the research engine.
//!
//! Only `search` and `fetch_batch` are needed. `structuredContent` avoids parsing
//! rendered Markdown, while the text fallback makes drift a clean job failure
//! rather than a panic. Other free-search tools remain available alongside this
//! plugin for the agent's own targeted follow-ups.

use crate::config::Settings;
use crate::mcp::{Backend, BackendConfig};
use crate::model::ResearchOptions;
use serde_json::{json, Map, Value};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub engines: Vec<String>,
    pub cached: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FetchPage {
    pub url: String,
    pub title: String,
    pub content: String,
    pub published_date: Option<String>,
    pub retrieved_at: Option<String>,
    pub method: Option<String>,
    pub cache_age_seconds: Option<u64>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub requested_url: String,
    pub result: Result<FetchPage, String>,
}

pub trait ResearchGateway {
    fn search(&mut self, query: &str, options: &ResearchOptions) -> Result<SearchResponse, String>;
    fn fetch(
        &mut self,
        urls: &[String],
        options: &ResearchOptions,
    ) -> Result<Vec<FetchOutcome>, String>;
}

pub struct McpGateway {
    backend: Backend,
}

impl McpGateway {
    pub fn new(settings: &Settings) -> Self {
        Self {
            backend: Backend::new(BackendConfig {
                command: settings.command.clone(),
                env: settings.backend_env(),
                start_timeout: Duration::from_secs(settings.start_timeout_secs),
                call_timeout: Duration::from_secs(settings.call_timeout_secs),
            }),
        }
    }
}

impl ResearchGateway for McpGateway {
    fn search(&mut self, query: &str, options: &ResearchOptions) -> Result<SearchResponse, String> {
        let mut arguments = Map::new();
        arguments.insert("query".into(), json!(query));
        arguments.insert("max_results".into(), json!(options.results_per_search));
        arguments.insert("format".into(), json!("json"));
        arguments.insert("use_cache".into(), json!(!options.force_refresh));
        if options.force_refresh {
            arguments.insert("max_age_hours".into(), json!(0));
        }
        if let Some(freshness) = &options.freshness {
            arguments.insert("freshness".into(), json!(freshness));
        }
        if let Some(category) = &options.category {
            arguments.insert("category".into(), json!(category));
        }
        for (key, values) in [
            ("engines", &options.engines),
            ("include_domains", &options.include_domains),
            ("exclude_domains", &options.exclude_domains),
        ] {
            if !values.is_empty() {
                arguments.insert(key.into(), json!(values));
            }
        }
        let result = self
            .backend
            .call_tool("search", Value::Object(arguments))
            .map_err(|e| e.to_string())?;
        check_error(&result)?;
        let payload = payload(&result)?;
        let hits = payload
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                "search returned no results array; the free-search-mcp schema may have changed"
                    .to_string()
            })?
            .iter()
            .filter_map(|hit| {
                let url = hit.get("url")?.as_str()?.to_string();
                Some(SearchHit {
                    title: hit
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(&url)
                        .to_string(),
                    url,
                })
            })
            .collect();
        Ok(SearchResponse {
            hits,
            engines: payload
                .get("engines")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            cached: payload
                .get("cached")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn fetch(
        &mut self,
        urls: &[String],
        options: &ResearchOptions,
    ) -> Result<Vec<FetchOutcome>, String> {
        if options.force_refresh {
            // Upstream fetch_batch has no force_refresh flag. Deliberately trade
            // batching for freshness by using its single-URL fetch API.
            let mut outcomes = Vec::new();
            for url in urls {
                let result = self
                    .backend
                    .call_tool(
                        "fetch",
                        json!({"url": url, "format": "json", "force_refresh": true, "max_age_hours": 0}),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|result| {
                        check_error(&result)?;
                        payload(&result)
                    });
                outcomes.push(match result {
                    Ok(page) => page_outcome(&page, url),
                    Err(error) => FetchOutcome {
                        requested_url: url.clone(),
                        result: Err(error),
                    },
                });
            }
            return Ok(outcomes);
        }
        let result = self
            .backend
            .call_tool(
                "fetch_batch",
                json!({"urls": urls, "format": "json", "render": "auto"}),
            )
            .map_err(|e| e.to_string())?;
        check_error(&result)?;
        let payload = payload(&result)?;
        let pages = if let Some(array) = payload.as_array() {
            array
        } else {
            payload
                .get("result")
                .and_then(Value::as_array)
                .ok_or_else(|| "fetch_batch returned no result array; the free-search-mcp schema may have changed".to_string())?
        };
        Ok(pages
            .iter()
            .enumerate()
            .map(|(index, page)| {
                page_outcome(
                    page,
                    urls.get(index).map(String::as_str).unwrap_or_default(),
                )
            })
            .collect())
    }
}

fn page_outcome(page: &Value, fallback_url: &str) -> FetchOutcome {
    let requested_url = page
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or(fallback_url)
        .to_string();
    if let Some(error) = page.get("error").and_then(Value::as_str) {
        return FetchOutcome {
            requested_url,
            result: Err(error.to_string()),
        };
    }
    let content = page
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if content.trim().is_empty() {
        return FetchOutcome {
            requested_url,
            result: Err(
                "the URL returned no readable text (it may be an image or blocked page)".into(),
            ),
        };
    }
    FetchOutcome {
        requested_url: requested_url.clone(),
        result: Ok(FetchPage {
            url: page
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or(&requested_url)
                .to_string(),
            title: page
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
                .unwrap_or(&requested_url)
                .to_string(),
            content: content.to_string(),
            published_date: string_field(page, "published_date"),
            retrieved_at: string_field(page, "retrieved_at"),
            method: string_field(page, "method"),
            cache_age_seconds: page.get("cache_age_seconds").and_then(Value::as_u64),
            truncated: page
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn check_error(result: &Value) -> Result<(), String> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(if text.is_empty() {
            "free-search-mcp reported an unspecified error".into()
        } else {
            text
        });
    }
    Ok(())
}

fn payload(result: &Value) -> Result<Value, String> {
    if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
        return Ok(structured.clone());
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let json_text = text
        .split("\n\nHandles you can fetch again")
        .next()
        .unwrap_or(&text)
        .trim();
    serde_json::from_str(json_text)
        .map_err(|e| format!("free-search-mcp returned no usable structuredContent ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_reads_the_real_structured_shape() {
        let raw = json!({"structuredContent": {
            "query":"x", "cached":true, "engines":["one"],
            "results":[{"title":"A","url":"https://a.example","snippet":"not evidence"}]
        }, "content":[]});
        let value = payload(&raw).unwrap();
        assert_eq!(value["results"][0]["url"], "https://a.example");
        assert_eq!(value["cached"], true);
    }

    #[test]
    fn fetch_batch_unwraps_fastmcp_result_and_preserves_dates() {
        let raw = json!({"structuredContent":{"result":[{
            "url":"https://a.example", "title":"A", "content":"body",
            "published_date":"2025-01-01", "method":"http", "truncated":false
        }]}});
        let pages = payload(&raw).unwrap();
        assert_eq!(pages["result"][0]["published_date"], "2025-01-01");
    }

    #[test]
    fn json_text_fallback_does_not_include_rendered_handles() {
        let raw = json!({"content":[{"type":"text","text":"{\"results\":[]}\n\nHandles you can fetch again with `fetch`:\n- x: <cache://x>"}]});
        assert_eq!(payload(&raw).unwrap()["results"], json!([]));
    }

    #[test]
    fn backend_tool_errors_are_not_parsed_as_empty_success() {
        let error = check_error(
            &json!({"isError":true,"content":[{"type":"text","text":"query must not be empty"}]}),
        );
        assert_eq!(error.unwrap_err(), "query must not be empty");
    }
}
