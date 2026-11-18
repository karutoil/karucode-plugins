"""Tests for Stage 1 context: token budget, 1-hop symbols, conventions."""

from __future__ import annotations

import pytest

from tests.support.gitfix import make_repo, run
from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import TriageFocus, TriagePlan, sha256_hex
from zero_loop_reviewer.stage1.context import (
    ast_grep_binary,
    TokenCounter,
    build_context,
    critical_tokens,
    extract_symbols,
    load_conventions,
)

BASE_A = "def parse(value):\n    return value\n"
HEAD_A = "def parse(value):\n    return eval(value)\n"
B_FILE = "from pkg.a import parse\n\n\ndef use():\n    return parse(1)\n"
C_FILE = "def helper(value):\n    return value + 1\n"
AGENTS = (
    "# Agent notes\n"
    "\n"
    "- Never log secrets or tokens; redact them at the boundary.\n"
    "- Must run the full test suite before pushing.\n"
    "- Prefer small functions with a single return path.\n"
)
DESIGNLORE = (
    '{"precedents": [{"id": "DP-1", "problem": "token validation duplicated",'
    ' "decision": "validate once at the boundary", "rationale": "single choke point",'
    ' "applies_to": ["pkg/a.py"]}]}'
)
MARKER = "PRDESCRIPTIONMARKER"


def _repo_and_diff(tmp_path, name: str = "ctx"):
    fixture = make_repo(tmp_path, name)
    base = fixture.commit(
        f"base: {MARKER} removes legacy_parse",
        **{
            "pkg__a.py": BASE_A,
            "pkg__b.py": B_FILE,
            "pkg__c.py": C_FILE,
            "AGENTS.md": AGENTS,
            ".zero_loop__designlore.json": DESIGNLORE,
        },
    )
    head = fixture.commit(f"head: {MARKER} removes legacy_parse", **{"pkg__a.py": HEAD_A})
    # Only in the working tree, never committed: must not be loaded.
    fixture.write(".cursorrules", "- must use tabs\n")
    repo = GitRepo(fixture.root)
    return fixture, repo, repo.diff(base, head)


# --------------------------------------------------------------------------- #
# Token estimation and critical tokens
# --------------------------------------------------------------------------- #


def test_token_counter_uses_chars_over_four_with_a_floor_of_one():
    counter = TokenCounter()
    assert counter.count("") == 1
    assert counter.count("a") == 1
    assert counter.count("abcd") == 1
    assert counter.count("a" * 8) == 2
    assert counter.count("a" * 400) == 100


def test_critical_tokens_come_from_added_lines_only(tmp_path):
    _, _, diff = _repo_and_diff(tmp_path)
    tokens = critical_tokens(diff)
    assert "eval" in tokens
    # `return value` was removed, not added, and nothing else is critical.
    assert "set" not in tokens


# --------------------------------------------------------------------------- #
# 1-hop symbols
# --------------------------------------------------------------------------- #


def test_extract_symbols_finds_the_changed_definition(tmp_path):
    _, repo, diff = _repo_and_diff(tmp_path)
    views = extract_symbols(repo, diff, head=diff.head_sha)
    changed = [view for view in views if view.changed]
    assert [view.name for view in changed] == ["parse"]
    assert changed[0].hop == 0
    assert changed[0].path == "pkg/a.py"
    assert changed[0].line == 1
    assert changed[0].signature.startswith("def parse(value)")


def test_extract_symbols_finds_a_caller_one_hop_away(tmp_path):
    _, repo, diff = _repo_and_diff(tmp_path)
    views = extract_symbols(repo, diff, head=diff.head_sha)
    callers = [view for view in views if view.name == "use"]
    assert callers, views
    caller = callers[0]
    assert caller.hop == 1
    assert caller.changed is False
    assert caller.path == "pkg/b.py"
    assert caller.line == 4
    assert "parse" in caller.signature


