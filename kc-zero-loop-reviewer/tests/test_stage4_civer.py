"""Stage 4A: CIVER repairs a real off-by-one, and stores nothing it cannot verify."""

from __future__ import annotations

import hashlib
import subprocess
import sys
from pathlib import Path

import pytest

from tests.support.gitfix import make_repo, run
from tests.support.llm import ScriptedLLM
from zero_loop_reviewer.config import Settings, load_settings
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import (
    Candidate,
    Category,
    CitedSite,
    ContextSlice,
    Evidence,
    PatchProposal,
    StageId,
)
from zero_loop_reviewer.stage4.civer import LocalCommandRunner, propose_fix, verify_fix

CORRECT = '''"""Tiny helpers."""


def last(items):
    """Return the final element, or 0 for an empty list."""
    if not items:
        return 0
    return items[-1]


def total(values):
    return sum(values)
'''

#: The head commit reads one element past the end: a real off-by-one.
BUGGY = CORRECT.replace("return items[-1]", "return items[len(items)]")
BUG_LINE = 8
NOOP = BUGGY.replace("    return sum(values)", "    return sum(values) ")


@pytest.fixture()
def checkout(tmp_path):
    fixture = make_repo(tmp_path / "checkout")
    fixture.commit("base: last() reads the final item", **{"calc.py": CORRECT})
    fixture.commit("head: last() reads one past the end", **{"calc.py": BUGGY})
    return fixture


def _diff(fixture, path: str, text: str) -> str:
    """A real unified diff produced by git itself."""
    target = fixture.root / path
    original = target.read_text(encoding="utf-8")
    try:
        target.write_text(text, encoding="utf-8")
        patch = run(fixture.root, "diff", "--", path)
    finally:
        target.write_text(original, encoding="utf-8")
    assert patch.startswith("diff --git"), patch
    return patch + "\n"


@pytest.fixture()
def repair_patch(checkout) -> str:
    return _diff(checkout, "calc.py", CORRECT)


@pytest.fixture()
def noop_patch(checkout) -> str:
    return _diff(checkout, "calc.py", NOOP)


def _defect_probe(tree: Path) -> bool:
    """True while ``calc.last([1, 2, 3])`` still raises (a real run, not a string match)."""
    code = f"import sys; sys.path.insert(0, r'{Path(tree)}'); import calc; calc.last([1, 2, 3])"
    proc = subprocess.run(
        [sys.executable, "-c", code],
        cwd=str(tree),
        capture_output=True,
        text=True,
        timeout=60,
    )
    return proc.returncode != 0


def _candidate() -> Candidate:
    return Candidate(
        id="cand-civer-off-by-one",
        stage=StageId.FALSIFY,
        category=Category.LOGIC,
        title="last() indexes one element past the end of the list",
        root_cause="items[len(items)] is out of range for every non-empty list, so last() raises IndexError",
        severity=2,
        cited_sites=[CitedSite(path="calc.py", line=BUG_LINE, symbol="last", snippet="return items[len(items)]")],
        evidence=Evidence(kind="analyzer", analyzer="stage2:rale", rule_id="off-by-one"),
    )


def _context() -> ContextSlice:
    return ContextSlice(text="[diff:calc.py]\n@@ -1,12 +1,12 @@", tokens=20, budget=4000)


def _proposal(patch: str) -> PatchProposal:
    return PatchProposal(patch=patch, files_changed=["calc.py"], rationale="restore the final-index read")


def _verify(checkout, tmp_path, patch: str, **kwargs):
    settings = Settings().with_paths(tmp_path / "state")
    return verify_fix(
        _candidate(),
        _proposal(patch),
        GitRepo(checkout.root),
        checkout.head(),
        tmp_path / "work",
        LocalCommandRunner(),
        settings,
        **kwargs,
    )


# --------------------------------------------------------------------------- #
# Proposal
# --------------------------------------------------------------------------- #


def test_propose_fix_returns_the_validated_proposal_and_uses_the_civer_prompt():
    llm = ScriptedLLM(answers={"civer": [{"patch": "--- a/x.py\n+++ b/x.py\n", "files_changed": ["x.py"]}]})
    proposal = propose_fix(_candidate(), _context(), llm)
    assert isinstance(proposal, PatchProposal)
    assert proposal.files_changed == ["x.py"]
    call = llm.calls[0]
    assert call["stage"] == "civer"
    assert "SCHEMA: PatchProposal" in call["prompt"].user
    assert "last() indexes one element past the end" in call["prompt"].user


