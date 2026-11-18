"""Stage 2B host side: CPG frame projection, the shipped policy and track mapping."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from zero_loop_reviewer.errors import ToolError
from zero_loop_reviewer.models import AnalyzerFinding
from zero_loop_reviewer.stage2.cpg import frame_projection, parse_policy, taint_and_lifetime

RULES_DIR = Path(__file__).resolve().parents[1] / "rules"
PY_TAINT = '''import subprocess


def handler(request):
    user_id = request.args.get("id")
    alias = user_id
    cursor = db.cursor()
    cursor.execute("SELECT * FROM t WHERE id = " + alias)
    cmd = input()
    subprocess.run(cmd, shell=True)
    return 1
'''
PY_LIFETIME = '''def leak(path):
    fh = open(path)
    return fh.read()


def closed(path):
    fh = open(path)
    try:
        return fh.read()
    finally:
        fh.close()
'''
C_SOURCE = r'''
int main(void) {
  char *buf = malloc(16);
  memcpy(dst, buf, 16);
  free(buf);
  buf->len = 1;
}
/* \separated(a, b) */
/* assigns out \from in */
'''


# --------------------------------------------------------------------------- #
# Policy
# --------------------------------------------------------------------------- #


def test_shipped_policy_parses_and_uses_only_real_tracks():
    policy = parse_policy(RULES_DIR / "policy.json")
    rules = policy["rules"]
    assert rules, "the shipped policy must map at least one rule"
    assert [rule["id"] for rule in rules] == sorted(rule["id"] for rule in rules)
    for rule in rules:
        assert rule["track"] in ("taint", "lifetime")
        assert 0 <= rule["severity"] <= 3
        assert rule["cwe"].startswith("CWE-")
    ids = {rule["id"] for rule in rules}
    assert "zero-loop.sql-string-concat" in ids
    assert "zero-loop.open-without-context" in ids


def test_parse_policy_defaults_and_rejects_bad_input(tmp_path):
    good = tmp_path / "good.json"
    good.write_text(json.dumps({"rules": [{"id": "a.b", "track": "lifetime"}]}), encoding="utf-8")
    policy = parse_policy(good)
    assert policy["rules"] == [{"id": "a.b", "track": "lifetime", "cwe": "", "severity": 1}]

    bad_track = tmp_path / "bad-track.json"
    bad_track.write_text(json.dumps({"rules": [{"id": "a.b", "track": "pattern"}]}), encoding="utf-8")
    with pytest.raises(ToolError):
        parse_policy(bad_track)

    duplicate = tmp_path / "duplicate.json"
    duplicate.write_text(
        json.dumps({"rules": [{"id": "a.b", "track": "taint"}, {"id": "a.b", "track": "lifetime"}]}),
        encoding="utf-8",
    )
    with pytest.raises(ToolError):
        parse_policy(duplicate)

    missing = tmp_path / "nope.json"
    with pytest.raises(ToolError):
        parse_policy(missing)

    not_json = tmp_path / "broken.json"
    not_json.write_text("{not json", encoding="utf-8")
    with pytest.raises(ToolError):
        parse_policy(not_json)


# --------------------------------------------------------------------------- #
# Frame projection
# --------------------------------------------------------------------------- #


def test_python_frame_projects_taint_aliases_and_write_sets():
    frame = frame_projection("app.py", PY_TAINT, "python")
    assert frame.language == "python"
    assert frame.function == "handler"
    assert frame.memory_roots == ["input", "request.args.get"]
    assert ("user_id", "alias") in frame.alias_pairs
    assert {"user_id", "alias", "cmd", "cursor"} <= set(frame.write_sets)

    sources = {(item.source.line, item.source.symbol) for item in frame.taint}
    sinks = {(item.sink.line, item.sink.symbol) for item in frame.taint}
    assert (5, "request.args.get") in sources
    assert (8, "cursor.execute") in sinks, sinks
    assert (9, "input") in sources
    assert (10, "subprocess.run") in sinks, sinks
    assert all(item.source.path == "app.py" and item.sink.path == "app.py" for item in frame.taint)
    assert [item.kind for item in frame.obligations] == ["taint"]
    assert frame.obligations[0].subjects == ["input", "request.args.get"]


def test_frame_projection_is_deterministic():
    first = frame_projection("app.py", PY_TAINT, "python")
    second = frame_projection("app.py", PY_TAINT, "python")
    assert first.model_dump() == second.model_dump()


def test_python_frame_reports_lifetime_with_and_without_close():
    frame = frame_projection("io.py", PY_LIFETIME, "python")
    assert len(frame.lifetime) == 2
    leak, closed = sorted(frame.lifetime, key=lambda item: item.alloc.line)
    assert leak.alloc is not None and leak.alloc.line == 2
    assert leak.free is None
    assert closed.alloc is not None and closed.alloc.line == 7
    assert closed.free is not None and closed.free.line == 11


def test_python_frame_ignores_context_manager_lifetime():
    source = 'def read(path):\n    with open(path) as fh:\n        return fh.read()\n'
    frame = frame_projection("io.py", source, "python")
    assert [item.rule_id for item in frame.lifetime] == ["cpg.context-manager"]
    assert frame.lifetime[0].free is not None


def test_c_frame_projects_heap_lifetime_and_lsog_obligations():
    frame = frame_projection("a.c", C_SOURCE, "c")
    assert frame.language == "c"
    assert frame.function == "main"
    assert frame.memory_roots == ["buf"]
    assert ("buf", "dst") in frame.alias_pairs
    assert "buf->len" in frame.write_sets
    assert len(frame.lifetime) == 1
    life = frame.lifetime[0]
    assert life.alloc is not None and life.alloc.line == 3
    assert life.free is not None and life.free.line == 5
    assert life.use is not None and life.use.line == 6
    kinds = [item.kind for item in frame.obligations]
    assert kinds == ["separated", "assigns_from"]
    assert frame.obligations[0].subjects == ["a", "b"]


def test_unknown_language_projects_nothing():
    frame = frame_projection("x.rs", "fn main() {}", "")
    assert frame.path == "x.rs"
    assert frame.taint == [] and frame.lifetime == [] and frame.memory_roots == []
    assert frame.language == ""


def test_syntax_error_projects_an_empty_python_frame():
    frame = frame_projection("broken.py", "def f(:\n", "python")
    assert frame.language == "python"
    assert frame.taint == [] and frame.lifetime == []


# --------------------------------------------------------------------------- #
# Track mapping
# --------------------------------------------------------------------------- #


def test_taint_and_lifetime_follow_the_policy_track():
    policy = {
        "rules": [
            {"id": "zero-loop.sql-string-concat", "track": "taint", "cwe": "CWE-89", "severity": 3},
            {"id": "zero-loop.open-without-context", "track": "lifetime", "cwe": "CWE-772", "severity": 1},
        ]
    }
    analyzer = [
        AnalyzerFinding(analyzer="semgrep", rule_id="zero-loop.sql-string-concat", path="a.py", line=10, cwe="CWE-89"),
        AnalyzerFinding(analyzer="semgrep", rule_id="not.in.policy", path="a.py", line=3),
        AnalyzerFinding(
            analyzer="semgrep",
            rule_id="zero-loop.open-without-context",
            path="a.py",
            line=20,
            end_line=20,
            cwe="CWE-772",
        ),
    ]
    taint, lifetime = taint_and_lifetime(analyzer, policy)
    assert [item.sink.line for item in taint] == [10]
    assert taint[0].source.line == 10
    assert taint[0].rule_id == "zero-loop.sql-string-concat"
    assert len(lifetime) == 1
    assert lifetime[0].alloc is not None and lifetime[0].alloc.line == 20
    assert lifetime[0].free is None and lifetime[0].use is None


def test_taint_and_lifetime_matches_by_cwe_and_is_ordered():
    policy = {"rules": [{"id": "zero-loop.eval-exec", "track": "taint", "cwe": "CWE-95", "severity": 3}]}
    analyzer = [
        AnalyzerFinding(analyzer="custom", rule_id="custom.code-eval", path="b.py", line=4, cwe="CWE-95: Eval"),
        AnalyzerFinding(analyzer="custom", rule_id="custom.code-eval", path="a.py", line=9, cwe="CWE-95"),
        AnalyzerFinding(analyzer="custom", rule_id="custom.code-eval", path="a.py", line=2, cwe="CWE-95"),
    ]
    taint, lifetime = taint_and_lifetime(analyzer, policy)
    assert [(item.sink.path, item.sink.line) for item in taint] == [("a.py", 2), ("a.py", 9), ("b.py", 4)]
    assert lifetime == []


def test_taint_and_lifetime_tolerates_an_empty_policy():
    analyzer = [AnalyzerFinding(analyzer="semgrep", rule_id="zero-loop.eval-exec", path="a.py", line=1)]
    assert taint_and_lifetime(analyzer, {"rules": []}) == ([], [])
    assert taint_and_lifetime(analyzer, {}) == ([], [])
