"""End-to-end review on real git repositories.

The model is a real HTTP client talking to a local OpenAI-compatible server;
the analyzer is real Semgrep; the sandbox is the real local runner; worktrees
and patches are real git.  Only the *answers* are scripted, so a failure here
is a pipeline failure, not a model-quality claim.
"""

from __future__ import annotations

import difflib
import json
import os
import sys
from pathlib import Path

import pytest

from tests.support.gitfix import make_repo, run
from tests.support.openai_server import FakeOpenAI
from zero_loop_reviewer.config import load_settings
from zero_loop_reviewer.errors import FailureKind, LLMError
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import Decision, FindingStatus, VerificationKind
from zero_loop_reviewer.pipeline import ReviewRequest, Reviewer
from zero_loop_reviewer.stage1.receipts import verify_chain

RULES = Path(__file__).resolve().parents[1] / "rules"
SENTINEL = "SENTINEL_PR_DESCRIPTION_TEXT"

BASE_CALC = "def add(a, b):\n    return a + b\n"
HEAD_CALC = (
    "def add(a, b):\n"
    "    return a + b\n"
    "\n"
    "\n"
    "def percentile(values, p):\n"
    "    ordered = sorted(values)\n"
    "    return ordered[int(len(ordered) * p)]\n"
)
FIXED_CALC = (
    "def add(a, b):\n"
    "    return a + b\n"
    "\n"
    "\n"
    "def percentile(values, p):\n"
    "    if not values:\n"
    "        raise ValueError(\"empty\")\n"
    "    ordered = sorted(values)\n"
    "    index = min(len(ordered) - 1, max(0, int(len(ordered) * p)))\n"
    "    return ordered[index]\n"
)
PROBE = (
    "from calc import percentile\n"
    "\n"
    "\n"
    "def test_full_percentile_returns_last():\n"
    "    assert percentile([1, 2, 3], 1.0) == 3\n"
    "\n"
    "\n"
    "def test_zero_percentile_returns_first():\n"
    "    assert percentile([1, 2, 3], 0.0) == 1\n"
)


def _patch(old: str, new: str, path: str = "calc.py") -> str:
    return "\n".join(
        difflib.unified_diff(
            old.splitlines(),
            new.splitlines(),
            fromfile=f"a/{path}",
            tofile=f"b/{path}",
            lineterm="",
        )
    ) + "\n"


def _answers(patch: str, *, hypotheses=1, behavioral_category="correctness") -> dict[str, object]:
    return {
        "TriagePlan": {
            "focus": [{"path": "calc.py", "function": "percentile", "reason": "new boundary math", "priority": 3}],
            "search_symbols": ["percentile"],
            "risk_hypotheses": ["p=1.0 indexes past the end"],
            "budget_note": "",
        },
        "SecSemScan": {
            "monologue": "No untrusted data reaches a dangerous sink in this changed code.",
            "cwe": "",
            "verdict": "SAFE",
            "cited_lines": [],
            "taint": [],
            "lifetime": [],
            "confidence": 0.6,
        },
        "BehavioralCandidates": {
            "candidates": (
                [
                    {
                        "title": "percentile indexes past the end at p=1.0",
                        "category": behavioral_category,
                        "severity": 2,
                        "cited_sites": [{"path": "calc.py", "line": 7, "symbol": "percentile"}],
                        "root_cause": "int(len * 1.0) == len, so the index is out of range",
                        "suspected_trigger": "percentile([1,2,3], 1.0)",
                    }
                ]
                if hypotheses
                else []
            )
        },
        "ProbeSpec": {
            "test_path": ".zero/test_percentile.py",
            "content": PROBE,
            "target_symbols": ["percentile"],
            "framework": "pytest",
            "rationale": "boundary input",
        },
        "FallbackVerdict": {"verdict": "uncertain", "confidence": 0.4, "rationale": "the probe is the evidence"},
        "PatchProposal": {"patch": patch, "files_changed": ["calc.py"], "rationale": "clamp the index"},
        "RiskElicitation": {
            "defect_present": True,
            "category": "correctness",
            "severity_score": 2,
            "failure_probability": 0.95,
            "cited_sites": [{"path": "calc.py", "line": 7}],
            "root_cause": "p=1.0 produces index len",
        },
        "MonitorVote": {"score": 0.9, "rationale": "boundary arithmetic is wrong"},
        "MciScan": {"claims": [], "summary": "no behavioural claims"},
    }


def _settings(fake: FakeOpenAI, state: Path):
    return load_settings(
        {
            "llm.base_url": fake.base_url,
            "llm.api_key_env": "E2E_LLM_KEY",
            "sandbox.runtime": "local",
            "state.dir": str(state),
            "review.deadline_secs": 150,
            "review.stage2_timeout_secs": 45,
            "sast.semgrep_config": str(RULES),
        },
        {**os.environ, "E2E_LLM_KEY": "sk-e2e"},
    )


