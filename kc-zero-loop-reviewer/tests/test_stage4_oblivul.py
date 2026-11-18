"""Stage 4B: ObliVul measures both interventions with a real analyzer command."""

from __future__ import annotations

import sys

import pytest

from tests.support.gitfix import make_repo
from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.models import (
    Candidate,
    Category,
    CitedSite,
    CpgFrame,
    Evidence,
    LifetimeTuple,
    LsegObligation,
    StageId,
    TaintTuple,
)
from zero_loop_reviewer.stage4.civer import LocalCommandRunner
from zero_loop_reviewer.stage4.oblivul import build_lsog, counterfactuals

VULN = '''import subprocess


def run(cmd):
    return subprocess.run(cmd, shell=True)


def run_guarded(cmd):
    if cmd is None:
        raise ValueError("cmd is required")
    return subprocess.run(cmd, shell=True)
'''

SINK_LINE = 5

#: A tiny analyzer: it counts ``subprocess.run`` calls that have no ``is None``
#: guard in the four lines above them and prints the count as JSON.
ANALYZER = '''import json
import sys
from pathlib import Path

root = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
findings = []
for path in sorted(root.rglob("*.py")):
    if path.name == "analyzer.py":
        continue
    lines = path.read_text(encoding="utf-8").splitlines()
    for index, line in enumerate(lines):
        if "subprocess.run(" not in line:
            continue
        window = lines[max(0, index - 4):index]
        guarded = any(
            "zero_loop_guard" in previous
            or ("is None" in previous and previous.strip().startswith("if "))
            for previous in window
        )
        if not guarded:
            findings.append({"path": str(path), "line": index + 1})
print(json.dumps({"findings": findings, "count": len(findings)}))
'''


@pytest.fixture()
def checkout(tmp_path):
    fixture = make_repo(tmp_path / "checkout")
    fixture.commit("a reachable sink and a guarded one", **{"vuln.py": VULN})
    return fixture


@pytest.fixture()
def analyzer(tmp_path) -> list[str]:
    script = tmp_path / "analyzer.py"
    script.write_text(ANALYZER, encoding="utf-8")
    return [sys.executable, str(script), "."]


def _candidate() -> Candidate:
    return Candidate(
        id="cand-oblivul-1",
        stage=StageId.FALSIFY,
        category=Category.SECURITY,
        title="shell command built from caller input",
        root_cause="cmd reaches subprocess.run without a None guard",
        severity=3,
        cited_sites=[
            CitedSite(path="vuln.py", line=SINK_LINE, symbol="run", snippet="subprocess.run(cmd, shell=True)")
        ],
        evidence=Evidence(kind="cpg", analyzer="stage2:cpg", rule_id="taint-subprocess"),
    )


def _frame() -> CpgFrame:
    return CpgFrame(
        path="vuln.py",
        function="run",
        taint=[
            TaintTuple(
                source=CitedSite(path="vuln.py", line=4, symbol="cmd"),
                sink=CitedSite(path="vuln.py", line=SINK_LINE, symbol="run"),
                rule_id="taint-subprocess",
            )
        ],
        lifetime=[
            LifetimeTuple(
                alloc=CitedSite(path="vuln.py", line=4, symbol="cmd"),
                use=CitedSite(path="vuln.py", line=SINK_LINE, symbol="cmd"),
                rule_id="lifetime-cmd",
            )
        ],
        memory_roots=["cmd_buffer", "out_buffer"],
        alias_pairs=[("cmd", "alias_cmd")],
        write_sets=["cmd"],
        language="python",
    )


def _settings(tmp_path) -> Settings:
    return Settings().with_paths(tmp_path / "state")


# --------------------------------------------------------------------------- #
# LSOG projection
# --------------------------------------------------------------------------- #


def test_build_lsog_projects_taint_lifetime_and_separation_obligations():
    obligations = build_lsog(_frame())
    kinds = {obligation.kind for obligation in obligations}

    assert kinds == {"taint", "lifetime", "separated", "assigns_from"}
    taint = next(obligation for obligation in obligations if obligation.kind == "taint")
    assert taint.subjects == ["cmd", "run"]
    assert "taint-subprocess" in taint.detail
    separated = [obligation for obligation in obligations if obligation.kind == "separated"]
    assert any(obligation.subjects == ["cmd_buffer", "out_buffer"] for obligation in separated)
    assert any(obligation.subjects == ["cmd", "alias_cmd"] for obligation in separated)


def test_build_lsog_is_deterministic_and_invents_nothing():
    frame = _frame()
    assert build_lsog(frame) == build_lsog(frame)
    assert build_lsog(CpgFrame(path="empty.py")) == []

    existing = LsegObligation(kind="separated", subjects=["a", "b"], detail="from the frame")
    kept = build_lsog(CpgFrame(path="x.c", obligations=[existing]))
    assert kept == [existing]


# --------------------------------------------------------------------------- #
# Counterfactuals
# --------------------------------------------------------------------------- #


