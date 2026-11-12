#!/bin/sh
# Install the background-terminals karucode plugin.
#
# ./install.sh              Build and register in the data directory.
# ./install.sh --no-build   Register an existing release binary.
# ./install.sh --print      Print the registration without installing it.
# ./install.sh --uninstall  Remove the data-directory registration.
# KARUCODE_DATA_DIR overrides the data directory; otherwise XDG_DATA_HOME or
# ~/.local/share is used. This does not edit config.toml.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
binary="$here/target/release/kc-background-terminals"
data_dir=${KARUCODE_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/karucode}
plugin_dir="$data_dir/plugins/background-terminals"
manifest="$plugin_dir/plugin.toml"
mode=install
build=1
for arg in "$@"; do
    case "$arg" in
        --no-build) build=0 ;;
        --print) mode=print ;;
        --uninstall) mode=uninstall ;;
        -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

if [ "$mode" = uninstall ]; then
    if [ -f "$manifest" ]; then
        rm "$manifest"
        rmdir "$plugin_dir" 2>/dev/null || true
        echo "Removed $manifest"
        echo "Rescan plugins to apply the change."
    else
        echo "Nothing to remove: $manifest does not exist."
    fi
    exit 0
fi

if [ "$mode" = install ] && [ "$build" = 1 ]; then
    (cd "$here" && cargo build --release)
fi
if [ "$mode" = install ] && [ ! -x "$binary" ]; then
    echo "Missing executable: $binary (run without --no-build)" >&2
    exit 1
fi

# TOML basic strings require escaping backslashes and double quotes.
escaped_binary=$(printf '%s' "$binary" | sed 's/\\/\\\\/g; s/"/\\"/g')
manifest_body=$(printf 'name = "background-terminals"\ncommand = ["%s"]\nenabled = true\npermissions = ["shell"]\n' "$escaped_binary")
if [ "$mode" = print ]; then
    printf '# %s\n%s\n' "$manifest" "$manifest_body"
    exit 0
fi
mkdir -p "$plugin_dir"
printf '%s\n' "$manifest_body" > "$manifest"
echo "Installed $manifest"
echo "Rescan plugins:"
echo "  curl -s -X POST localhost:8787/api/plugins/reload -H 'X-Karucode-Client: web' -H 'Content-Type: application/json' -d '{}'"
echo "Do not also register background-terminals in config.toml."