@pytest.fixture()
def repo(tmp_path: Path):
    fixture = make_repo(tmp_path)
    base = fixture.commit("base", **{"calc.py": BASE_CALC})
    head = fixture.commit("add percentile", **{"calc.py": HEAD_CALC})
    return fixture, base, head


def test_buggy_new_function_is_blocked_with_a_verified_suggestion(repo, tmp_path):
    fixture, base, head = repo
    patch = _patch(HEAD_CALC, FIXED_CALC)
    with FakeOpenAI(_answers(patch)) as fake:
        settings = _settings(fake, tmp_path / "state")
        reviewer = Reviewer(
            settings,
            environ={**os.environ, "E2E_LLM_KEY": "sk-e2e"},
        )
        verdict = reviewer.review(
            ReviewRequest(
                workspace=fixture.root,
                base_ref=base,
                head_ref=head,
                expected_head_sha=head,
                description=f"Unrelated prose. {SENTINEL}",
            )
        )
        requests = list(fake.requests)

    assert verdict.decision is Decision.REQUEST_CHANGES, verdict.degraded
    assert len(verdict.findings) == 1
    finding = verdict.findings[0]
    assert finding.verification is VerificationKind.PROBE
    assert finding.p_combined >= verdict.tau
    assert "```suggestion" in finding.suggestion
    assert "return ordered[index]" in finding.suggestion
    assert "+++ b/" not in finding.suggestion
    assert finding.suggestion_start_line is not None
    assert finding.suggestion_patch

    # The verified patch applies to the real head commit.
    worktree = Path(tmp_path) / "patch-check"
    repo_obj = GitRepo(fixture.root)
    repo_obj.add_worktree(head, worktree)
    try:
        patch_file = Path(tmp_path) / "fix.diff"
        patch_file.write_text(finding.suggestion_patch, encoding="utf-8")
        run(worktree, "apply", "--check", str(patch_file))
    finally:
        repo_obj.remove_worktree(worktree)

    ok, why = verify_chain(verdict.receipts)
    assert ok, why
    assert any(receipt.stage == "3.probe" and receipt.status.value == "completed" for receipt in verdict.receipts)
    model_receipts = [
        receipt
        for receipt in verdict.receipts
        if receipt.stage in {"triage", "security", "behavioral", "probe", "civer", "risk", "monitor", "fallback"}
    ]
    assert model_receipts
    assert all(receipt.input_sha256 and len(receipt.input_sha256) == 64 for receipt in model_receipts)
    assert all(receipt.model for receipt in model_receipts)

    # The monitor is an independent instance with its own k=5 sampling.
    monitor_calls = [
        request
        for request in requests
        if "SCHEMA: MonitorVote" in request["body"]["messages"][1]["content"]
    ]
    assert len(monitor_calls) == 5
    assert all(request["body"]["temperature"] == 0.8 for request in monitor_calls)

    # Author text reached the PMI prompt and nothing else.
    for request in requests:
        user = request["body"]["messages"][1]["content"]
        if "SCHEMA: MciScan" not in user:
            assert SENTINEL not in user, "the PR description leaked into a review prompt"
    assert any("SCHEMA: MciScan" in request["body"]["messages"][1]["content"] for request in requests)

    # The risk elicitation carried no policy vocabulary.
    risk_calls = [
        request for request in requests if "SCHEMA: RiskElicitation" in request["body"]["messages"][1]["content"]
    ]
    assert risk_calls
    text = risk_calls[0]["body"]["messages"][1]["content"].lower()
    for banned in ("false accept", "false reject", "threshold", "accept/reject", "request changes"):
        assert banned not in text


def test_clean_diff_approves_with_no_findings(repo, tmp_path):
    fixture, base, head = repo
    # A healthy feature change with no structural or behavioural risk.
    fixture.checkout("main")
    clean_head = fixture.commit(
        "correct feature",
        **{
            "calc.py": BASE_CALC
            + "\n\ndef clamp(value, low, high):\n    if high < low:\n        raise ValueError(\"bad range\")\n    return min(high, max(low, value))\n"
        },
    )
    answers = _answers(_patch(HEAD_CALC, FIXED_CALC), hypotheses=0)
    answers["BehavioralCandidates"] = {"candidates": []}
    with FakeOpenAI(answers) as fake:
        settings = _settings(fake, tmp_path / "state")
        verdict = Reviewer(settings, environ={**os.environ, "E2E_LLM_KEY": "sk-e2e"}).review(
            ReviewRequest(workspace=fixture.root, base_ref=base, head_ref=clean_head, expected_head_sha=clean_head)
        )
    assert verdict.decision is Decision.APPROVE, verdict.degraded
    assert verdict.findings == []
    assert verdict.suppressed_count == 0


