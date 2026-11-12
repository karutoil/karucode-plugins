#!/bin/sh
# Build and register kc-image-handoff.
#   ./install.sh              # build + data-dir manifest
#   ./install.sh --no-build   # register the existing release binary
# Then POST /api/plugins/reload, or enable karucode's plugins_watch.
#
# The API key never goes through this script. Put it in one of:
#   ~/.config/karucode/config.toml:
#     [[plugins]]
#     ...
#     [plugins.env]
#     IMAGE_HANDOFF_API_KEY = "sk-..."
#   or settings.api_key_file = "/home/you/.config/image-handoff.key" (mode 0600)
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
BIN="$HERE/target/release/image-handoff"
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

DEST_DIR="$DATA_DIR/plugins/image-handoff"
mkdir -p "$DEST_DIR"
sed "s#^command = .*#command = [\"$BIN\"]#" "$HERE/plugin.toml" > "$DEST_DIR/plugin.toml"
chmod 0644 "$DEST_DIR/plugin.toml"
echo "==> registered $DEST_DIR/plugin.toml"
echo "    command: $BIN"

echo
echo "Apply it:"
echo "  curl -s -X POST localhost:8787/api/plugins/reload -H 'X-Karucode-Client: web' -H 'Content-Type: application/json' -d '{}' | jq .report"
echo "  curl -s localhost:8787/api/plugins | jq '.plugins[] | select(.name==\"image-handoff\")'"
echo
echo "Verify in a session: call plugin__image-handoff__status, then plugin__image-handoff__see with a real image."
