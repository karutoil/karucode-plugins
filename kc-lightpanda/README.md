# kc-lightpanda

A native karucode plugin that drives [Lightpanda](https://github.com/lightpanda-io/browser) directly over its Chrome DevTools Protocol (CDP) WebSocket server. It does **not** launch Chromium, connect to Chrome, invoke Puppeteer, or use an LLM/MCP agent service.

Registered tools are namespaced as `plugin__lightpanda__<tool>`. A typical flow is `list_pages` → `new_page`/`navigate_page` → `take_snapshot` → `click`/`fill` → `evaluate_script`/`take_screenshot`.

## What it provides

| Tool | Behavior |
| --- | --- |
| `list_pages` | List the plugin-owned pages as `pageId`, current URL and title. |
| `new_page` | Open a URL on a fresh Lightpanda browser context; `foreground=false` preserves the plugin's previous active page. |
| `navigate_page` | Navigate and wait for the matching loaded-and-settled navigation lifecycle. |
| `take_snapshot` | Render an accessibility tree with element `uid`s. `max_nodes` defaults to 500 and is bounded by configuration. |
| `click` | Single- or double-click a snapshot uid via DOM coordinates, with a DOM-click fallback if no box is available. |
| `fill` | Fill inputs, textareas, selects, checkbox/radio controls, and contenteditable nodes; fire applicable input/change events. |
| `hover` | Dispatch mouseover, mouseenter and mousemove on a snapshot uid (or its parent when the uid resolves to a text node). |
| `evaluate_script` | Evaluate main-world JavaScript with `returnByValue`; optionally await a promise and surface thrown exceptions as errors. |
| `get_console` | Drain console and log events oldest-first; later calls do not repeat already reported messages. |
| `take_screenshot` | Save a uniquely named PNG under `screenshot_dir` or the system temporary directory and return path, byte count and dimensions. |
| `resize_page` | Set/clear CSS viewport dimensions and device scale; report the dimensions actually visible to the page. |

`uid`s are page- and snapshot-specific. Take another snapshot after navigation or a substantial DOM change. The mapping used by actions is the same saved snapshot the tool returned, not a newly generated one whose numbering could differ.

## Lightpanda is not Chromium

Lightpanda is designed for fast, lightweight automation. The following are intentional limits, not silent fallbacks:

- **Screenshots are text-rendered, not graphical browser pixels.** The tested renderer flows page text into blocks and writes a PNG. Do not use this plugin for Chrome pixel comparisons, CSS visual-regression testing or GPU/canvas validation.
- **Only PNG is exposed.** The tested CDP server rejects JPEG and WebP, so the tool schema does not offer them.
- **No `take_heap_snapshot`.** The tested CDP dispatcher has no `HeapProfiler` domain. The plugin deliberately does not advertise a misleading compatibility tool. (`Runtime.getHeapUsage` used manually is not a heap snapshot.)
- **One independent context per page.** Lightpanda permits one real target per CDP WebSocket. This plugin retains a separate socket/context for every page, with its own cookies/storage. These are isolated pages, not ordinary same-profile Chrome tabs. The default CDP server admits at most 16 simultaneous connections, so this is not intended for hundreds of tabs.
- **Only plugin-owned pages are listed.** Other clients of a shared server do not appear. One plugin process is shared by karucode sessions, however, so those sessions can see plugin-owned pages.
- Web-platform coverage varies by feature. Use Lightpanda's [WPT results](https://perf.lightpanda.io/wpt) for platform compatibility rather than assuming desktop Chrome equivalence.

## Installation

### 1. Build the plugin and install its browser backend

```sh
cd /path/to/karucode-plugins/kc-lightpanda
./install.sh --with-browser
```

`--with-browser` downloads the official binary from [Lightpanda's releases](https://github.com/lightpanda-io/browser/releases/tag/nightly) into this package's `target/browser/`. Supported direct-download targets are Linux and macOS on x86_64 and aarch64. Linux release binaries require glibc rather than musl. On Windows, install and run it inside WSL2 as described by upstream.

To use an existing backend instead:

```sh
LIGHTPANDA_PATH=/absolute/path/to/lightpanda ./install.sh
```

Or `./lightpanda version`-check a binary on `PATH`, `$HOME/.local/bin/lightpanda`, `/usr/local/bin/lightpanda`, or `/usr/bin/lightpanda`; the plugin never falls back to Chrome.

`./install.sh --no-build` skips compiling, `--print` prints the registration without writing it, and `./install.sh --uninstall` removes only the data-directory registration. Registration lands at:

```text
${KARUCODE_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/karucode}/plugins/lightpanda/plugin.toml
```

Do not also create a same-name `[[plugins]]` config entry: duplicate sources are an error. To register from config instead, adapt `plugin.toml` into this entry:

```toml
[[plugins]]
name = "lightpanda"
command = ["/absolute/path/to/kc-lightpanda/target/release/kc-lightpanda"]
enabled = true
permissions = ["shell", "network"]

[plugins.settings]
browser_path = "/absolute/path/to/lightpanda"
viewport = { width = 1440, height = 900 }
protocol_timeout_secs = 30
startup_timeout_secs = 30
max_snapshot_nodes = 500
screenshot_dir = "/tmp"
telemetry = false
obey_robots = false
extra_args = []
```

The host clears environment variables before starting plugins. Put backend configuration in `settings`, not in an interactive shell environment that the server may not share. The downloader itself honors `LIGHTPANDA_RELEASE` (default `nightly`) and `LIGHTPANDA_INSTALL_DIR` when you need a different binary destination.

### 2. Refresh the running host

```sh
curl -sS -X POST http://127.0.0.1:8787/api/plugins/reload \
  -H 'X-Karucode-Client: web' \
  -H 'Content-Type: application/json' \
  -d '{}'
curl -sS http://127.0.0.1:8787/api/plugins
```

`POST /api/plugins/reload` re-reads manifests and `[[plugins]]`, adds new plugins, removes deleted ones and restarts changed binaries. `POST /api/plugins/lightpanda/reload` reloads only this plugin. An in-flight agent keeps its original tool snapshot; start a new run/session to use a newly registered tool set in the model conversation.

The server needs no restart for installation or binary replacement. If `plugins_watch = true` and `plugins_watch_secs = 2` are set, it can poll and apply changes automatically instead.

## Settings

| Key / type | Default | Meaning |
|---|---:|---|
| `browser_path: string` | autodetect | Explicit Lightpanda executable. A bad path is an error, never a Chrome fallback. |
| `web_socket_url: string` | unset | Connect to an existing `ws://.../` CDP endpoint instead of spawning. The external server is not killed on plugin shutdown. Plain local WS is supported; TLS is not compiled in. |
| `viewport: {width,height}` | `1440x900` | Initial viewport on every page. |
| `protocol_timeout_secs: integer` | `30` | Timeout for each blocking CDP round trip. |
| `startup_timeout_secs: integer` | `30` | Local `/json/version` readiness timeout. |
| `max_snapshot_nodes: integer` | `2500` | Hard cap for a snapshot request, clamped to 1..100000. The tool itself defaults to 500. |
| `screenshot_dir: string` | system temp | Output directory for PNG files. |
| `extra_args: string[]` | `[]` | Extra arguments appended to `lightpanda serve`. Intended for trusted operators; do not put untrusted page content here. |
| `obey_robots: boolean` | `false` | Add `--obey-robots` when spawning the browser. |
| `telemetry: boolean` | `false` | Set `LIGHTPANDA_DISABLE_TELEMETRY=true` on the spawned browser unless enabled. |

The plugin also sets `LIGHTPANDA_DISABLE_CORE_DUMP=1` to avoid writing potentially sensitive page contents to crash dumps. Owned browsers bind a random loopback port with metrics disabled. Browser stdout/stderr/stdin never inherit the plugin protocol streams. Protocol EOF explicitly shuts down the owned browser; on Linux a parent-death signal also handles abrupt plugin termination. Non-Linux hosts use ordinary supervisor/group cleanup for forced termination.

Screenshots are created with exclusive names, mode `0600` on Unix, and failed writes remove the partial file. Because saving a screenshot writes a file, that tool uses the normal external approval gate even though image capture does not modify the page. Disconnected page sockets are dropped from the local registry; listing continues with surviving pages, while explicitly operating on a removed `pageId` is an error.

## Testing

The ordinary gate needs only Rust and karucode's `plugin sdk` command:

```sh
./check.sh
```

It checks SDK freshness and formatting, compiles every target, runs tests that need no browser (including malformed chunks, silent upgrade/read/write peers, and disconnected-page recovery), runs strict Clippy, and builds the release binary.

The real-browser gate uses no external website and no Chrome installation:

```sh
LIGHTPANDA_PATH=/absolute/path/to/lightpanda ./check.sh --live
# or directly:
LIGHTPANDA_PATH=/absolute/path/to/lightpanda \
  cargo test --test live -- --ignored --test-threads=1 --nocapture
```

The live suite verifies the real server identity/version, navigation and document readiness, synchronous/asynchronous JS and errors, console ordering/draining, uid single/double clicks, input/textarea/select/checkbox/contenteditable fills, hover, active-page selection, independent storage, viewport/media queries, private viewport/full-page PNG output, all 11 tools through JSON-RPC, and cleanup after graceful plugin stdin EOF or abrupt plugin termination. Process cleanup assertions require Linux `/proc`; the other browser tests are platform-neutral.

For CDP diagnostics on a **test page**, a debug build with `KC_LIGHTPANDA_DEBUG_CDP=1` logs commands and events to stderr. It may include page URLs, script source and page values; never enable it on sensitive sessions. Release builds do not include this output path.

### Tested backend

The initial port was exercised with official nightly:

```text
1.1.0-nightly.11745+1b60dcc7a
```

Source commit: [`1b60dcc7adfc12ffd18e2685130495cc837784cd`](https://github.com/lightpanda-io/browser/commit/1b60dcc7adfc12ffd18e2685130495cc837784cd). A moving nightly can change CDP coverage; rerun `--live` when upgrading it.
