#!/bin/sh
# Everything CI should run, in one command.
#
# The ordering and the one exclusion are deliberate:
#
#  * `cargo fmt` cannot be used across the whole crate. `src/protocol.rs` and
#    `src/sdk.rs` are vendored copies of the harness's SDK and must stay
#    byte-identical to what `karucode plugin sdk` emits, or `--check` fails and a
#    fresh checkout cannot be distinguished from a stale one. rustfmt wants to
#    reformat them, so they are formatted by `karucode plugin sdk .` alone and
#    this script checks the SDK *after* formatting our own sources.
#  * The vendored-SDK check runs last so it reports on the final tree.

set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"

karucode=${KARUCODE:-karucode}
if ! command -v "$karucode" >/dev/null 2>&1; then
    # Fall back to a built checkout, which is how this repo is usually used.
    if [ -x /home/karutoil/karucode/target/release/karucode ]; then
        karucode=/home/karutoil/karucode/target/release/karucode
    else
        echo "SKIP: no karucode binary found, cannot check the vendored SDK" >&2
        karucode=
    fi
fi

echo "==> cargo test"
cargo test --release

echo "==> cargo clippy"
cargo clippy --release --all-targets -- -D warnings

echo "==> rustfmt (our sources only)"
# `skip_children=true` is required. Without it rustfmt follows `mod protocol;`
# and `mod sdk;` from main.rs into the vendored copies and reformats them, which
# then fails the SDK check below. The option is a config key, not a flag:
# `--skip-children` does not exist.
rustfmt --check --edition 2021 --config skip_children=true \
    src/main.rs src/model.rs src/render.rs src/session.rs src/config.rs

echo "==> vendored SDK is in sync"
if [ -n "$karucode" ]; then
    "$karucode" plugin sdk . --check
else
    echo "skipped (no karucode binary)"
fi

echo
echo "All checks passed."
