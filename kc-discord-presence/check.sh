#!/bin/sh
# Everything that must pass before this plugin is worth handing over.
#
#   ./check.sh                 # tests, lints, vendored-SDK freshness
#   KARUCODE_DIR=/path ./check.sh
#
# There is no UI copy to compare (this plugin ships no renderer - its panel is a
# `view` payload the app already knows how to draw), so the checks are the Rust
# ones plus the SDK guard.
set -eu

PLUGIN_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
KARUCODE_DIR=${KARUCODE_DIR:-/home/karutoil/karucode}
fail=0

step() {
    echo
    echo "==> $*"
}

step "unit + end-to-end tests"
(cd "$PLUGIN_DIR" && cargo test) || fail=1

step "lints"
(cd "$PLUGIN_DIR" && cargo clippy --all-targets -- -D warnings) || fail=1

step "vendored SDK is the one this karucode ships"
if command -v karucode >/dev/null 2>&1; then
    (cd "$PLUGIN_DIR" && karucode plugin sdk --check .) || fail=1
else
    echo "karucode is not on PATH; skipped"
fi

step "the manifest points at a binary that exists"
BIN=$(sed -n 's/^command = \["\(.*\)"\]/\1/p' "$PLUGIN_DIR/plugin.toml")
if [ -x "$BIN" ]; then
    echo "ok: $BIN"
else
    echo "missing: $BIN (run ./install.sh)"
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo
    echo "Not ready: fix the failures above."
    exit 1
fi
echo
echo "All checks passed."