def test_counterfactuals_measure_a_drop_and_a_rise(checkout, analyzer, tmp_path):
    settings = _settings(tmp_path)

    result = counterfactuals(
        _candidate(),
        _frame(),
        checkout.root,
        LocalCommandRunner(),
        settings,
        analysis_cmd=analyzer,
    )

    assert result.passed is True, result.detail
    assert result.positive is not None and result.negative is not None
    assert result.positive.intervention == "add_guard"
    assert result.negative.intervention == "remove_guard"
    assert result.positive.risk_after < result.positive.risk_before
    assert result.negative.risk_after > result.negative.risk_before
    assert result.positive.direction_ok and result.negative.direction_ok
    assert "findings 1 -> 0" in result.positive.detail
    assert "findings 1 -> 2" in result.negative.detail
    assert result.obligations
    # The review checkout is untouched and the scratch copies are gone.
    assert (checkout.root / "vuln.py").read_text(encoding="utf-8") == VULN
    assert not list(settings.paths.work_dir.glob("oblivul-*"))


def test_a_nonzero_analyzer_exit_with_json_output_is_still_measured(checkout, tmp_path):
    script = tmp_path / "noisy_analyzer.py"
    script.write_text(ANALYZER + "\nsys.exit(1)\n", encoding="utf-8")

    result = counterfactuals(
        _candidate(),
        _frame(),
        checkout.root,
        LocalCommandRunner(),
        _settings(tmp_path),
        analysis_cmd=[sys.executable, str(script), "."],
    )

    assert result.passed is True, result.detail
    assert "findings 1 -> 0" in result.positive.detail
    assert "findings 1 -> 2" in result.negative.detail


def test_counterfactuals_without_a_command_suppress_the_candidate(checkout, tmp_path):
    result = counterfactuals(
        _candidate(), _frame(), checkout.root, LocalCommandRunner(), _settings(tmp_path), analysis_cmd=None
    )

    assert result.passed is False
    assert result.positive is None and result.negative is None
    assert "no analyzer command" in result.detail
    assert result.obligations


def test_counterfactuals_with_unparseable_output_suppress_the_candidate(checkout, tmp_path):
    result = counterfactuals(
        _candidate(),
        _frame(),
        checkout.root,
        LocalCommandRunner(),
        _settings(tmp_path),
        analysis_cmd=[sys.executable, "-c", "print('the analyzer printed prose, not JSON')"],
    )

    assert result.passed is False
    assert "no usable count" in result.detail


def test_counterfactuals_refuse_when_the_guard_does_not_lower_the_measured_risk(checkout, tmp_path):
    flat = [sys.executable, "-c", "import json; print(json.dumps({'count': 1}))"]
    result = counterfactuals(
        _candidate(), _frame(), checkout.root, LocalCommandRunner(), _settings(tmp_path), analysis_cmd=flat
    )

    assert result.passed is False
    assert result.positive is not None
    assert result.positive.direction_ok is False
    assert "did not lower" in result.detail


def test_counterfactuals_refuse_when_the_cited_site_is_outside_the_worktree(checkout, tmp_path):
    candidate = _candidate()
    candidate.cited_sites = [CitedSite(path="../escape.py", line=1)]

    result = counterfactuals(
        candidate,
        CpgFrame(path=""),
        checkout.root,
        LocalCommandRunner(),
        _settings(tmp_path),
        analysis_cmd=[sys.executable, "-c", "import json; print(json.dumps({'count': 1}))"],
    )

    assert result.passed is False
    assert "does not resolve" in result.detail


SIMPLE = "import subprocess\n\n\ndef run(cmd):\n    return subprocess.run(cmd, shell=True)\n"


def test_an_unguarded_sink_passes_on_the_already_maximal_ceiling(tmp_path):
    """There is no guard to remove, so the positive direction is the informative one."""
    fixture = make_repo(tmp_path / "ceiling")
    fixture.commit("one unguarded sink", **{"vuln.py": SIMPLE})
    script = tmp_path / "analyzer.py"
    script.write_text(ANALYZER, encoding="utf-8")
    command = [sys.executable, str(script), "."]
    candidate = Candidate(
        id="cand-ceiling",
        stage=StageId.FALSIFY,
        category=Category.SECURITY,
        title="shell command built from caller input",
        root_cause="cmd reaches subprocess.run without a guard",
        severity=3,
        cited_sites=[CitedSite(path="vuln.py", line=5, symbol="run")],
        evidence=Evidence(kind="cpg", analyzer="stage2:cpg", rule_id="taint-subprocess"),
    )
    frame = CpgFrame(
        path="vuln.py",
        function="run",
        taint=[
            TaintTuple(
                source=CitedSite(path="vuln.py", line=4, symbol="cmd"),
                sink=CitedSite(path="vuln.py", line=5, symbol="run"),
                rule_id="taint-subprocess",
            )
        ],
        language="python",
    )
    settings = Settings()
    result = counterfactuals(
        candidate,
        frame,
        fixture.root,
        LocalCommandRunner(),
        settings,
        analysis_cmd=command,
    )
    assert result.passed is True, result.detail
    assert result.positive is not None and result.positive.applied is True
    assert result.positive.risk_after < result.positive.risk_before
    assert result.negative is not None and result.negative.applied is False
    assert "ceiling" in result.detail
