#!/bin/sh
# Install the Lightpanda karucode plugin without restarting the host.
#
# ./install.sh                  Build and register using an installed browser.
# ./install.sh --with-browser   Also download the official Lightpanda binary.
# ./install.sh --no-build       Register an existing release binary.
# ./install.sh --print          Print the registration without writing files.
# ./install.sh --uninstall      Remove only the data-directory registration.
#
# LIGHTPANDA_PATH selects an existing browser. With --with-browser,
# LIGHTPANDA_RELEASE defaults to nightly and LIGHTPANDA_INSTALL_DIR defaults to
# this package's target/browser. Never register this same name in config.toml too.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
binary="$here/target/release/kc-lightpanda"
data_dir=${KARUCODE_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/karucode}
plugin_dir="$data_dir/plugins/lightpanda"
manifest="$plugin_dir/plugin.toml"
browser=${LIGHTPANDA_PATH:-}
release=${LIGHTPANDA_RELEASE:-nightly}
mode=install
build=1
with_browser=0

for arg in "$@"; do
    case "$arg" in
        --with-browser) with_browser=1 ;;
        --no-build) build=0 ;;
        --print) mode=print ;;
        --uninstall) mode=uninstall ;;
        -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; exit 2 ;;
    esac
done

if [ "$mode" = uninstall ]; then
    if [ -f "$manifest" ]; then
        rm -- "$manifest"
        rmdir "$plugin_dir" 2>/dev/null || true
        echo "Removed $manifest. Rescan plugins to apply the removal."
    else
        echo "Nothing to remove: $manifest does not exist."
    fi
    exit 0
fi

if [ "$mode" = install ] && [ "$build" = 1 ]; then
    echo "==> Building kc-lightpanda (release)"
    (cd "$here" && cargo build --release)
fi
if [ "$mode" = install ] && [ ! -x "$binary" ]; then
    echo "Missing executable: $binary (run without --no-build)" >&2
    exit 1
fi

if [ "$mode" = print ]; then
    with_browser=0
fi
if [ "$with_browser" = 1 ]; then
    arch=$(uname -m)
    system=$(uname -s)
    case "$system:$arch" in
        Linux:x86_64|Linux:amd64) asset=lightpanda-x86_64-linux ;;
        Linux:aarch64|Linux:arm64) asset=lightpanda-aarch64-linux ;;
        Darwin:x86_64|Darwin:amd64) asset=lightpanda-x86_64-macos ;;
        Darwin:aarch64|Darwin:arm64) asset=lightpanda-aarch64-macos ;;
        *) echo "No official direct-download asset for $system $arch; install manually." >&2; exit 1 ;;
    esac
    install_dir=${LIGHTPANDA_INSTALL_DIR:-"$here/target/browser"}
    mkdir -p "$install_dir"
    browser="$install_dir/lightpanda"
    download="$browser.download.$$"
    trap 'rm -f -- "$download"' EXIT HUP INT TERM
    echo "==> Downloading Lightpanda $release for $system/$arch"
    curl --fail --location --retry 3 --connect-timeout 10 --max-time 300 \
        --output "$download" \
        "https://github.com/lightpanda-io/browser/releases/download/$release/$asset"
    chmod 755 "$download"
    echo "==> Verifying the downloaded executable"
    LIGHTPANDA_DISABLE_TELEMETRY=true LIGHTPANDA_DISABLE_CORE_DUMP=1 "$download" version
    mv -f -- "$download" "$browser"
    trap - EXIT HUP INT TERM
fi

if [ -z "$browser" ]; then
    if [ -x "$HOME/.local/bin/lightpanda" ]; then
        browser="$HOME/.local/bin/lightpanda"
    elif command -v lightpanda >/dev/null 2>&1; then
        browser=$(command -v lightpanda)
    elif [ -x "$here/target/browser/lightpanda" ]; then
        browser="$here/target/browser/lightpanda"
    elif [ -x /usr/local/bin/lightpanda ]; then
        browser=/usr/local/bin/lightpanda
    fi
fi
if [ "$mode" = install ]; then
    if [ -z "$browser" ] || [ ! -x "$browser" ]; then
        echo "No executable Lightpanda backend found. Use --with-browser or LIGHTPANDA_PATH=/absolute/path." >&2
        exit 1
    fi
    echo "==> Browser: $(LIGHTPANDA_DISABLE_TELEMETRY=true LIGHTPANDA_DISABLE_CORE_DUMP=1 "$browser" version)"
fi
if [ -z "$browser" ]; then
    browser="$here/target/browser/lightpanda"
fi

# Escape TOML basic strings; this also keeps paths with quotation marks intact.
escape_toml() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }
escaped_binary=$(escape_toml "$binary")
escaped_browser=$(escape_toml "$browser")
manifest_body=$(cat <<EOF
name = "lightpanda"
command = ["$escaped_binary"]
enabled = true
permissions = ["shell", "network"]

[settings]
browser_path = "$escaped_browser"
viewport = { width = 1440, height = 900 }
protocol_timeout_secs = 30
startup_timeout_secs = 30
max_snapshot_nodes = 500
telemetry = false
obey_robots = false
extra_args = []
EOF
)

if [ "$mode" = print ]; then
    printf '# Would write %s\n%s\n' "$manifest" "$manifest_body"
    exit 0
fi
mkdir -p "$plugin_dir"
printf '%s\n' "$manifest_body" > "$manifest"
echo "==> Registered $manifest"
echo
printf '%s\n' "Apply the registration:"
printf '%s\n' "  curl -sS -X POST http://127.0.0.1:8787/api/plugins/reload -H 'X-Karucode-Client: web' -H 'Content-Type: application/json' -d '{}'"
printf '%s\n' "  curl -sS http://127.0.0.1:8787/api/plugins"
