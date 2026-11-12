# kc-free-search

A karucode plugin that puts [free-search-mcp](https://github.com/sweetcornna/free-search-mcp)
inside the agentic loop: keyless multi-engine web search, page fetching and
document reading, as 11 native tools with a supervised backend.

No API key. No account. Nothing to sign up for.

```
plugin__free-search__search("rust process group kill")   →  URLs + freshness-marked snippets
plugin__free-search__fetch("https://…")                  →  reader-mode Markdown of the page
plugin__free-search__research("how does RRF work")       →  sources and their full text in one call
```

## Why a plugin and not `[[mcp_servers]]`

karucode is already an MCP client, so this could have been three lines of config.
Five things are lost that way, and they are the whole reason this exists:

| | plain `[[mcp_servers]]` | this plugin |
| --- | --- | --- |
| a backend that died | **stays failed forever** — no reconnect, no health check | respawned on the next call, with a consecutive-failure budget that stops retrying a genuinely broken backend |
| a search that runs long | killed by the harness's **generic** 60 s plugin timeout | killed first (50 s), with a message that says how to narrow the request |
| tool descriptions | upstream's, **24 kB in total**, written for a chat client ("Best for / Not recommended for") | **6.1 kB**, written as a decision procedure, with the one mistake that matters per tool |
| approval | every call prompts (`external`; `read_only` mode denies) | the ten read-only tools are allowlisted, so search and fetch stop prompting |
| the discipline | only in `initialize.instructions`, which agents skim once | `before_prompt` on **every model call**, plus each tool's own description |

The last one is the point. An agent's characteristic failure with web search is
not "cannot find a URL" — it is answering from a snippet, stating a two-year-old
version number as current. So the rule is stated where it cannot be missed and
made checkable where it is used:

> A search result's snippet is an engine's summary, not the page. Read any date,
> version, price, deadline, rule or number from the page itself with `fetch`
> before you state it or act on it.

## Install

```bash
./install.sh                  # build + register (manifest in the data dir)
./install.sh --global         # also append a [[plugins]] entry to config.toml
```

Then, if the server is already running:

```bash
curl -s -X POST localhost:8787/api/plugins/reload \
     -H 'Content-Type: application/json' -d '{}' | jq .report
curl -s localhost:8787/api/plugins | jq '.plugins[] | select(.name=="free-search")'
```

Or set `plugins_watch = true` in the config and it notices on its own.

You need [`uv`](https://docs.astral.sh/uv/) on `PATH`; the backend is
`uvx --from free-search-mcp search-mcp`, which downloads and runs itself. The
first search on a fresh machine resolves about ninety Python packages (a minute or
so); every start after that is under a second.

Optional, for the browser-rendered engines (`brave`, `zhihu`, …):

```bash
uvx --from free-search-mcp playwright install chromium
```

## The tools

| tool | what it is for |
| --- | --- |
| `search` | find URLs: multi-engine, RRF-merged, deduplicated, freshness-marked |
| `fetch` | read one URL: page text, parsed document, or a description of a binary |
| `research` | search **and** read in one call — a brief with its sources' full text |
| `fetch_batch` | read up to 20 known URLs at once; per-URL failures do not fail the call |
| `read_doc` | paginated PDF / DOCX / XLSX / PPTX / EPUB / CSV / code / archive |
| `cache_search` | full-text search over pages already fetched, via local FTS5 |
| `engines` | list the sources, grouped by what they index |
| `compare` | fetch 2–5 URLs, excerpt them against one question |
| `extract_structured` | JSON-LD / OpenGraph / microdata as fields |
| `paper_graph` | one paper's citation graph, plus retraction notices |
| `download` | save a file to disk (the only tool with a side effect) |

Every upstream argument is preserved verbatim — all 52 `category` values, the four
`freshness` windows, every default. Only Pydantic's redundant `title` keys are
stripped. See `src/schemas.rs` for why the schemas are vendored and how drift is
reported.

## Settings

All optional; these are the shipped values. Full commentary in `plugin.toml`.

| key | default | meaning |
| --- | --- | --- |
| `command` | `["uvx","--from","free-search-mcp","search-mcp"]` | the backend to supervise; pin a version here |
| `call_timeout_secs` | `50` | backend budget, spent *after* the lock wait |
| `start_timeout_secs` | `120` | budget for the MCP handshake (the cold `uvx` install) |
| `max_output_bytes` | `20000` | under the harness's 24 000 so this plugin's better truncation note wins |
| `prewarm` | `true` | start the backend at plugin start so the first search is fast |
| `tools` | `[]` (all) | expose only these backend tools |
| `cache_dir` | — | sets `SEARCH_MCP_CACHE_DIR` |
| `env` | `{}` | upstream `SEARCH_MCP_*` variables; the only way to reach them |

The two timeouts are not arbitrary: `LOCK_WAIT_SECS (8) + call_timeout_secs (50)`
stays under karucode's 60 s plugin budget, so the model gets this plugin's
explanation rather than the harness's generic timeout. A test enforces it.

Note that karucode hands a plugin a **cleared environment**, so `env` is how
`SEARCH_MCP_PROXY`, `SEARCH_MCP_REGION` or an optional engine's key reach the
backend. Inherited `*_API_KEY`/`*_TOKEN`/`*_SECRET` are never passed through.

## The panel

The **Free search** inspector tab reports, cheaply and without starting anything:

* whether the backend is running, its pid, version and uptime;
* calls, errors, restarts, consecutive failures, last call duration and last error;
* the resolved command, timeouts, cache directory and which env keys are set;
* **schema drift**: tools the running backend advertises that this build does not,
  tools this build has that the backend dropped, and arguments that differ —
  checked by name, so a cosmetic upstream rewording does not raise a false alarm;
* the prompt cost saved: 24 151 B of upstream descriptions versus 6 132 B curated;
* `config.read_only_candidates`, the allowlist derived from the tool table so the
  manifest cannot silently drift from it.

## Development

```bash
cargo test --release              # 76 tests, no network needed
cargo build --release

python3 probe/drive_plugin.py        # protocol + panel checks, offline
python3 probe/drive_plugin.py --live # ...and a real search, cache hit, error path,
                                     #    and "no orphaned backend after shutdown"
```

`probe/` also holds the upstream probes: `probe_mcp.py` (handshake timing, schema
dump), `probe_calls.py` (per-tool latency), `probe_shapes.py` (result shapes) and
`gen_schemas.py` (regenerates `src/schemas.rs`).

After a backend upgrade:

```bash
python3 probe/probe_mcp.py --dump && python3 probe/gen_schemas.py
```

The panel shows the drift before and after, so this is a task with a due date
rather than a thing to remember.

After a karucode upgrade, refresh the vendored SDK:

```bash
karucode plugin sdk .            # rewrites src/sdk.rs and src/protocol.rs
karucode plugin sdk . --check    # exits 1 on drift; use it in CI
```

## Layout

| file | what it holds |
| --- | --- |
| `src/main.rs` | tool registration, `before_prompt`, the panel, the drift report |
| `src/mcp.rs` | the MCP stdio client and the backend supervisor |
| `src/tools.rs` | the curated descriptions, block rendering, truncation |
| `src/config.rs` | settings, defaults, the timeout budget arithmetic |
| `src/schemas.rs` | the vendored tool schemas (generated) |
| `src/sdk.rs`, `src/protocol.rs` | the vendored plugin SDK (generated) |

## Process lifecycle (the part that took the longest)

Supervising `uvx → search-mcp → Playwright` is where the real bugs were, and all
three looked correct before they were tested:

1. **`kill(-pgid, 0)` lies.** Once the group leader is a zombie it reports the
   group gone while live descendants remain. The group is enumerated from
   `/proc/<pid>/stat` instead.
2. **The leader exits before its work does.** `uvx` exec's the Python entry
   point, so our direct child can be gone with children still running. The pgid
   is parked and reaped rather than forgotten with the handle.
3. **A reload kills the plugin without its teardown.** `PR_SET_PDEATHSIG` covers
   the SIGKILL case; for the graceful one the pgid is recorded on disk
   (`$XDG_CACHE_HOME/karucode-free-search/backends/<pgid>` → owner pid) and the
   *next* process reaps it — but only when the owner is dead, so two plugin
   processes (a probe run, a dev copy) never reap each other's backends.

Verified by interleaving a probe run with a server reload four times: backends
stay at exactly 1. Before these fixes the same loop climbed 1 → 2 → 3 → 4 → 5.
`probe/inspect_group.py <pgid>` prints what the reaper sees for a group.

## Limits

* One backend request in flight at a time — the plugin's request loop is
  synchronous, so a concurrent call waits for the lock (bounded, see above).
* Images cannot be forwarded. `fetch(inline=true)` returns a real image block;
  karucode's plugin path carries text only, so the plugin says so explicitly
  instead of silently dropping the picture or pretending to have seen it.
* Tool results are rendered as text. `structuredContent` is used only when there
  is no text block, so `format="json"` is what you want when you need to parse.
* The backend is **not** sandboxed. It runs as the server user and fetches any
  URL you ask it to. Install plugins you trust, exactly like shell scripts.
* `download` writes outside the project directory, so it keeps prompting; the
  path it returns is worth showing the user before relying on it.
