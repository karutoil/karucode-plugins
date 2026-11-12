#!/bin/sh
# Verify the two halves of the panel still match.
#
# The plugin owns the renderer's *source* (`ui/`), but the app compiles its own
# copy (`web/src/...`): a plugin cannot ship UI, so the files are copied, and a
# copy can drift. This is the same guard kc-todo uses.
#
#   ./check.sh                 # compare against a karucode checkout
#   KARUCODE_DIR=/path ./check.sh
set -eu

PLUGIN_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
KARUCODE_DIR=${KARUCODE_DIR:-/home/karutoil/karucode}
WEB="$KARUCODE_DIR/web/src"

fail=0

compare() {
    src="$PLUGIN_DIR/ui/$1"
    dst="$WEB/$2"
    if [ ! -f "$dst" ]; then
        echo "missing: $dst"
        echo "  copy it: cp $src $dst"
        fail=1
        return
    fi
    if ! diff -q "$src" "$dst" >/dev/null; then
        echo "drifted: $dst"
        echo "  copy it: cp $src $dst"
        diff -u "$src" "$dst" | head -20
        fail=1
    else
        echo "ok: $2"
    fi
}

compare GoalsPanel.tsx components/GoalsPanel.tsx
compare GoalsPanel.test.tsx components/GoalsPanel.test.tsx
compare goals.css styles/goals.css

# The two files the app owns (they are not copies, they are one-line wirings).
for wiring in components/goalsPanelRegister.ts; do
    if [ -f "$WEB/$wiring" ]; then echo "ok: $wiring"; else echo "missing: $wiring"; fail=1; fi
done
if grep -q "goalsPanelRegister" "$WEB/main.tsx" 2>/dev/null; then
    echo "ok: main.tsx imports the register"
else
    echo "missing: main.tsx does not import goalsPanelRegister"
    fail=1
fi
if grep -q "goals.css" "$WEB/styles/index.css" 2>/dev/null; then
    echo "ok: index.css imports goals.css"
else
    echo "missing: index.css does not import goals.css"
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo
    echo "The panel payload and its renderer are one contract; fix the copy above."
    exit 1
fi
echo
echo "Panel files match ($KARUCODE_DIR)."