def test_stale_head_refuses_before_any_model_call(repo, tmp_path):
    fixture, base, head = repo
    with FakeOpenAI(_answers("")) as fake:
        settings = _settings(fake, tmp_path / "state")
        verdict = Reviewer(settings, environ={**os.environ, "E2E_LLM_KEY": "sk-e2e"}).review(
            ReviewRequest(
                workspace=fixture.root,
                base_ref=base,
                head_ref=head,
                expected_head_sha="b" * 40,
            )
        )
        calls = fake.requests
    assert verdict.decision is Decision.HUMAN_REVIEW
    assert any("stale_head" in reason for reason in verdict.degraded)
    assert verdict.findings == []
    assert calls == []


def test_no_model_endpoint_fails_open_to_a_human(repo, tmp_path):
    fixture, base, head = repo
    settings = load_settings(
        {
            "sandbox.runtime": "local",
            "state.dir": str(tmp_path / "state"),
            "review.deadline_secs": 90,
            "sast.semgrep_config": str(RULES),
        },
        {**os.environ, "E2E_LLM_KEY": ""},
    )
    verdict = Reviewer(settings, environ={**os.environ, "E2E_LLM_KEY": ""}).review(
        ReviewRequest(workspace=fixture.root, base_ref=base, head_ref=head, expected_head_sha=head)
    )
    assert verdict.decision is Decision.HUMAN_REVIEW
    assert any("llm_unavailable" in reason for reason in verdict.degraded)
    assert verdict.findings == []


def test_an_unanchored_hypothesis_never_becomes_a_finding(repo, tmp_path):
    fixture, base, head = repo
    answers = _answers(_patch(HEAD_CALC, FIXED_CALC))
    answers["BehavioralCandidates"] = {
        "candidates": [
            {
                "title": "invented defect in a file that is not changed",
                "category": "correctness",
                "severity": 3,
                "cited_sites": [{"path": "nowhere.py", "line": 99}],
                "root_cause": "hallucinated",
                "suspected_trigger": "anything",
            }
        ]
    }
    with FakeOpenAI(answers) as fake:
        settings = _settings(fake, tmp_path / "state")
        verdict = Reviewer(settings, environ={**os.environ, "E2E_LLM_KEY": "sk-e2e"}).review(
            ReviewRequest(workspace=fixture.root, base_ref=base, head_ref=head, expected_head_sha=head)
        )
    assert verdict.findings == []
    assert any("behavioral_scan_filtered" in reason for reason in verdict.degraded)


