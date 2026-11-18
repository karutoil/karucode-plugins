"""Minimal probe runner for sandbox images without pytest.

The generated probe is a normal Python file with ``test_*`` functions and plain
``assert`` statements.  This runner imports it by path, calls every test
function, and exits with a pytest-compatible code:

* 0 — every test passed;
* 1 — a test failed (assertion or an exception in project code);
* 2 — the probe could not be imported (setup error);
* 5 — the module exposed no test functions.

It is copied into the scratch tree by the sandbox and is only used when the
image has no pytest.  It never imports project files for its own sake, and it
does not catch ``KeyboardInterrupt``/``SystemExit``.
"""

from __future__ import annotations

import importlib.util
import sys
import traceback
from pathlib import Path


def _load(path: Path):
    spec = importlib.util.spec_from_file_location("zero_loop_probe", path)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load probe from {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["zero_loop_probe"] = module
    spec.loader.exec_module(module)
    return module


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: probe_runner.py <probe.py>", file=sys.stderr)
        return 2
    path = Path(argv[1])
    if not path.is_file():
        print(f"probe not found: {path}", file=sys.stderr)
        return 2
    try:
        module = _load(path)
    except BaseException:
        traceback.print_exc()
        return 2
    tests = [
        (name, value)
        for name, value in vars(module).items()
        if name.startswith("test") and callable(value)
    ]
    if not tests:
        # A probe written as a script (assertions at module level) has already
        # run during import; reaching here with no test functions means there
        # was nothing else to execute.
        print("no test functions were collected", file=sys.stderr)
        return 0
    failures = 0
    for name, test in tests:
        try:
            test()
            print(f"PASSED {name}")
        except AssertionError:
            failures += 1
            print(f"FAILED {name}", file=sys.stderr)
            traceback.print_exc()
        except Exception:
            failures += 1
            print(f"ERROR {name}", file=sys.stderr)
            traceback.print_exc()
    if failures:
        print(f"{failures} failed, {len(tests) - failures} passed", file=sys.stderr)
        return 1
    print(f"{len(tests)} passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
