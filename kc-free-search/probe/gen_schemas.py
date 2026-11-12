#!/usr/bin/env python3
"""Regenerate src/schemas.rs from a live backend.

The plugin bakes the backend's tool schemas in rather than fetching them during
`initialize` (see the header of src/schemas.rs for why). That means a backend
upgrade can drift, so regenerating is a deliberate two-step:

    python3 probe/probe_mcp.py --dump    # writes /tmp/fs_tools.json
    python3 probe/gen_schemas.py         # rewrites src/schemas.rs

Only the STDIN/STDOUT half of this file lives here; the interesting work is
`verify_against` in src/main.rs, which compares the live backend with what is
baked in and reports the difference in the plugin's inspector panel.
"""
import json
import pathlib
import sys

# The order the agent should reach for the tools in, not the backend's own order.
ORDER = [
    "search", "fetch", "research", "fetch_batch", "read_doc", "cache_search",
    "engines", "compare", "extract_structured", "paper_graph", "download",
]

HEADER = '''//! The tools free-search-mcp {version} advertises, vendored as a literal.
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
//! The cost is drift. `main::backend_report` compares these with what a live
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

use serde_json::{{json, Value}};

/// The backend version these schemas were taken from.
pub const UPSTREAM_VERSION: &str = "{version}";

/// `(name, inputSchema)` for every tool the backend registers, in the order the
/// agent should reach for them.
pub fn all() -> Vec<(&'static str, Value)> {{
    vec![
'''

FOOTER = '''    ]
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
'''


def strip_titles(value):
    """Drop Pydantic's `title` keys, matching what `tools::schema_for` exposes.

    They duplicate the property name and are paid for on every request; the
    structural schema (enums, defaults, required) is what the model reads.
    """
    if isinstance(value, dict):
        value.pop("title", None)
        for child in value.values():
            strip_titles(child)
    elif isinstance(value, list):
        for child in value:
            strip_titles(child)
    return value


def render_entries(tools: dict) -> list[str]:
    out = []
    for name in ORDER:
        schema = strip_titles(tools[name]["inputSchema"])
        blob = json.dumps(schema, indent=4, ensure_ascii=False)
        # Sit the literal inside json!( ... ) at a readable depth.
        body = "\n".join("                " + line for line in blob.splitlines())
        out.append("        (\n")
        out.append(f"            {json.dumps(name)},\n")
        out.append("            json!(\n")
        out.append(body + "\n")
        out.append("            ),\n")
        out.append("        ),\n")
    return out


def main() -> int:
    src = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/fs_tools.json")
    if not src.exists():
        print(f"{src} is missing; run probe/probe_mcp.py --dump first", file=sys.stderr)
        return 1
    tools = {t["name"]: t for t in json.loads(src.read_text())}
    missing = [n for n in ORDER if n not in tools]
    if missing:
        print(f"backend no longer advertises: {', '.join(missing)}", file=sys.stderr)
        return 1
    extra = sorted(set(tools) - set(ORDER))
    if extra:
        print(
            f"note: backend advertises uncurated tools ({', '.join(extra)}); "
            "add a description in src/tools.rs before adding them here",
            file=sys.stderr,
        )

    version = "0.17.0"
    out = [HEADER.format(version=version)]
    out.extend(render_entries(tools))
    out.append(FOOTER)

    dest = pathlib.Path(__file__).resolve().parent.parent / "src" / "schemas.rs"
    dest.write_text("".join(out))
    print(f"wrote {dest} ({dest.stat().st_size} bytes, {len(ORDER)} tools)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
