#!/bin/sh
# Install into this machine's karucode data dir.
#
# A local directory is a valid install source; the installer copies the plugin,
# rewrites `command` to the installed path and never grants operator-only
# settings.  On this machine the installer stages under /tmp, which is a
# different filesystem from ~/.local, so a local install can fail with EXDEV;
# the fallback below performs the same self-contained copy.
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

data_dir="${KARUCODE_DATA_DIR:-$HOME/.local/share/karucode}"
previous=""
for arg in "$@"; do
  case "$previous" in
    --data-dir) data_dir="$arg" ;;
  esac
  case "$arg" in
    --data-dir=*) data_dir="${arg#--data-dir=}" ;;
  esac
  previous="$arg"
done
if [ -z "$data_dir" ]; then
  echo "--data-dir needs a path" >&2
  exit 2
fi

if command -v karucode >/dev/null 2>&1; then
  if karucode plugin install "$here" "$@"; then
    echo
    echo "Installed through karucode."
    exit 0
  fi
  echo "karucode plugin install failed; falling back to a direct self-contained copy." >&2
else
  echo "karucode is not on PATH; copying directly into $data_dir/plugins." >&2
fi

dest="$data_dir/plugins/zero-loop-reviewer"
rm -rf "$dest"
mkdir -p "$dest"
cp -a "$here/bin" "$here/zero_loop_reviewer" "$here/rules" "$here/plugin.toml" "$here/pyproject.toml" "$here/README.md" "$dest/"
find "$dest" -type d -name '__pycache__' -prune -exec rm -rf {} + 2>/dev/null || true
find "$dest" -type d -name '.pytest_cache' -prune -exec rm -rf {} + 2>/dev/null || true

python3 - "$dest/plugin.toml" "$dest/bin/zero-loop-reviewer" <<'PY'
import re
import sys
from pathlib import Path

manifest = Path(sys.argv[1])
command = sys.argv[2]
text = manifest.read_text(encoding="utf-8")
text = re.sub(r'^command = .*$', f'command = ["{command}"]', text, count=1, flags=re.MULTILINE)
manifest.write_text(text, encoding="utf-8")
PY
chmod +x "$dest/bin/zero-loop-reviewer"

echo
echo "Installed at $dest"
echo "Next: configure a model endpoint (Settings > Plugins & MCP or plugin.toml):"
echo "  llm.base_url / llm.api_key_env   e.g. http://127.0.0.1:4000/v1 + ZERO_LOOP_LLM_API_KEY"
echo "  sandbox.runtime = podman         and pull \$sandbox.image once"
echo "  ci.provider + ci.token_env       for review_pr post=true / zero-loop-reviewer ci"
