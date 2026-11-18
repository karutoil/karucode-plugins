#!/bin/sh
# Everything CI should run for this plugin: byte-compile, the manifest smoke
# test and the full test suite (real git/semgrep/podman where available).
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"
export PYTHONPATH="$here${PYTHONPATH:+:$PYTHONPATH}"

python3 -m compileall -q zero_loop_reviewer rules eval
python3 -m zero_loop_reviewer --version
python3 -m pytest tests -q "$@"
