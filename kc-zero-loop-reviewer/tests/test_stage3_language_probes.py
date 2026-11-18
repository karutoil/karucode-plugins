"""Real local probes for Rust and TypeScript/JavaScript.

These are skipped when the host has no toolchain; they prove the language
dispatch, not a container image.  The container path is covered by the pure
script/argv tests.
"""

from __future__ import annotations

import shutil
from pathlib import Path

import pytest

from zero_loop_reviewer.models import ProbeSpec, ProbeStatus, sha256_hex
from zero_loop_reviewer.stage3.sandbox import LocalSandbox, SandboxConfigView


def _view(tmp_path: Path) -> SandboxConfigView:
    return SandboxConfigView(runtime="local", work_dir=tmp_path / "work", visible_roots=())


@pytest.mark.skipif(shutil.which("cargo") is None, reason="cargo is not installed")
def test_a_local_rust_probe_reaches_cargo_and_fails_behaviourally(tmp_path) -> None:
    tree = tmp_path / "rust"
    (tree / "src").mkdir(parents=True)
    (tree / "tests").mkdir()
    (tree / "Cargo.toml").write_text(
        '[package]\nname = "demo"\nversion = "0.1.0"\nedition = "2021"\n', encoding="utf-8"
    )
    (tree / "src/lib.rs").write_text(
        "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n", encoding="utf-8"
    )
    content = (
        "use demo::add;\n\n"
        "#[test]\n"
        "fn empty_and_zero() {\n"
        "    let empty: Vec<i32> = vec![];\n"
        "    assert!(empty.is_empty());\n"
        "    assert_eq!(add(2, 3), 5);\n"
        "    assert_eq!(add(0, 0), 0);\n"
        "}\n"
    )
    spec = ProbeSpec(test_path="tests/zl_probe.rs", content=content, framework="cargo")
    sandbox = LocalSandbox(_view(tmp_path))
    result = sandbox.run_probe(
        tree, spec, expected_sha=sha256_hex(content.encode("utf-8")), timeout_s=120
    )
    assert result.status is ProbeStatus.FAIL_BEHAVIORAL, result.stderr[-2000:]
    assert result.exit_code not in (None, 0)


@pytest.mark.skipif(shutil.which("node") is None, reason="node is not installed")
def test_a_local_javascript_probe_reaches_node_and_fails_behaviourally(tmp_path) -> None:
    tree = tmp_path / "web"
    (tree / "src").mkdir(parents=True)
    (tree / "src/lib.mjs").write_text(
        "export function add(a, b) {\n  return a - b;\n}\n", encoding="utf-8"
    )
    content = (
        "import test from 'node:test';\n"
        "import assert from 'node:assert';\n"
        "import { add } from './lib.mjs';\n"
        "test('empty and zero', () => {\n"
        "  assert.deepEqual([], []);\n"
        "  assert.equal(add(2, 3), 5);\n"
        "});\n"
    )
    (tree / "src/probe.test.mjs").write_text(content, encoding="utf-8")
    spec = ProbeSpec(test_path="src/probe.test.mjs", content=content, framework="node")
    sandbox = LocalSandbox(_view(tmp_path))
    result = sandbox.run_probe(
        tree, spec, expected_sha=sha256_hex(content.encode("utf-8")), timeout_s=60
    )
    assert result.status is ProbeStatus.FAIL_BEHAVIORAL, result.stderr[-2000:]
    assert result.exit_code not in (None, 0)


