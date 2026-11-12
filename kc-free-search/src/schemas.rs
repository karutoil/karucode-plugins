//! The tools free-search-mcp 0.17.0 advertises, vendored as a literal.
//!
//! # Why these are baked in rather than read from the backend
//!
//! A plugin must answer `initialize` within 10 s (karucode's handshake budget,
//! `PluginTimeouts::default`), and the harness cannot list tools it does not see
//! in that answer. Getting these from the backend would mean starting a Python
//! process - `uvx` resolving ninety packages on a fresh machine - inside that
//! budget, so a cold install would fail the plugin instead of the first search.
//!
//! Baking them in buys a second thing that matters as much: the tool schemas are
//! byte-identical on every start, so they sit in the cached part of the prompt
//! and a provider does not re-bill the whole context when the plugin restarts.
//!
//! The cost is drift. `main::schema_drift` compares these with what a live
//! backend actually advertises, and the `free-search` panel reports any
//! difference, so a backend upgrade that adds or changes a tool is visible
//! rather than silent.
//!
//! Regenerate after a backend upgrade:
//!
//! ```bash
//! python3 probe/probe_mcp.py --dump   # writes /tmp/fs_tools.json
//! python3 probe/gen_schemas.py        # rewrites this file
//! ```

use serde_json::{json, Value};

/// The backend version these schemas were taken from.
pub const UPSTREAM_VERSION: &str = "0.17.0";