def test_propose_fix_returns_none_when_the_answer_does_not_fit_the_schema():
    llm = ScriptedLLM(answers={"civer": [{"patch": ""}]})
    assert propose_fix(_candidate(), _context(), llm) is None


# --------------------------------------------------------------------------- #
# Verification — the real repair
# --------------------------------------------------------------------------- #


def test_verify_fix_accepts_a_real_patch_that_removes_a_real_defect(checkout, tmp_path, repair_patch):
    result = _verify(checkout, tmp_path, repair_patch, defect_probe=_defect_probe)

    assert result.passed is True, result.detail
    assert result.semantic_noop_signal is False
    fix = result.fix
    assert fix is not None
    assert fix.verified and fix.compile_ok and fix.defect_removed and fix.noop_outperformed
    assert fix.applies_to_sha == checkout.head()
    assert fix.files_changed == ["calc.py"]
    assert any("compileall" in check for check in fix.checks)
    assert any(check.startswith("defect_removed") for check in fix.checks)
    assert any(check.startswith("semantic_noop") for check in fix.checks)
    # The compile ran in a scratch copy, never in the review checkout.
    assert not (checkout.root / "__pycache__").exists()
    assert (checkout.root / "calc.py").read_text(encoding="utf-8") == BUGGY
    # And the scratch area is cleaned up.
    assert not list((tmp_path / "work").glob("civer-head-*"))


def test_verify_fix_uses_the_analyzer_rerun_when_no_probe_is_available(checkout, tmp_path, repair_patch):
    def analyzer_rerun(tree: Path) -> bool:
        return "items[len(items)]" in (Path(tree) / "calc.py").read_text(encoding="utf-8")

    result = _verify(checkout, tmp_path, repair_patch, defect_probe=None, analyzer_rerun=analyzer_rerun)

    assert result.passed is True, result.detail
    assert result.fix is not None and result.fix.defect_removed


# --------------------------------------------------------------------------- #
# Verification — the refusals
# --------------------------------------------------------------------------- #


def test_verify_fix_refuses_a_whitespace_only_patch(checkout, tmp_path, noop_patch):
    result = _verify(checkout, tmp_path, noop_patch, defect_probe=_defect_probe)

    assert result.passed is False
    assert result.fix is None
    assert "defect_removed" in result.detail


def test_verify_fix_refuses_when_a_noop_also_removes_the_signal(checkout, tmp_path, repair_patch):
    buggy_sha = hashlib.sha256(BUGGY.encode("utf-8")).hexdigest()

    def content_probe(tree: Path) -> bool:
        data = (Path(tree) / "calc.py").read_bytes()
        return hashlib.sha256(data).hexdigest() == buggy_sha

    result = _verify(checkout, tmp_path, repair_patch, defect_probe=content_probe)

    assert result.passed is False
    assert result.fix is None
    assert result.semantic_noop_signal is True
    assert "semantic_noop" in result.detail


def test_verify_fix_refuses_without_any_defect_signal_callback(checkout, tmp_path, repair_patch):
    result = _verify(checkout, tmp_path, repair_patch)

    assert result.passed is False
    assert result.fix is None
    assert "defect_removal_unverified" in result.detail


def test_verify_fix_refuses_a_patch_that_does_not_apply(checkout, tmp_path):
    patch = "--- a/calc.py\n+++ b/calc.py\n@@ -1,1 +1,1 @@\n-not the real content\n+something else\n"
    result = _verify(checkout, tmp_path, patch, defect_probe=_defect_probe)

    assert result.passed is False
    assert result.fix is None
    assert "patch_rejected" in result.detail


def test_verify_fix_refuses_a_patch_that_escapes_the_worktree(checkout, tmp_path):
    patch = "--- a/../escape.py\n+++ b/../escape.py\n@@ -1,1 +1,1 @@\n-a\n+b\n"
    result = _verify(checkout, tmp_path, patch, defect_probe=_defect_probe)

    assert result.passed is False
    assert "outside the worktree" in result.detail


def test_verify_fix_refuses_a_patch_that_does_not_compile(checkout, tmp_path):
    broken = "def last(items):\n    return items[len(items) - 1]\n\n\ndef total(values:\n"
    patch = _diff(checkout, "calc.py", broken)

    result = _verify(checkout, tmp_path, patch, defect_probe=_defect_probe)

    assert result.passed is False
    assert result.fix is None
    assert "compile_failed" in result.detail


