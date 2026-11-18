#!/bin/sh
# Build and register kc-discord-presence.
#   ./install.sh              # build + data-dir manifest
#   ./install.sh --no-build   # register the existing release binary
#   ./install.sh --reload     # ... and rescan the host afterwards
#
# No credential goes through this script. The Discord webhook URL is pasted into
# a chat and stored by `discord_configure` in
# <data_dir>/discord-presence/state.json (mode 0600).
#
# `--reload` needs KARUCODE_TOKEN (an admin-scoped application API key, or the
# server's auth token) because plugin routes need admin. Without it, print the
# curl to run.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
BIN="$HERE/target/release/discord-presence"
DATA_DIR="${KARUCODE_DATA_DIR:-$HOME/.local/share/karucode}"
SERVER="${KARUCODE_SERVER:-http://127.0.0.1:8787}"
BUILD=1
RELOAD=0
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    --reload) RELOAD=1 ;;
    -h|--help) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

DEST_DIR="$DATA_DIR/plugins/discord-presence"
mkdir -p "$DEST_DIR"
sed "s#^command = .*#command = [\"$BIN\"]#" "$HERE/plugin.toml" > "$DEST_DIR/plugin.toml"
chmod 0644 "$DEST_DIR/plugin.toml"
echo "==> registered $DEST_DIR/plugin.toml"
echo "    command: $BIN"

reload() {
  if [ -n "${KARUCODE_TOKEN:-}" ]; then
    curl -s -X POST "$SERVER/api/plugins/reload" \
      -H "Authorization: Bearer $KARUCODE_TOKEN" \
      -H 'X-Karucode-Client: web' -H 'Content-Type: application/json' -d '{}'
    echo
  else
    echo
    echo "Apply it (needs an admin-scoped token):"
    echo "  curl -s -X POST $SERVER/api/plugins/reload \\"
    echo "    -H 'Authorization: Bearer \$KARUCODE_TOKEN' -H 'X-Karucode-Client: web' \\"
    echo "    -H 'Content-Type: application/json' -d '{}' | jq .report"
    echo "  curl -s -H 'Authorization: Bearer \$KARUCODE_TOKEN' $SERVER/api/plugins \\"
    echo "    | jq '.plugins[] | select(.name==\"discord-presence\")'"
  fi
}

# Always print (or perform) the reload: without a token it prints the curl, and
# with one it applies the change. `--reload` is accepted for symmetry with the
# other plugins' installers.
reload

cat <<'EOF'

Next: in a karucode chat, ask for the Discord presence and paste your Application ID.
The agent calls discord_configure, which stores it and publishes the presence.
  Discord: https://discord.com/developers/applications -> New Application -> Application ID
           (the name you give it is the "Playing ..." line; the ID is public, not a secret)
  Requires the Discord desktop client running on this machine.
Verify:   plugin__discord-presence__discord_status
EOF