@pytest.mark.integration
@pytest.mark.skipif(not ast_grep_binary(), reason="ast-grep CLI is not installed")
def test_extract_symbols_finds_a_non_python_caller_via_ast_grep(tmp_path):
    fixture = make_repo(tmp_path, "golang")
    base = fixture.commit(
        "base",
        **{
            "main.go": "package main\n\nfunc parse(x int) int {\n\treturn x\n}\n",
            "b.go": "package main\n\nfunc use() int {\n\treturn parse(1)\n}\n",
        },
    )
    head = fixture.commit(
        "head", **{"main.go": "package main\n\nfunc parse(x int32) int {\n\treturn x\n}\n"}
    )
    repo = GitRepo(fixture.root)
    diff = repo.diff(base, head)
    views = extract_symbols(repo, diff, head=diff.head_sha)
    changed = [view for view in views if view.changed]
    assert [view.name for view in changed] == ["parse"]
    assert changed[0].path == "main.go"
    callers = [view for view in views if view.name == "use"]
    assert callers, views
    assert callers[0].hop == 1
    assert callers[0].path == "b.go"
    assert callers[0].changed is False


def test_extract_symbols_reports_an_unresolved_callee(tmp_path):
    fixture = make_repo(tmp_path, "unresolved")
    fixture.commit("base", **{"m.py": "def f():\n    return 1\n"})
    fixture.commit("head", **{"m.py": "def f():\n    return nowhere_function(1)\n"})
    repo = GitRepo(fixture.root)
    diff = repo.diff("HEAD~1", "HEAD")
    views = extract_symbols(repo, diff, head=diff.head_sha)
    unresolved = [view for view in views if view.unresolved]
    assert [view.name for view in unresolved] == ["nowhere_function"]
    assert unresolved[0].hop == 1
    slice_ = build_context(repo, diff, Settings())
    assert "nowhere_function" in slice_.unresolved_symbols


def test_extract_symbols_reports_an_unresolved_project_import(tmp_path):
    fixture = make_repo(tmp_path, "badimport")
    fixture.commit("base", **{"pkg__a.py": "import json\n\n\ndef parse(v):\n    return v\n"})
    fixture.commit(
        "head",
        **{"pkg__a.py": "import json\nfrom pkg.missing import helper\n\n\ndef parse(v):\n    return v + 1\n"},
    )
    repo = GitRepo(fixture.root)
    diff = repo.diff("HEAD~1", "HEAD")
    views = extract_symbols(repo, diff, head=diff.head_sha)
    unresolved = {view.name for view in views if view.unresolved}
    assert "pkg.missing" in unresolved
    assert "json" not in unresolved, "stdlib imports are not the repository's symbols"
    slice_ = build_context(repo, diff, Settings())
    assert "pkg.missing" in slice_.unresolved_symbols


# --------------------------------------------------------------------------- #
# Conventions and precedents
# --------------------------------------------------------------------------- #


def test_load_conventions_reads_the_head_commit_only(tmp_path):
    _, repo, diff = _repo_and_diff(tmp_path)
    directives, precedents, loaded = load_conventions(repo, diff.head_sha, Settings())
    assert loaded == ["AGENTS.md", ".zero_loop/designlore.json"]
    assert ".cursorrules" not in loaded
    kinds = {directive.kind for directive in directives if directive.source == "AGENTS.md"}
    assert kinds == {"never", "must", "prefer"}
    assert any("Never log secrets" in directive.text for directive in directives)
    assert [precedent.id for precedent in precedents] == ["DP-1"]
    assert precedents[0].decision == "validate once at the boundary"
    assert precedents[0].source == ".zero_loop/designlore.json"


def test_load_conventions_reads_pr_agent_toml(tmp_path):
    fixture = make_repo(tmp_path, "pragent")
    fixture.commit(
        "base",
        **{
            ".pr_agent.toml": (
                "# review\n[pr_reviewer]\nextra_instructions = \"\"\"\n"
                "- Prefer dataclasses over dicts.\nMust keep public APIs stable.\n\"\"\"\n"
            ),
            "m.py": "x = 1\n",
        },
    )
    repo = GitRepo(fixture.root)
    directives, _, loaded = load_conventions(repo, repo.head_sha(), Settings())
    assert loaded == [".pr_agent.toml"]
    kinds = {directive.text: directive.kind for directive in directives}
    assert kinds["Prefer dataclasses over dicts."] == "prefer"
    assert kinds["Must keep public APIs stable."] == "must"


def test_load_conventions_survives_a_malformed_designlore(tmp_path):
    fixture = make_repo(tmp_path, "baddesign")
    fixture.commit("base", **{".zero_loop__designlore.json": "{not json", "m.py": "x = 1\n"})
    repo = GitRepo(fixture.root)
    directives, precedents, loaded = load_conventions(repo, repo.head_sha(), Settings())
    assert precedents == []
    assert directives == []
    assert loaded == [".zero_loop/designlore.json"]