def test_verify_fix_skips_an_uninstalled_linter_but_refuses_a_failing_one(checkout, tmp_path, repair_patch):
    skipped = _verify(
        checkout,
        tmp_path,
        repair_patch,
        defect_probe=_defect_probe,
        linter=["definitely-not-a-real-linter"],
    )
    assert skipped.passed is True, skipped.detail
    assert skipped.fix is not None and skipped.fix.lint_ok is False
    assert any(check.startswith("linter_skipped") for check in skipped.fix.checks)

    failed = _verify(
        checkout,
        tmp_path,
        repair_patch,
        defect_probe=_defect_probe,
        linter=["python3", "-c", "raise SystemExit(3)"],
    )
    assert failed.passed is False
    assert failed.fix is None
    assert "lint_failed" in failed.detail


def test_syntax_commands_cover_python_and_rust(tmp_path) -> None:
    from zero_loop_reviewer.stage4.civer import _syntax_commands

    (tmp_path / "a.py").write_text("x = 1\n", encoding="utf-8")
    (tmp_path / "a.rs").write_text("pub fn f() {}\n", encoding="utf-8")
    commands, unavailable = _syntax_commands(["a.py", "a.rs"], tmp_path, None)
    rendered = " ".join(" ".join(argv) for _label, argv in commands)
    assert unavailable == ""
    assert "compileall" in rendered
    assert "rustfmt" in rendered


def test_a_missing_typescript_checker_fails_closed(tmp_path) -> None:
    from zero_loop_reviewer.stage4.civer import _find_esbuild, _syntax_commands

    if _find_esbuild(None) is not None:
        return
    (tmp_path / "a.ts").write_text("export const x: number = 1;\n", encoding="utf-8")
    commands, unavailable = _syntax_commands(["a.ts"], tmp_path, None)
    assert commands == []
    assert "esbuild" in unavailable


def test_a_minimal_model_patch_without_trailing_context_applies(tmp_path) -> None:
    from zero_loop_reviewer.stage4.civer import _apply_patch

    tree = tmp_path / "tree"
    (tree / "src").mkdir(parents=True)
    (tree / "src" / "lib.rs").write_text(
        "pub fn parse(raw: Option<i32>) -> i32 {\n    raw.unwrap()\n}\n",
        encoding="utf-8",
    )
    # Model-authored Git diffs often omit the final context line.  Strict
    # `git apply` rejects this; CIVER's zero-context fallback must still apply
    # it, because compile/analyzer/no-op checks decide whether it is a repair.
    patch = (
        "--- a/src/lib.rs\n"
        "+++ b/src/lib.rs\n"
        "@@ -1,2 +1,2 @@\n"
        " pub fn parse(raw: Option<i32>) -> i32 {\n"
        "-    raw.unwrap()\n"
        "+    raw.unwrap_or(0)\n"
    )
    work = tmp_path / "work"
    work.mkdir()
    result = _apply_patch(patch, tree, LocalCommandRunner(), Settings().with_paths(tmp_path / "state"), work)
    assert result == ""
    assert "unwrap_or(0)" in (tree / "src" / "lib.rs").read_text(encoding="utf-8")


def test_rule_present_is_scoped_to_the_candidate_line(tmp_path) -> None:
    from zero_loop_reviewer.pipeline import _rule_present

    tree = tmp_path / "tree"
    (tree / "src").mkdir(parents=True)
    target = tree / "src" / "lib.rs"
    target.write_text(
        "pub fn a(raw: Option<i32>) -> i32 {\n    raw.unwrap()\n}\n"
        "pub fn b(raw: Option<i32>) -> i32 {\n    raw.unwrap()\n}\n",
        encoding="utf-8",
    )
    settings = load_settings(
        {"sast.semgrep_config": str(Path(__file__).resolve().parents[1] / "rules")}, {}
    )
    assert _rule_present(tree, "src/lib.rs", "rust-unwrap", settings, 30, line=2) is True
    assert _rule_present(tree, "src/lib.rs", "rust-unwrap", settings, 30, line=5) is True
    target.write_text(
        "pub fn a(raw: Option<i32>) -> i32 {\n    raw.unwrap_or(0)\n}\n"
        "pub fn b(raw: Option<i32>) -> i32 {\n    raw.unwrap()\n}\n",
        encoding="utf-8",
    )
    assert _rule_present(tree, "src/lib.rs", "rust-unwrap", settings, 30, line=2) is False
    assert _rule_present(tree, "src/lib.rs", "rust-unwrap", settings, 30, line=5) is True
