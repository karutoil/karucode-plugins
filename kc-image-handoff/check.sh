#!/bin/sh
# `./check.sh` runs offline checks: fmt, tests, clippy, release build.
# `IMAGE_HANDOFF_LIVE=1 ./check.sh --live` also runs real vision round trips
# through the configured endpoint (needs the live gateway up; see README §4).
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"
case "${1:-}" in
    "") live=0 ;;
    --live) live=1 ;;
    -h|--help) sed -n '2,3p' "$0" | sed 's/^# //'; exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
esac

# Never let rustfmt follow mod sdk/protocol and alter the vendored copies.
rustfmt --edition 2021 --config skip_children=true --check \
    src/main.rs src/config.rs src/media.rs src/server.rs src/flow.rs src/karucode.rs \
    tests/integration.rs
cargo test --offline --all-targets
cargo clippy --offline --all-targets -- -D warnings
cargo build --release --offline
if [ "$live" = 1 ]; then
    cargo test --offline --test integration -- --ignored --test-threads=1
fi
karucode plugin sdk . --check
echo "kc-image-handoff checks passed; live=$live"
