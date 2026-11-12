# kc-deep-research

A sustained research loop built on the same [`free-search-mcp`](https://github.com/sweetcornna/free-search-mcp) backend as `kc-free-search`, with planning, repeated query reformulation, source reading, evidence coverage and explicit stopping criteria.

This is deliberately **not** another `search → three snippets → answer` wrapper. It runs in the background and keeps working across tool calls. Every accepted source was fetched and checked against the question and an aspect. Search snippets are discovery leads only and never appear as evidence.

```
plugin__deep-research__start   → a scoped background job and id
plugin__deep-research__status  → wait up to 45s for real progress / terminal state
plugin__deep-research__report  → paginated, cited evidence dossier + gaps + query audit
plugin__deep-research__cancel  → cooperative cancellation after the current MCP call
plugin__deep-research__list    → jobs visible in this session + workspace
```

The ordinary `plugin__free-search__*` tools remain unchanged and available for targeted follow-up. Use free-search for one fact or URL; use deep-research for a broad, multi-faceted or high-confidence investigation.

## 1. Install beside free-search

Requirements: Rust/Cargo to build, and [`uv`](https://docs.astral.sh/uv/) for the default backend command. No API key is needed.

```bash
cd kc-deep-research
./install.sh                  # release build + data-dir manifest
./install.sh --no-build       # register an existing release binary

curl -s -X POST localhost:8787/api/plugins/reload \
  -H 'Content-Type: application/json' -d '{}' | jq .report
curl -s localhost:8787/api/plugins | jq \
  '.plugins[] | select(.name=="deep-research" or .name=="free-search")'
```

Or set karucode's `plugins_watch = true`. The install does not replace, disable or edit `free-search`.

All plugin tools remain `external` under karucode's approval policy. With `require_approval = "ask"`, every status poll can prompt. An **operator** can register through `~/.config/karucode/config.toml` instead of the data-directory manifest and explicitly allow these project-safe job/search calls:

```toml
[[plugins]]
name = "deep-research"
command = ["/absolute/path/kc-deep-research/target/release/deep-research"]
enabled = true
permissions = ["shell", "network"]

[plugins.settings]
read_only_tools = ["start", "status", "report", "list", "cancel"]
# Optional backend settings:
call_timeout_secs = 50
start_timeout_secs = 120
max_active_jobs = 2
max_output_bytes = 20000
# command = ["uvx", "--from", "free-search-mcp==0.17.0", "search-mcp"]
# cache_dir = "/home/me/.cache/search-mcp"
# env = { SEARCH_MCP_REGION = "us-en" }
```

`read_only_tools` is intentionally **not** shipped in the data-dir manifest: karucode accepts that exemption only from operator-owned config. Do not register the same name in both places; duplicate sources are an error.

## 2. Use it

### Start once

```json
{
  "question": "Model Context Protocol (MCP)",
  "aspects": ["tools and capabilities", "security and consent"],
  "min_sources": 8,
  "min_domains": 3,
  "min_rounds": 3
}
```

`aspects` is the coverage contract. If supplied, they replace the generic context/evidence/limitations checklist (the core question is always retained). This is the best way to steer the calling agent's understanding of what “everything needed” means. Without explicit aspects, the planner covers:

1. the direct answer;
2. definitions/background/architecture;
3. evidence/examples/data/applications;
4. limitations/criticisms/risks/open questions;
5. current developments for a time-sensitive question, and comparisons for a decision/comparison question.

### Wait for actual completion

Call `status` with the returned id and `wait_secs = 45`, then keep calling while `terminal` is false. A status timeout is still a healthy running job, not a failure.

```json
{"id":"dr-12345-1", "wait_secs":45}
```

The worker runs independently between calls. `start`, `status`, `report` and `list` never spawn work during plugin initialization, so karucode's 10-second handshake is not exposed to a cold Python install. Each status wait is capped at 45 seconds, safely below the 60-second plugin tool-call budget. One session-scoped `notify` frame is emitted when a job becomes terminal, so the UI can alert the user without progress-notification spam; the agent still uses `status` to inspect the result.

### Read the evidence report

```json
{"id":"dr-12345-1", "format":"markdown", "max_bytes":20000}
```

Follow the result metadata's `next_offset` while `has_more` is true. JSON reports are UTF-8 byte chunks: concatenate chunks exactly, then parse the complete JSON; an individual page is not represented as valid standalone JSON. Markdown pages include the next cursor and stay within `max_bytes`.

The report contains:

- status and exact stopping reason;
- per-aspect coverage and query-attempt counts;
- targeted excerpts from fetched, reader-parsed source text (whitespace normalized);
- source URLs/titles, published/retrieved dates, fetch method, cache age, content fingerprint and truncation flag;
- distinct host/content groups for the core coverage check;
- per-URL fetch failures and rejected-page count;
- every query, hit count and newly discovered candidate count.

The calling agent should synthesize a response from this dossier, cite the source URLs actually read, and surface unresolved aspects. The plugin intentionally does not write an unsourced narrative and call it research.

## 3. What makes it keep researching

Each round selects the aspects with the least accepted evidence. It then:

1. reformulates uncovered/under-covered aspects (official sources, research/data/case studies, criticism/alternatives, technical references, recent developments);
2. calls free-search-mcp `search(format="json")`;
3. queues unique HTTP(S) URLs, with domain filters applied to discovered links too;
4. reads selected pages with `fetch_batch(format="json")`;
5. rejects empty, failed, too-short or insufficiently relevant pages;
6. extracts aspect-targeted evidence and relevant outbound documentation/reference links for citation chasing;
7. recomputes gaps and starts another round.

The engine uses raw MCP `structuredContent`, not the adjacent plugin's rendered/truncated text. It observes `search`'s object shape and `fetch_batch`'s `structuredContent.result` array. Per-URL errors inside an otherwise successful batch remain failures, never sources.

### Completion predicate

`state = "complete"` requires **all** of the following after a fully ingested round:

- `round >= min_rounds`;
- accepted, deduplicated read sources `>= min_sources`;
- distinct host domains `>= min_domains`;
- at least one relevant fetched source for **every** aspect;
- two distinct core source groups by default, where each group consumes a unique host **and** a unique normalized-content fingerprint.

The final allowed round can complete: coverage is checked before the budget stop. `min_sources = 0` is not accepted; setting both it and `min_rounds` to 1 permits a deliberately shallow run, but does not add new aspects or turn snippets into evidence.

### Partial is honest

`state = "partial"` means the evidence exists but the contract did not finish. Reasons include:

- the explicit round/search/source/time budget;
- three consecutive rounds with no new relevant source after uncovered aspects were retried;
- all six query variants and all discovered candidates exhausted;
- repeated backend failures after some evidence was collected.

`failed` means a backend failure/panic prevented any evidence. `cancelled` preserves all evidence collected before cancellation. None of these states is presented as complete.

### Hard stops matter

No plugin can infer an unbounded “everything” for every question. The default and maximum budgets make the research finite and inspectable. Status and the report expose exactly which facet remains missing, so the agent can launch a focused new aspect or use free-search manually instead of hallucinating coverage.

## 4. Budgets and freshness

| start argument | default | bounds | meaning |
| --- | ---: | ---: | --- |
| `max_rounds` | 12 | 1–40 | complete search/read rounds |
| `max_searches` | 36 | 1–120 | search calls across all aspects |
| `max_sources` | 24 | 1–60 | accepted source cap (batch fetch cannot overshoot it) |
| `min_sources` | 8 | 1–60 | accepted-source coverage target; must be `<= max_sources` |
| `min_domains` | 3 | 1–20 | distinct **host** diversity target, not organizational independence |
| `min_rounds` | 3 | 1–40 | prevents one-round shallow completion; must be `<= max_rounds` |
| `max_minutes` | 20 | 1–180 | cooperative wall-clock deadline, checked between MCP calls |
| `results_per_search` | 8 | 3–20 | URLs requested per search |
| `force_refresh` | false | bool | bypass search/page caches; use fresh single-URL fetches |
| `freshness` | — | day/week/month/year | upstream search freshness filter |
| `category` | — | string | upstream source category (for example `paper`, `docs`, `gov.us`) |
| `engines` | all healthy | string array | optional upstream engine selection |
| `include_domains` / `exclude_domains` | — | string array | scope both search results and citation-chased URLs |

With ordinary settings, both plugins use the same upstream default cache (`SEARCH_MCP_CACHE_DIR`), so a page already fetched by free-search can be reused. `cache_dir` and `settings.env` configure that backend environment explicitly because karucode gives plugins a cleared environment.

`force_refresh = true` sets `use_cache=false` and `max_age_hours=0` for search. Because upstream `fetch_batch` has no refresh switch, the adapter then deliberately calls `fetch` once per selected URL with `force_refresh=true, max_age_hours=0`. This is slower and the report shows the actual retrieval/cache metadata.

The time budget cannot interrupt a blocking OS read itself. One in-flight backend call may finish or hit its configured timeout (50s by default; a cold handshake has 120s) before the deadline or cancellation is observed.

## 5. Settings and lifecycle

| plugin setting | default |
| --- | --- |
| `command` | `uvx --from free-search-mcp search-mcp` |
| `call_timeout_secs` | 50 |
| `start_timeout_secs` | 120 |
| `max_active_jobs` | 2 (clamped 1–4) |
| `max_output_bytes` | 20000 (clamped 1000–20000) |
| `cache_dir` | upstream default |
| `env` | `{}` |

Each active job owns one separately supervised backend and process group; two jobs are allowed by default. The companion uses its own orphan-record directory (`karucode-deep-research`) so it cannot reap a live free-search backend. The vendored client retains free-search's handshake, restart, timeout, `/proc` process-group teardown and parent-death safeguards.

Job state is **process-local**, retained in memory and scoped by both host `session_id` and `workspace`. Guessing another session's id does not expose it. At 64 retained jobs, starting another evicts the oldest terminal job first; active jobs are never evicted for this purpose. **Collect reports before plugin reload/server restart.** Active work is cancelled on shutdown/reload; no checkpoint is claimed.

## 6. Verify

```bash
cargo fmt --check
cargo test --release -- --test-threads=1
cargo clippy --all-targets -- -D warnings
python3 probe/drive_plugin.py           # real binary + scripted MCP; fully offline
python3 probe/drive_plugin.py --live    # real binary + actual web searches/fetches
karucode plugin sdk . --check           # detect stale vendored SDK after an upgrade
```

The deterministic suite exercises iterative success, aspect starvation, off-topic/short content, failed and mixed fetches, mirrors, each budget, cancellation, idempotent terminal cancel, scoped visibility, all query variants, and UTF-8 report pagination. The protocol probe additionally drives five jobs through the real release binary, reconstructs a complete JSON dossier from pages, confirms no snippet leakage, verifies `force_refresh` routing, and checks backend PIDs disappear after shutdown.

`probe/fake_search_mcp.py` is a real MCP stdio fixture, not a mock of the plugin's engine. Rust unit tests separately inject a scripted gateway to make exact stopping behavior deterministic.

### Layout

| file | responsibility |
| --- | --- |
| `src/main.rs` | host tools, worker lifecycle, scoped job access, static prompt, panel |
| `src/engine.rs` | iterative rounds, adaptive priorities, candidates, stopping |
| `src/model.rs` | evidence/aspect/coverage models, snapshots, condvar job store |
| `src/evidence.rs` | lexical relevance, parsed-text excerpts, URL/domain/fingerprint handling |
| `src/plan.rs` | generic/explicit aspects and finite query reformulation |
| `src/gateway.rs` | raw free-search-mcp search/fetch/fetch_batch integration |
| `src/report.rs` | Markdown/JSON dossier and bounded cursor pagination |
| `src/mcp.rs` | adapted supervised MCP stdio/process-group client from kc-free-search |
| `src/sdk.rs`, `src/protocol.rs` | vendored karucode SDK; regenerate with `karucode plugin sdk .` |

## Limits worth stating

- Lexical relevance establishes **matching coverage**, not factual support. Negated claims and contradictions can match the same words; read linked sources and do not resolve disagreement by counting domains.
- Distinct host names are not independent organizations. Content fingerprints catch exact normalized mirrors, not paraphrased syndication.
- Excerpts come from upstream reader-mode/document parsing, not original page bytes. Whitespace is normalized, and the report marks a backend-truncated fetch.
- The loop has no model-provider dependency and does not call an LLM to “approve” its own evidence. Deliberate reasoning/synthesis belongs to the karucode agent using the dossier.
- Search quality remains bounded by the public engines and pages free-search-mcp can reach. Blocked/paywalled/offline material appears as gaps or failures, never as imaginary support.