def test_exploitable_security_finding_is_redacted_but_recorded_privately(tmp_path):
    """A public PR gets a gate status; the patch and mechanics stay private."""
    fixture = make_repo(tmp_path, "sqlinj")
    base = fixture.commit(
        "base",
        **{"db.py": "def table_name():\n    return \"users\"\n"},
    )
    head_file = (
        "def table_name():\n"
        "    return \"users\"\n"
        "\n"
        "\n"
        "def find_user(conn, name):\n"
        "    query = f\"SELECT * FROM users WHERE name = '{name}'\"\n"
        "    return conn.execute(query).fetchall()\n"
    )
    head = fixture.commit("injectable lookup", **{"db.py": head_file})
    fixed = (
        "def table_name():\n"
        "    return \"users\"\n"
        "\n"
        "\n"
        "def find_user(conn, name):\n"
        "    return conn.execute(\"SELECT * FROM users WHERE name = ?\", (name,)).fetchall()\n"
    )
    probe = (
        "import sqlite3\n"
        "\n"
        "from db import find_user\n"
        "\n"
        "\n"
        "def test_injection_returns_no_rows():\n"
        "    conn = sqlite3.connect(\":memory:\")\n"
        "    conn.execute(\"create table users (name text)\")\n"
        "    conn.execute(\"insert into users values ('alice')\")\n"
        "    assert find_user(conn, \"x' OR '1'='1\") == []\n"
    )
    answers = {
        "TriagePlan": {
            "focus": [{"path": "db.py", "function": "find_user", "reason": "new SQL", "priority": 3}],
            "search_symbols": [],
            "risk_hypotheses": [],
            "budget_note": "",
        },
        "SecSemScan": {
            "monologue": "The untrusted name reaches conn.execute through an f-string at db.py:6, with no sanitizer.",
            "cwe": "CWE-89",
            "verdict": "VULNERABLE",
            "cited_lines": [{"path": "db.py", "line": 6, "symbol": "name"}],
            "taint": [
                {
                    "source": {"path": "db.py", "line": 6, "symbol": "name"},
                    "sink": {"path": "db.py", "line": 7, "symbol": "execute"},
                    "rule_id": "zero-loop.sql-injection-via-variable",
                }
            ],
            "lifetime": [],
            "confidence": 0.9,
        },
        "BehavioralCandidates": {"candidates": []},
        "ProbeSpec": {
            "test_path": ".zero/test_db.py",
            "content": probe,
            "target_symbols": ["find_user"],
            "framework": "pytest",
            "rationale": "injection",
        },
        "FallbackVerdict": {"verdict": "uncertain", "confidence": 0.3, "rationale": "probe is the evidence"},
        "PatchProposal": {
            "patch": _patch(head_file, fixed, "db.py"),
            "files_changed": ["db.py"],
            "rationale": "bind parameters",
        },
        "RiskElicitation": {
            "defect_present": True,
            "category": "security",
            "severity_score": 3,
            "failure_probability": 0.9,
            "cited_sites": [{"path": "db.py", "line": 6}],
            "root_cause": "the name is interpolated into SQL",
        },
        "MonitorVote": {"score": 0.9, "rationale": "injection is reachable"},
        "MciScan": {"claims": [], "summary": ""},
    }
    with FakeOpenAI(answers) as fake:
        settings = _settings(fake, tmp_path / "state")
        verdict = Reviewer(settings, environ={**os.environ, "E2E_LLM_KEY": "sk-e2e"}).review(
            ReviewRequest(workspace=fixture.root, base_ref=base, head_ref=head, expected_head_sha=head)
        )

    assert verdict.decision is Decision.REQUEST_CHANGES, verdict.degraded
    assert verdict.findings
    assert all(finding.redacted for finding in verdict.findings), [
        (finding.title, finding.category.value, finding.cwe, finding.evidence_summary)
        for finding in verdict.findings
    ]
    assert all(finding.suggestion == "" and finding.suggestion_patch == "" for finding in verdict.findings)
    assert all("CWE-89" in finding.title for finding in verdict.findings)

    private_dir = (tmp_path / "state") / "private"
    records = sorted(private_dir.glob("*.json"))
    assert records, verdict.findings
    for record in records:
        assert record.stat().st_mode & 0o777 == 0o600
    text = "\n".join(record.read_text(encoding="utf-8") for record in records)
    assert "find_user" in text
    assert "SELECT * FROM users WHERE name = ?" in text, (text[:1200], verdict.degraded)

    from zero_loop_reviewer.stage5.formatter import render_review

    payload = render_review(verdict, bot_login="zero-loop-reviewer[bot]")
    assert payload.comments == []
    assert "conn.execute" not in payload.body
    assert "SELECT *" not in payload.body
    assert "CWE-89" in payload.body


@pytest.mark.integration
def test_pipeline_runs_a_real_podman_dual_commit_review(repo, tmp_path):
    """The whole cascade inside a network-isolated container, not the host runner."""
    import shutil
    import time
    from zero_loop_reviewer.stage3.sandbox import build_sandbox

    fixture, base, head = repo
    state = Path("/home/karutoil/.cache") / f"zero-loop-e2e-{os.getpid()}-{int(time.time())}"
    if state.exists():
        shutil.rmtree(state)
    settings = load_settings(
        {
            "sandbox.runtime": "podman",
            "sandbox.timeout_secs": 120,
            "state.dir": str(state),
            "review.deadline_secs": 240,
            "review.stage2_timeout_secs": 45,
            "sast.semgrep_config": str(RULES),
        },
        {**os.environ},
    )
    available, detail = build_sandbox(settings).available()
    if not available:
        pytest.skip(f"podman sandbox unavailable: {detail}")
    patch = _patch(HEAD_CALC, FIXED_CALC)
    try:
        with FakeOpenAI(_answers(patch)) as fake:
            settings = load_settings(
                {
                    "llm.base_url": fake.base_url,
                    "llm.api_key_env": "E2E_LLM_KEY",
                    "sandbox.runtime": "podman",
                    "sandbox.timeout_secs": 120,
                    "state.dir": str(state),
                    "review.deadline_secs": 240,
                    "review.stage2_timeout_secs": 45,
                    "sast.semgrep_config": str(RULES),
                },
                {**os.environ, "E2E_LLM_KEY": "sk-e2e"},
            )
            verdict = Reviewer(settings, environ={**os.environ, "E2E_LLM_KEY": "sk-e2e"}).review(
                ReviewRequest(workspace=fixture.root, base_ref=base, head_ref=head, expected_head_sha=head)
            )
        assert verdict.decision is Decision.REQUEST_CHANGES, verdict.degraded
        assert verdict.findings and verdict.findings[0].verification is VerificationKind.PROBE
        assert "```suggestion" in verdict.findings[0].suggestion
        probe_receipts = [receipt for receipt in verdict.receipts if receipt.stage == "3.sandbox"]
        assert probe_receipts, verdict.degraded
        assert all("python:3.12-alpine" in receipt.detail for receipt in probe_receipts)
    finally:
        shutil.rmtree(state, ignore_errors=True)