@pytest.mark.skipif(shutil.which("node") is None, reason="node is not installed")
def test_a_local_typescript_probe_uses_node_strip_types(tmp_path) -> None:
    tree = tmp_path / "webts"
    (tree / "src").mkdir(parents=True)
    (tree / "src/lib.ts").write_text(
        "export function add(a: number, b: number): number {\n  return a - b;\n}\n", encoding="utf-8"
    )
    content = (
        "import test from 'node:test';\n"
        "import assert from 'node:assert';\n"
        "import { add } from './lib.ts';\n"
        "test('zero', () => {\n"
        "  assert.deepEqual([], []);\n"
        "  assert.equal(add(2, 3), 5);\n"
        "});\n"
    )
    (tree / "src/probe.test.ts").write_text(content, encoding="utf-8")
    spec = ProbeSpec(test_path="src/probe.test.ts", content=content, framework="node")
    sandbox = LocalSandbox(_view(tmp_path))
    result = sandbox.run_probe(
        tree, spec, expected_sha=sha256_hex(content.encode("utf-8")), timeout_s=60
    )
    assert result.status is ProbeStatus.FAIL_BEHAVIORAL, result.stderr[-2000:]


@pytest.mark.skipif(shutil.which("go") is None, reason="go is not installed")
def test_a_local_go_probe_reaches_go_test_and_fails_behaviourally(tmp_path) -> None:
    tree = tmp_path / "goproj"
    (tree / "calc").mkdir(parents=True)
    (tree / "go.mod").write_text("module example.com/goproj\n\ngo 1.23\n", encoding="utf-8")
    (tree / "calc/calc.go").write_text(
        "package calc\n\nfunc Add(a, b int) int {\n\treturn a - b\n}\n", encoding="utf-8"
    )
    content = (
        "package calc\n\n"
        "import \"testing\"\n\n"
        "func TestZeroAndTwo(t *testing.T) {\n"
        "\tempty := []int{}\n"
        "\tif len(empty) != 0 {\n"
        "\t\tt.Fatalf(\"bad empty\")\n"
        "\t}\n"
        "\tif Add(2, 3) != 5 {\n"
        "\t\tt.Fatalf(\"Add(2,3) = %d, want 5\", Add(2, 3))\n"
        "\t}\n"
        "}\n"
    )
    (tree / "calc/calc_test.go").write_text(content, encoding="utf-8")
    spec = ProbeSpec(test_path="calc/calc_test.go", content=content, framework="go")
    sandbox = LocalSandbox(_view(tmp_path))
    result = sandbox.run_probe(
        tree, spec, expected_sha=sha256_hex(content.encode("utf-8")), timeout_s=60
    )
    assert result.status is ProbeStatus.FAIL_BEHAVIORAL, result.stderr[-2000:]


def test_a_custom_adapter_runs_a_shell_probe_without_builtin_language_code(tmp_path) -> None:
    import json

    from zero_loop_reviewer.adapters import adapters_map, parse_adapters
    from zero_loop_reviewer.config import load_settings
    from zero_loop_reviewer.stage3.probe import inspect_probe

    raw = json.dumps(
        {
            "shell": {
                "extensions": [".sh"],
                "test_path": "probe/run.sh",
                "framework": "shell",
                "command": ["sh", "{test_path}"],
                "test_marker": "exit",
                "assertion_marker": "exit 1",
            }
        }
    )
    adapters = parse_adapters(raw)
    settings = load_settings({"probe.adapters": raw}, {})
    adapter = adapters_map(settings.probe_adapters)["shell"]
    content = "#!/bin/sh\nif [ 0 -eq 0 ]; then echo failed; exit 1; fi\n"
    spec = ProbeSpec(test_path="probe/run.sh", content=content, framework="shell")
    assert inspect_probe(spec, adapter=adapter) == []
    tree = tmp_path / "shellrepo"
    (tree / "probe").mkdir(parents=True)
    (tree / "probe/run.sh").write_text(content, encoding="utf-8")
    view = SandboxConfigView(
        runtime="local",
        work_dir=tmp_path / "work",
        visible_roots=(),
        adapters=adapters_map(adapters),
    )
    result = LocalSandbox(view).run_probe(
        tree, spec, expected_sha=sha256_hex(content.encode("utf-8")), timeout_s=30
    )
    assert result.status is ProbeStatus.FAIL_BEHAVIORAL, result.stderr[-2000:]
