#!/bin/sh
# Install kc-free-search as a karucode plugin.
#
#   ./install.sh              # build, then register the plugin
#   ./install.sh --no-build   # register the binary as it already stands
#   ./install.sh --global     # also expose it from ~/.config/karucode/config.toml
#
# Both registration routes are supported and they are equivalent:
#
#   * a manifest the host discovers at <data_dir>/plugins/free-search/plugin.toml
#     (what this script writes — `<data_dir>` defaults to ~/.local/share/karucode)
#   * a `[[plugins]]` entry in ~/.config/karucode/config.toml (--global)
#
# Never both: a duplicate name is reported as an error naming both sources rather
# than silently running one of them.
#
# Nothing here restarts the server. If it is already running, ask it to rescan:
#
#   curl -s -X POST localhost:8787/api/plugins/reload -H 'Content-Type: application/json' -d '{}'
#
# (or set plugins_watch = true and it happens on its own).

set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
BIN="$HERE/target/release/free-search"
DATA_DIR="${KARUCODE_DATA_DIR:-$HOME/.local/share/karucode}"
CONFIG="${KARUCODE_CONFIG:-$HOME/.config/karucode/config.toml}"
BUILD=1
GLOBAL=0

for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    --global) GLOBAL=1 ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

if [ "$BUILD" = 1 ]; then
  echo "==> building the plugin"
  ( cd "$HERE" && cargo build --release )
fi

if [ ! -x "$BIN" ]; then
  echo "error: $BIN does not exist or is not executable; run without --no-build" >&2
  exit 1
fi

# The manifest ships with an absolute path for this machine. Rewrite it if the
# checkout lives somewhere else, so the plugin does not start with "command not
# found" on a copy of the repo.
DEST_DIR="$DATA_DIR/plugins/free-search"
mkdir -p "$DEST_DIR"
sed "s#^command = .*#command = [\"$BIN\"]#" "$HERE/plugin.toml" > "$DEST_DIR/plugin.toml"

echo "==> registered $DEST_DIR/plugin.toml"
echo "    command: $BIN"

if [ "$GLOBAL" = 1 ]; then
  touch "$CONFIG"
  if grep -q '^name = "free-search"$' "$CONFIG"; then
    echo "==> $CONFIG already has a free-search entry; left untouched"
  else
    cat >> "$CONFIG" <<EOF

# ── Free search (kc-free-search) ───────────────────────────────────────────
# Keyless multi-engine web search behind a supervised backend. The settings
# table is optional; these are the shipped defaults. NOTE: this entry and the
# manifest at $DEST_DIR are two sources for the same name. Use one.
[[plugins]]
name = "free-search"
command = ["$BIN"]
enabled = true
# files.write is for `download` alone: the host withholds a `mutating` tool
# without it. `download` is not in read_only_tools, so it still prompts.
permissions = ["shell", "network", "files.write"]

[plugins.settings]
call_timeout_secs = 50
prewarm = true
read_only_tools = [
  "search", "fetch", "research", "fetch_batch", "read_doc",
  "cache_search", "engines", "compare", "extract_structured", "paper_graph",
]
EOF
    echo "==> appended a [[plugins]] entry to $CONFIG"
    echo "    (remove one of the two sources, or the host reports a duplicate)"
  fi
fi

# Is the backend reachable at all? This is the failure that costs the most time
# to diagnose later, so it is checked now rather than on the first search.
echo "==> checking the backend command"
if command -v uvx >/dev/null 2>&1; then
  echo "    uvx found at $(command -v uvx)"
else
  echo "    WARNING: uvx is not on PATH. Install uv (https://docs.astral.sh/uv/) or"
  echo "    point settings.command at a Python entry point you have already installed."
fi

echo
echo "Next:"
echo "  curl -s -X POST localhost:8787/api/plugins/reload \\"
echo "       -H 'Content-Type: application/json' -d '{}' | jq .report"
echo "  curl -s localhost:8787/api/plugins | jq '.plugins[] | select(.name==\"free-search\")'"
echo
echo "Then call plugin__free-search__search, and open the \"Free search\" inspector tab"
echo "to see whether the backend is healthy and whether its schemas have drifted."
