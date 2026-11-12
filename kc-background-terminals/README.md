# kc-background-terminals

A karucode plugin for starting and supervising long-running commands across tool calls. Unlike a foreground shell call, a command can keep running while the agent reads its output, sends input, or works on something else.

## Install

Requires Rust/Cargo and a Unix-like environment with `/bin/sh`.

```sh
cd kc-background-terminals
./install.sh                  # build release binary and register it
./install.sh --print          # print the registration without installing
./install.sh --no-build       # register an existing release binary
./install.sh --uninstall      # remove the data-directory registration
```

The registration is written to `<data_dir>/plugins/background-terminals/plugin.toml`. `KARUCODE_DATA_DIR` overrides the data directory; otherwise it is `${XDG_DATA_HOME:-$HOME/.local/share}/karucode`. The manifest points at the binary in this checkout, so keep the checkout and binary in place. The checked-in `plugin.toml` is an example: its command must be replaced with an absolute path before manual installation. Do not also add the same plugin name in a `[[plugins]]` config entry; duplicate registrations fail.

Apply installation, rebuilds or removal to a running server with a rescan, or enable `plugins_watch`:

```sh
curl -s -X POST localhost:8787/api/plugins/reload \
  -H 'X-Karucode-Client: web' -H 'Content-Type: application/json' -d '{}'
curl -s localhost:8787/api/plugins | jq '.plugins[] | select(.name == "background-terminals")'
```

## Tools

The host prefixes these names with `plugin__background-terminals__`. Results are JSON text.

| Tool | Arguments | Purpose |
| --- | --- | --- |
| `terminal_start` | `command` (required), `cwd` (optional), `timeout_ms` (optional) | Start a shell command; default working directory is the host call's workspace. Relative `cwd` is resolved against that workspace; without one, supply an absolute `cwd`. Returns an ID. |
| `terminal_list` | none | List managed commands and their status. |
| `terminal_read` | `id` (required), `cursor` (optional) | Read retained combined output. Pass the returned byte cursor on the next call to continue. |
| `terminal_write` | `id`, `input` (both required) | Write text to the command's stdin. Include a newline when the command expects one. |
| `terminal_stop` | `id` (required) | Stop the command's process group. |

All calls require a host-provided session ID and workspace. Terminals are visible and controllable only within that session and workspace; older hosts without this context are not supported. Absolute `cwd` paths are allowed and are not workspace-confined.

Output is bounded to the most recent 256 KiB per terminal, with at most 64 KiB returned per read. If a reader falls behind, older bytes can be discarded and the read result signals truncation. Cursors are byte offsets, not character counts; output is decoded lossily as UTF-8, so binary data and characters split across reads may be corrupted. Stdout and stderr ordering is not guaranteed. Writes accept up to 64 KiB and may be partial or zero; retry the unwritten bytes using the returned `written` count. Up to 128 records are retained; when full, the oldest completed record is evicted, and starts fail if all are running.

## Lifecycle and security

`timeout_ms` is an optional positive lifetime limit; without it a command runs until it exits, is stopped, or the plugin shuts down. On Unix the plugin sends SIGKILL to the process group on stop, timeout, and when the shell exits. Commands run through `/bin/sh -c` with piped stdin, stdout and stderr, **not a PTY**. Programs requiring an interactive terminal, terminal sizing, or terminal control sequences may not work correctly. A started command runs in its own Unix process group. Stop, timeout, and normal plugin shutdown terminate that group; a process that deliberately leaves the group is outside this cleanup. Do not rely on cleanup after an abrupt plugin or host crash. Reloading or removing the plugin replaces/stops its process and disrupts managed commands; terminal IDs and buffered output are in memory, not durable.

The registration grants `shell`; shell-class tools are withheld without that permission. Plugin tools are external and subject to the host approval policy. The plugin and its commands run with the server user's OS permissions; plugin permissions are not a sandbox. The host gives plugins a cleared environment with a small allowlist, so commands do not automatically inherit server credentials. Avoid putting secrets in commands or output.

## Development

```sh
cargo test
cargo build --release
```
