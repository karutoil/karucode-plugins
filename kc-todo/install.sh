#!/bin/sh
# Install kc-todo as a karucode plugin.
#
# Registers the plugin by writing a manifest into the host's data directory, so
# the plugin is discovered on the next server start (there is no hot reload for a
# newly added plugin). This does not edit ~/.config/karucode/config.toml: a
# duplicate name in both places is reported as an error by the host, so the
# manifest form is used to avoid touching a config file you may hand-maintain.
#
# Usage:
#   ./install.sh                 # build if needed, then install the manifest
#   ./install.sh --print         # show the manifest instead of writing it
#   ./install.sh --uninstall     # remove the manifest
#
# Override the data directory with KARUCODE_DATA_DIR (default: the platform's
# data dir, ~/.local/share/karucode on Linux).

set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
binary="$here/target/release/kc-todo"

data_dir=${KARUCODE_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/karucode}
plugin_dir="$data_dir/plugins/kc-todo"
manifest="$plugin_dir/plugin.toml"

mode=install
for arg in "$@"; do
    case "$arg" in
        --print) mode=print ;;
        --uninstall) mode=uninstall ;;
        -h|--help)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

if [ "$mode" = uninstall ]; then
    if [ -f "$manifest" ]; then
        rm "$manifest"
        rmdir "$plugin_dir" 2>/dev/null || true
        echo "Removed $manifest"
        echo "Restart the karucode server for the change to take effect."
    else
        echo "Nothing to remove: $manifest does not exist."
    fi
    exit 0
fi

# Build when the binary is missing or older than the sources.
needs_build=0
if [ ! -x "$binary" ]; then
    needs_build=1
elif [ -n "$(find "$here/src" "$here/Cargo.toml" -newer "$binary" 2>/dev/null | head -1)" ]; then
    needs_build=1
fi
if [ "$needs_build" -eq 1 ] && [ "$mode" = install ]; then
    echo "Building kc-todo (release)..."
    (cd "$here" && cargo build --release)
fi

if [ ! -x "$binary" ]; then
    echo "The plugin binary is missing: $binary" >&2
    echo "Build it first:  cd $here && cargo build --release" >&2
    exit 1
fi

manifest_body="name = \"kc-todo\"
command = [\"$binary\"]
enabled = true
permissions = [\"files.read\", \"files.write\"]

[settings]
max_in_progress = 1
carry_over_reminder = true
compact_from = 24
list_path = \".karucode/todo/list.json\"
read_only_tools = [\"todo_list\", \"todo_audit\"]"

if [ "$mode" = print ]; then
    echo "# $manifest"
    echo "$manifest_body"
    exit 0
fi

mkdir -p "$plugin_dir"
printf '%s\n' "$manifest_body" > "$manifest"
echo "Installed the plugin manifest at:"
echo "  $manifest"
echo
echo "Next:"
echo "  1. Restart the karucode server (a newly added plugin is not hot-reloaded)."
echo "  2. Confirm it loaded:"
echo "       curl -s localhost:8787/api/plugins | jq '.plugins[] | select(.name==\"kc-todo\")'"
echo
echo "The first mutating tool call prompts for approval. Add more tool names to"
echo "settings.read_only_tools above if you want them to skip the prompt."
echo
echo "Optional: keep the list out of git by adding  .karucode/  to .gitignore."
