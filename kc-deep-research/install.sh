#!/bin/sh
# Build and register kc-deep-research beside kc-free-search.
#   ./install.sh              # build + data-dir manifest
#   ./install.sh --no-build   # register the existing release binary
# Then POST /api/plugins/reload, or enable karucode's plugins_watch.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
BIN="$HERE/target/release/deep-research"
DATA_DIR="${KARUCODE_DATA_DIR:-$HOME/.local/share/karucode}"
BUILD=1
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    -h|--help) sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

if [ "$BUILD" = 1 ]; then
  (cd "$HERE" && cargo build --release)
fi
if [ ! -x "$BIN" ]; then
  echo "error: $BIN does not exist or is not executable" >&2
  exit 1
fi

DEST_DIR="$DATA_DIR/plugins/deep-research"
mkdir -p "$DEST_DIR"
sed "s#^command = .*#command = [\"$BIN\"]#" "$HERE/plugin.toml" > "$DEST_DIR/plugin.toml"
echo "==> registered $DEST_DIR/plugin.toml"
echo "    command: $BIN"
if command -v uvx >/dev/null 2>&1; then
  echo "    backend: uvx --from free-search-mcp search-mcp"
else
  echo "    WARNING: uvx is not on PATH; install uv or change settings.command" >&2
fi

echo
echo "Apply alongside free-search:"
echo "  curl -s -X POST localhost:8787/api/plugins/reload -H 'Content-Type: application/json' -d '{}' | jq .report"
echo "  curl -s localhost:8787/api/plugins | jq '.plugins[] | select(.name==\"deep-research\" or .name==\"free-search\")'"
echo
echo "Call plugin__deep-research__start once, then status(wait_secs=45) until terminal, then report."
