# karucode-plugins

A collection of plugins for [karucode](https://github.com/karutoil) — each one a
self-contained crate that speaks the karucode plugin protocol (JSON-RPC over
stdio: hooks, tools, and optionally a UI panel).

Everything here is MIT licensed. Each plugin has its own `README.md` with install
steps, configuration and the exact tool surface it registers; a plugin's
`plugin.toml` is the manifest form of its registration and points at the
`target/release/` binary it produces.

## Plugins

| Plugin | What it does |
| --- | --- |
| [`kc-background-terminals`](kc-background-terminals) | Start and supervise long-running commands across tool calls: read output, send stdin, stop the process group. |
| [`kc-chrome-devtools`](kc-chrome-devtools) | Drive and inspect a real Chrome/Chromium over the DevTools Protocol: pages, accessibility snapshots, clicks, fills, screenshots, heap snapshots. |
| [`kc-deep-research`](kc-deep-research) | A sustained background research loop with planning, query reformulation, source reading and cited per-aspect evidence dossiers. |
| [`kc-deepseek-pace`](kc-deepseek-pace) | Stops DeepSeek V4.1-Flash from narrating intent instead of working: a cached pacing contract, a stall detector over the live event stream, and steer/cancel recovery. |
| [`kc-discord-presence`](kc-discord-presence) | Shows what karucode is doing as Discord Rich Presence on the operator's profile — project, session, elapsed — over the local client's IPC socket, with an optional channel card and notifications. |
| [`kc-free-search`](kc-free-search) | Keyless multi-engine web search, page fetching, paginated document reading and a local FTS5 page cache, backed by `free-search-mcp`. |
| [`kc-goals`](kc-goals) | A goal an agent cannot quietly drop: recorded criteria with check commands, and automatic resume until every criterion is evidenced. |
| [`kc-image-handoff`](kc-image-handoff) | Hands an image to a vision model through a short-lived karucode session, so a text-only model can still read a screenshot or a diagram. |
| [`kc-lightpanda`](kc-lightpanda) | Drives [Lightpanda](https://github.com/lightpanda-io/browser) directly over its CDP WebSocket server — no Chromium, no Puppeteer. |
| [`kc-todo`](kc-todo) | A TODO list an agent actually keeps current, with an audit of open work, stale `in_progress` items and unproven completions, plus a Todo inspector tab. |
| [`kc-zero-loop-reviewer`](kc-zero-loop-reviewer) | A zero-loop CI code reviewer: patch-caused static analysis, sandboxed dual-commit reproduction probes, causal falsification, and at most three findings with a verified, committable patch. |

## Building

Each plugin is a normal Cargo crate:

```sh
cd kc-<name>
cargo build --release
./install.sh        # where the plugin ships one, or copy plugin.toml by hand
```

`target/` is ignored; nothing in this repository is prebuilt.

`kc-zero-loop-reviewer` is a Python plugin (it ships a `bin/` launcher and a
`plugin.toml` instead of a crate). Its suite and install steps are the same
shape: `cd kc-zero-loop-reviewer && ./check.sh`, then `./install.sh`.

## Layout

```
kc-<name>/
  Cargo.toml
  plugin.toml        # manifest form of the plugin registration
  src/               # main.rs + the vendored karucode SDK and protocol
  tests/
  README.md
  install.sh         # for the plugins that ship one
```

## Not in this repository

`.karucode/` (per-session runtime state: goals and TODO lists) is ignored, and so
are `kc-knowledge-graph/` and `kc-codegraph/`.