# --------------------------------------------------------------------------- #
# Context assembly and the token budget
# --------------------------------------------------------------------------- #


def test_build_context_never_includes_the_pull_request_description(tmp_path):
    _, repo, diff = _repo_and_diff(tmp_path)
    assert MARKER in (
        repo.show(diff.head_sha, "AGENTS.md") + _commit_message(repo, diff.head_sha)
    ), "fixture must actually carry the marker somewhere the builder could read"

    slice_ = build_context(repo, diff, Settings())
    assert MARKER not in slice_.text
    assert "legacy_parse" not in slice_.text, "commit-message prose leaked into the context"
    assert "eval(value)" in slice_.text, "the diff itself must be present"
    assert slice_.input_sha256 == sha256_hex(slice_.text)
    assert slice_.input_bytes == len(slice_.text.encode("utf-8"))


def _commit_message(repo: GitRepo, sha: str) -> str:
    return run(repo.root, "log", "-1", "--format=%B", sha)


def test_budget_40_keeps_the_diff_and_drops_whole_signature_units(tmp_path):
    _, repo, diff = _repo_and_diff(tmp_path)
    slice_ = build_context(repo, diff, Settings(max_model_tokens=40))

    assert slice_.budget == 40
    assert slice_.tokens <= 40
    assert [unit.kind for unit in slice_.units] == ["diff"]
    assert slice_.units[0].source == "pkg/a.py"
    assert all(unit.kind != "signature" for unit in slice_.units)
    assert slice_.dropped_units > 0
    assert slice_.truncated is True
    assert slice_.input_sha256 == sha256_hex(slice_.text)
    assert slice_.tokens == TokenCounter().count(slice_.text)


def test_build_context_with_room_keeps_conventions_signatures_and_precedents(tmp_path):
    _, repo, diff = _repo_and_diff(tmp_path)
    slice_ = build_context(repo, diff, Settings())
    kinds = {unit.kind for unit in slice_.units}
    assert {"diff", "critical_token", "convention", "signature", "precedent"} <= kinds
    assert slice_.tokens <= slice_.budget
    assert slice_.truncated is False
    assert slice_.dropped_units == 0
    assert slice_.conventions_loaded == ["AGENTS.md", ".zero_loop/designlore.json"]
    assert slice_.precedents_loaded == [".zero_loop/designlore.json"]
    assert "NEVER: Never log secrets" in slice_.text
    # Units are sorted by priority, highest first.
    priorities = [unit.priority for unit in slice_.units]
    assert priorities == sorted(priorities, reverse=True)


def test_build_context_honours_the_triage_plan(tmp_path):
    _, repo, diff = _repo_and_diff(tmp_path)
    plan = TriagePlan(
        focus=[TriageFocus(path="pkg/a.py", function="parse", reason="changed", priority=3)],
        search_symbols=["helper"],
    )
    slice_ = build_context(repo, diff, Settings(), plan=plan)
    assert slice_.units[0].kind == "diff"
    assert slice_.units[0].source == "pkg/a.py"
    assert slice_.units[0].priority == 100, "a focus path must outrank other diff units"
    planned = [unit for unit in slice_.units if "requested by triage" in unit.reason]
    assert [unit.source for unit in planned] == ["pkg/c.py"]
    assert "def helper(value)" in planned[0].text


def test_build_context_is_deterministic(tmp_path):
    _, repo, diff = _repo_and_diff(tmp_path)
    first = build_context(repo, diff, Settings())
    second = build_context(repo, diff, Settings())
    assert first.text == second.text
    assert first.input_sha256 == second.input_sha256
    assert [(u.kind, u.source, u.priority) for u in first.units] == [
        (u.kind, u.source, u.priority) for u in second.units
    ]


def test_context_includes_the_full_enclosing_function(tmp_path) -> None:
    _, repo, diff = _repo_and_diff(tmp_path, "ctx_function")
    context = build_context(repo, diff, Settings(max_model_tokens=100_000))
    assert "FUNCTION pkg/a.py:1-2" in context.text
    assert "def parse(value):" in context.text
    assert "return eval(value)" in context.text
