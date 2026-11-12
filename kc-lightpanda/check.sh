#!/bin/sh
# `./check.sh` runs browser-free checks and a release build.
# `LIGHTPANDA_PATH=/path/to/lightpanda ./check.sh --live` also runs all real tools.
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
    src/main.rs src/lib.rs src/browser.rs src/cdp.rs src/snapshot.rs \
    tests/live.rs tests/protocol.rs tests/transport.rs tests/resilience.rs tests/support/mod.rs
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
if [ "$live" = 1 ]; then
    if [ -z "${LIGHTPANDA_PATH:-}" ]; then
        echo "--live needs LIGHTPANDA_PATH=/absolute/path/to/lightpanda" >&2
        exit 2
    fi
    LIGHTPANDA_DISABLE_TELEMETRY=true LIGHTPANDA_DISABLE_CORE_DUMP=1 \
        cargo test --test live -- --ignored --test-threads=1
fi
karucode plugin sdk . --check
echo "kc-lightpanda checks passed; live=$live"