/// `(name, inputSchema)` for every tool the backend registers.
///
/// The order here is the order the curated surface in `tools.rs` reasons about
/// (find a URL, then read it, then the specialist shapes) and the order this file
/// is regenerated in. It is **not** the order the host sees: the plugin SDK keeps
/// tools in a `BTreeMap`, so `tools/list` answers alphabetically. That is fine —
/// an agent selects a tool by reading descriptions, not by position — but do not
/// claim otherwise.
pub fn all() -> Vec<(&'static str, Value)> {
    vec![
        (
            "search",
            json!(
                {
                    "properties": {
                        "query": {
                            "type": "string"
                        },
                        "engines": {
                            "anyOf": [
                                {
                                    "items": {
                                        "type": "string"
                                    },
                                    "type": "array"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "max_results": {
                            "default": 10,
                            "type": "integer"
                        },
                        "use_cache": {
                            "default": true,
                            "type": "boolean"
                        },
                        "max_age_hours": {
                            "anyOf": [
                                {
                                    "type": "number"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "freshness": {
                            "anyOf": [
                                {
                                    "enum": [
                                        "day",
                                        "week",
                                        "month",
                                        "year"
                                    ],
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "include_domains": {
                            "anyOf": [
                                {
                                    "items": {
                                        "type": "string"
                                    },
                                    "type": "array"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "exclude_domains": {
                            "anyOf": [
                                {
                                    "items": {
                                        "type": "string"
                                    },
                                    "type": "array"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "category": {
                            "anyOf": [
                                {
                                    "enum": [
                                        "news",
                                        "pdf",
                                        "github",
                                        "paper",
                                        "forum",
                                        "blog",
                                        "image",
                                        "dataset",
                                        "finance",
                                        "software",
                                        "security",
                                        "reference",
                                        "weather",
                                        "docs",
                                        "gov",
                                        "stats",
                                        "calendar",
                                        "news.world",
                                        "paper.index",
                                        "paper.preprint",
                                        "paper.biomed",
                                        "paper.cs",
                                        "paper.math",
                                        "paper.openaccess",
                                        "paper.trial",
                                        "dataset.repository",
                                        "dataset.ml",
                                        "dataset.gov",
                                        "finance.filings",
                                        "finance.market",
                                        "finance.macro",
                                        "finance.fx",
                                        "finance.entity",
                                        "finance.crypto",
                                        "software.lifecycle",
                                        "software.github",
                                        "software.python",
                                        "software.node",
                                        "software.rust",
                                        "software.registry",
                                        "software.app",
                                        "security.cve",
                                        "security.package",
                                        "security.exploited",
                                        "reference.domain",
                                        "docs.web",
                                        "docs.rfc",
                                        "gov.us",
                                        "gov.uk",
                                        "stats.indicator",
                                        "calendar.holidays",
                                        "calendar.clock"
                                    ],
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "include_text": {
                            "anyOf": [
                                {
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "exclude_text": {
                            "anyOf": [
                                {
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "query"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "fetch",
            json!(
                {
                    "properties": {
                        "url": {
                            "type": "string"
                        },
                        "render": {
                            "default": "auto",
                            "enum": [
                                "auto",
                                "http",
                                "browser"
                            ],
                            "type": "string"
                        },
                        "force_refresh": {
                            "default": false,
                            "type": "boolean"
                        },
                        "max_age_hours": {
                            "anyOf": [
                                {
                                    "type": "number"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "inline": {
                            "default": false,
                            "type": "boolean"
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "url"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "research",
            json!(
                {
                    "properties": {
                        "question": {
                            "type": "string"
                        },
                        "depth": {
                            "default": 3,
                            "type": "integer"
                        },
                        "engines": {
                            "anyOf": [
                                {
                                    "items": {
                                        "type": "string"
                                    },
                                    "type": "array"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "fetch": {
                            "default": true,
                            "type": "boolean"
                        },
                        "use_cache": {
                            "default": true,
                            "type": "boolean"
                        },
                        "max_age_hours": {
                            "anyOf": [
                                {
                                    "type": "number"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "freshness": {
                            "anyOf": [
                                {
                                    "enum": [
                                        "day",
                                        "week",
                                        "month",
                                        "year"
                                    ],
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "include_domains": {
                            "anyOf": [
                                {
                                    "items": {
                                        "type": "string"
                                    },
                                    "type": "array"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "exclude_domains": {
                            "anyOf": [
                                {
                                    "items": {
                                        "type": "string"
                                    },
                                    "type": "array"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "category": {
                            "anyOf": [
                                {
                                    "enum": [
                                        "news",
                                        "pdf",
                                        "github",
                                        "paper",
                                        "forum",
                                        "blog",
                                        "image",
                                        "dataset",
                                        "finance",
                                        "software",
                                        "security",
                                        "reference",
                                        "weather",
                                        "docs",
                                        "gov",
                                        "stats",
                                        "calendar",
                                        "news.world",
                                        "paper.index",
                                        "paper.preprint",
                                        "paper.biomed",
                                        "paper.cs",
                                        "paper.math",
                                        "paper.openaccess",
                                        "paper.trial",
                                        "dataset.repository",
                                        "dataset.ml",
                                        "dataset.gov",
                                        "finance.filings",
                                        "finance.market",
                                        "finance.macro",
                                        "finance.fx",
                                        "finance.entity",
                                        "finance.crypto",
                                        "software.lifecycle",
                                        "software.github",
                                        "software.python",
                                        "software.node",
                                        "software.rust",
                                        "software.registry",
                                        "software.app",
                                        "security.cve",
                                        "security.package",
                                        "security.exploited",
                                        "reference.domain",
                                        "docs.web",
                                        "docs.rfc",
                                        "gov.us",
                                        "gov.uk",
                                        "stats.indicator",
                                        "calendar.holidays",
                                        "calendar.clock"
                                    ],
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "include_text": {
                            "anyOf": [
                                {
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "exclude_text": {
                            "anyOf": [
                                {
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "question"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "fetch_batch",
            json!(
                {
                    "properties": {
                        "urls": {
                            "items": {
                                "type": "string"
                            },
                            "type": "array"
                        },
                        "render": {
                            "default": "auto",
                            "enum": [
                                "auto",
                                "http",
                                "browser"
                            ],
                            "type": "string"
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "urls"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "read_doc",
            json!(
                {
                    "properties": {
                        "source": {
                            "type": "string"
                        },
                        "start": {
                            "default": 0,
                            "type": "integer"
                        },
                        "length": {
                            "anyOf": [
                                {
                                    "type": "integer"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "source"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "cache_search",
            json!(
                {
                    "properties": {
                        "query": {
                            "type": "string"
                        },
                        "limit": {
                            "default": 10,
                            "type": "integer"
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "query"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "engines",
            json!(
                {
                    "properties": {
                        "group": {
                            "anyOf": [
                                {
                                    "enum": [
                                        "web",
                                        "news",
                                        "paper",
                                        "github",
                                        "forum",
                                        "image",
                                        "dataset",
                                        "finance",
                                        "software",
                                        "security",
                                        "reference",
                                        "weather",
                                        "docs",
                                        "gov",
                                        "stats",
                                        "calendar"
                                    ],
                                    "type": "string"
                                },
                                {
                                    "enum": [
                                        "news",
                                        "pdf",
                                        "github",
                                        "paper",
                                        "forum",
                                        "blog",
                                        "image",
                                        "dataset",
                                        "finance",
                                        "software",
                                        "security",
                                        "reference",
                                        "weather",
                                        "docs",
                                        "gov",
                                        "stats",
                                        "calendar",
                                        "news.world",
                                        "paper.index",
                                        "paper.preprint",
                                        "paper.biomed",
                                        "paper.cs",
                                        "paper.math",
                                        "paper.openaccess",
                                        "paper.trial",
                                        "dataset.repository",
                                        "dataset.ml",
                                        "dataset.gov",
                                        "finance.filings",
                                        "finance.market",
                                        "finance.macro",
                                        "finance.fx",
                                        "finance.entity",
                                        "finance.crypto",
                                        "software.lifecycle",
                                        "software.github",
                                        "software.python",
                                        "software.node",
                                        "software.rust",
                                        "software.registry",
                                        "software.app",
                                        "security.cve",
                                        "security.package",
                                        "security.exploited",
                                        "reference.domain",
                                        "docs.web",
                                        "docs.rfc",
                                        "gov.us",
                                        "gov.uk",
                                        "stats.indicator",
                                        "calendar.holidays",
                                        "calendar.clock"
                                    ],
                                    "type": "string"
                                },
                                {
                                    "type": "null"
                                }
                            ],
                            "default": null
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "type": "object"
                }
            ),
        ),
        (
            "compare",
            json!(
                {
                    "properties": {
                        "question": {
                            "type": "string"
                        },
                        "urls": {
                            "items": {
                                "type": "string"
                            },
                            "type": "array"
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "question",
                        "urls"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "extract_structured",
            json!(
                {
                    "properties": {
                        "url": {
                            "type": "string"
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "url"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "paper_graph",
            json!(
                {
                    "properties": {
                        "paper": {
                            "type": "string"
                        },
                        "direction": {
                            "default": "both",
                            "enum": [
                                "both",
                                "references",
                                "citations"
                            ],
                            "type": "string"
                        },
                        "limit": {
                            "default": 10,
                            "type": "integer"
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "paper"
                    ],
                    "type": "object"
                }
            ),
        ),
        (
            "download",
            json!(
                {
                    "properties": {
                        "url": {
                            "type": "string"
                        },
                        "format": {
                            "default": "markdown",
                            "enum": [
                                "markdown",
                                "json"
                            ],
                            "type": "string"
                        }
                    },
                    "required": [
                        "url"
                    ],
                    "type": "object"
                }
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_is_a_valid_object_schema() {
        for (name, schema) in all() {
            assert_eq!(schema["type"], json!("object"), "{name}");
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn the_enums_that_matter_are_intact() {
        let map: std::collections::BTreeMap<_, _> = all().into_iter().collect();
        // `freshness` is the argument an agent needs most often and mistypes most
        // often; losing its enum would turn a schema error into a wasted engine call.
        let freshness = &map["search"]["properties"]["freshness"]["anyOf"][0]["enum"];
        assert_eq!(freshness[0], json!("day"));
        assert_eq!(freshness[3], json!("year"));
        // `category` routes to a source that indexes the thing directly.
        let cats = &map["search"]["properties"]["category"]["anyOf"][0]["enum"];
        assert!(
            cats.as_array().unwrap().len() > 40,
            "{} category values",
            cats.as_array().unwrap().len()
        );
        // Documented defaults a model relies on by omitting the argument.
        assert_eq!(map["search"]["properties"]["max_results"]["default"], json!(10));
        assert_eq!(map["research"]["properties"]["depth"]["default"], json!(3));
        assert_eq!(map["fetch"]["properties"]["render"]["default"], json!("auto"));
    }

    #[test]
    fn the_required_arguments_match_the_documented_surface() {
        let map: std::collections::BTreeMap<_, _> = all().into_iter().collect();
        let req = |name: &str| -> Vec<String> {
            map[name]["required"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        assert_eq!(req("search"), vec!["query"]);
        assert_eq!(req("fetch"), vec!["url"]);
        assert_eq!(req("compare"), vec!["question", "urls"]);
        assert_eq!(req("engines"), Vec::<String>::new());
    }
}
