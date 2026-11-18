"""Stage 2 executor: three concurrent tracks under one deadline."""

from __future__ import annotations

import shutil
import time

import pytest

from tests.support.gitfix import make_repo
from tests.support.llm import ScriptedLLM
from zero_loop_reviewer.config import SastConfig, Settings
from zero_loop_reviewer.errors import FailureKind, LLMError
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import ContextSlice, FindingStatus, SecSemScan, StageId, VerificationKind
from zero_loop_reviewer.stage2.executor import Stage2Outcome, run_stage2
from zero_loop_reviewer.stage2.sast import DEFAULT_RULES

HAVE_SEMGREP = shutil.which("semgrep") is not None and DEFAULT_RULES.is_file()
requires_semgrep = pytest.mark.skipif(not HAVE_SEMGREP, reason="semgrep is not installed; executor tests are skipped")

BASE_FILE = "def f(v):\n    return v\n"
HEAD_FILE = (
    "import subprocess\n"
    "\n"
    "\n"
    "def run(cmd):\n"
    "    subprocess.run(cmd, shell=True)\n"
    "    return 1\n"
    "\n"
    "\n"
    "def leak(path):\n"
    "    fh = open(path)\n"
    "    return fh.read()\n"
)
SHELL_LINE = 5


@pytest.fixture(scope="module")
def scenario(tmp_path_factory):
    root = tmp_path_factory.mktemp("executor")
    fixture = make_repo(root, "repo")
    base = fixture.commit("base", **{"pkg__a.py": BASE_FILE})
    head = fixture.commit("head", **{"pkg__vuln.py": HEAD_FILE})
    repo = GitRepo(fixture.root)
    return fixture.root, repo, repo.diff(base, head)


def _context() -> ContextSlice:
    return ContextSlice(text="changed: pkg/vuln.py", tokens=6, budget=1000)


def _settings(*, timeout_s: float = 30.0, semgrep_bin: str = "semgrep") -> Settings:
    return Settings(stage2_timeout_s=timeout_s, sast=SastConfig(semgrep_bin=semgrep_bin, timeout_s=25.0))


def _scan() -> dict:
    return {
        "monologue": (
            "cmd comes from the caller and reaches subprocess.run with shell=True at pkg/vuln.py:5; "
            "nothing separates them."
        ),
        "cwe": "CWE-78",
        "verdict": "VULNERABLE",
        "cited_lines": [{"path": "pkg/vuln.py", "line": SHELL_LINE}],
        "confidence": 0.9,
    }


def _run(scenario, llm, settings) -> tuple[Stage2Outcome, float]:
    root, repo, diff = scenario
    started = time.monotonic()
    outcome = run_stage2(root, repo, diff, _context(), llm, settings, environ={"SEMGREP_SEND_METRICS": "off"})
    return outcome, time.monotonic() - started


@requires_semgrep
@pytest.mark.integration
def test_stage2_runs_every_track_and_stays_inside_the_deadline(scenario):
    llm = ScriptedLLM(answers={"security": [_scan()]})
    outcome, wall = _run(scenario, llm, _settings(timeout_s=30.0))

    assert isinstance(outcome, Stage2Outcome)
    assert outcome.elapsed_ms < 30_000
    assert wall < 30.0
    assert outcome.degraded == [], outcome.degraded

    rule_ids = {finding.rule_id for finding in outcome.findings}
    assert {"subprocess-shell-true", "open-without-context"} <= rule_ids
    assert all(finding.patch_caused for finding in outcome.findings if finding.path == "pkg/vuln.py")

    assert [frame.path for frame in outcome.frames] == ["pkg/vuln.py"]
    assert outcome.frames[0].language == "python"
    assert [result.path for result in outcome.rale] == ["pkg/vuln.py"]

    assert len(outcome.scans) == 1
    assert outcome.scans[0].verdict == "VULNERABLE"

    assert len(outcome.candidates) == 1
    candidate = outcome.candidates[0]
    assert candidate.stage is StageId.SECSEM
    assert candidate.evidence.kind == "analyzer"
    assert candidate.verification.kind is VerificationKind.ANALYZER_TRACE
    assert candidate.status is FindingStatus.VERIFIED
    assert candidate.cwe == "CWE-78"
    assert llm.calls[0]["stage"] == "security"


@requires_semgrep
@pytest.mark.integration
def test_output_ordering_is_deterministic(scenario):
    llm = ScriptedLLM(answers={"security": [_scan()]})
    outcome, _wall = _run(scenario, llm, _settings())

    assert [f.path for f in outcome.findings] == sorted(f.path for f in outcome.findings)
    assert [frame.path for frame in outcome.frames] == sorted(frame.path for frame in outcome.frames)
    assert [result.path for result in outcome.rale] == sorted(result.path for result in outcome.rale)
    ordered = sorted(
        outcome.candidates,
        key=lambda c: (-c.severity, -(c.p or 0.0), c.id),
    )
    assert [c.id for c in outcome.candidates] == [c.id for c in ordered]
    assert SecSemScan.model_validate(_scan()).verdict == outcome.scans[0].verdict


@requires_semgrep
@pytest.mark.integration
def test_an_llm_failure_degrades_only_track_2b(scenario):
    llm = ScriptedLLM(answers={"security": [LLMError("provider timed out", kind=FailureKind.TIMEOUT)]})
    outcome, wall = _run(scenario, llm, _settings())

    assert outcome.degraded, "the security track must report its failure"
    assert all(message.startswith("2b") for message in outcome.degraded), outcome.degraded
    assert "provider timed out" in " ".join(outcome.degraded)
    # 2A and 2C kept working.
    assert outcome.findings
    assert outcome.rale and outcome.rale[0].path == "pkg/vuln.py"
    assert outcome.frames and outcome.frames[0].path == "pkg/vuln.py"
    assert outcome.candidates == []
    assert outcome.scans == []
    assert wall < 30.0


@requires_semgrep
@pytest.mark.integration
def test_a_missing_analyzer_degrades_only_track_2a(scenario):
    llm = ScriptedLLM(answers={"security": [_scan()]})
    outcome, _wall = _run(scenario, llm, _settings(semgrep_bin="definitely-not-a-semgrep-binary"))

    assert any(message.startswith("2a") for message in outcome.degraded), outcome.degraded
    assert not any(message.startswith("2b") or message.startswith("2c") for message in outcome.degraded)
    assert outcome.findings == []
    assert outcome.rale and outcome.rale[0].path == "pkg/vuln.py"
    # Without an analyzer trace the confident, in-patch claim is still kept, but unverified.
    assert len(outcome.candidates) == 1
    assert outcome.candidates[0].status is FindingStatus.UNVERIFIED
    assert outcome.candidates[0].verification.kind is VerificationKind.UNVERIFIED


@requires_semgrep
@pytest.mark.integration
def test_the_stage_returns_at_the_timeout_even_if_a_track_is_slow(scenario):
    llm = ScriptedLLM(answers={"security": [_scan()]})
    outcome, wall = _run(scenario, llm, _settings(timeout_s=1.0))

    assert wall < 8.0, f"the stage overran its 1s budget: {wall:.2f}s"
    assert outcome.elapsed_ms < 8_000
    assert isinstance(outcome.degraded, list)
