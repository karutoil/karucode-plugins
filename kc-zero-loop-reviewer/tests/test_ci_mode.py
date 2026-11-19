"""``zero-loop-reviewer ci`` against a fake GitHub API and a fake model."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from tests.support.gitfix import make_repo
from tests.support.github_server import FakeGitHub
from tests.support.openai_server import FakeOpenAI
from zero_loop_reviewer.cli import main
from zero_loop_reviewer.gitops import GitRepo

RULES = Path(__file__).resolve().parents[1] / "rules"

HEAD_FILE = (
    "def percentile(values, p):\n"
    "    ordered = sorted(values)\n"
    "    return ordered[int(len(ordered) * p)]\n"
)
FIXED_FILE = (
    "def percentile(values, p):\n"
    "    ordered = sorted(values)\n"
    "    index = min(len(ordered) - 1, max(0, int(len(ordered) * p)))\n"
    "    return ordered[index]\n"
)


def _answers(patch: str) -> dict:
    return {
        "TriagePlan": {"focus": [], "search_symbols": [], "risk_hypotheses": [], "budget_note": ""},
        "SecSemScan": {
            "monologue": "No untrusted input.",
            "cwe": "",
            "verdict": "SAFE",
            "cited_lines": [],
            "taint": [],
            "lifetime": [],
            "confidence": 0.4,
        },
        "BehavioralCandidates": {
            "candidates": [
                {
                    "title": "percentile indexes past the end at p=1.0",
                    "category": "correctness",
                    "severity": 2,
                    "cited_sites": [{"path": "calc.py", "line": 3}],
                    "root_cause": "int(len * 1.0) == len",
                    "suspected_trigger": "percentile([1,2,3], 1.0)",
                }
            ]
        },
        "ProbeSpec": {
            "test_path": ".zero/test_percentile.py",
            "content": "from calc import percentile\n\n\ndef test_full():\n    assert percentile([1, 2, 3], 1.0) == 3\n",
            "target_symbols": ["percentile"],
            "framework": "pytest",
            "rationale": "boundary",
        },
        "FallbackVerdict": {"verdict": "accept", "confidence": 0.9, "rationale": "probe is evidence"},
        "PatchProposal": {"patch": patch, "files_changed": ["calc.py"], "rationale": "clamp"},
        "RiskElicitation": {
            "defect_present": True,
            "category": "correctness",
            "severity_score": 2,
            "failure_probability": 0.95,
            "cited_sites": [{"path": "calc.py", "line": 3}],
            "root_cause": "index len",
        },
        "MonitorVote": {"score": 0.9, "rationale": "boundary math"},
        "MciScan": {"claims": [], "summary": ""},
    }


def test_ci_posts_exactly_one_review(tmp_path, monkeypatch):
    fixture = make_repo(tmp_path, "ci")
    base = fixture.commit("base", **{"calc.py": "def add(a, b):\n    return a + b\n"})
    head = fixture.commit("head", **{"calc.py": HEAD_FILE})
    import difflib

    patch = "\n".join(
        difflib.unified_diff(
            HEAD_FILE.splitlines(),
            FIXED_FILE.splitlines(),
            fromfile="a/calc.py",
            tofile="b/calc.py",
            lineterm="",
        )
    ) + "\n"
    pull = {
        "number": 1,
        "title": "Add percentile",
        "body": "Adds a percentile helper.",
        "user": {"login": "toil"},
        "base": {"ref": "main", "sha": base},
        "head": {"ref": "feature", "sha": head},
        "html_url": "https://example.invalid/pr/1",
        "draft": False,
    }
    with FakeOpenAI(_answers(patch)) as openai, FakeGitHub(pull=pull) as github:
        config = tmp_path / "config.toml"
        config.write_text(
            f"""
[llm]
base_url = "{openai.base_url}"

[ci]
provider = "github"
api_base = "{github.base_url}"
token_env = "GITHUB_TOKEN"

[sandbox]
runtime = "local"

[sast]
semgrep_config = "{RULES}"

[review]
deadline_secs = 120
stage2_timeout_secs = 40
""",
            encoding="utf-8",
        )
        monkeypatch.setenv("GITHUB_TOKEN", "gh-test")
        monkeypatch.setenv("ZERO_LOOP_LLM_API_KEY", "sk-test")
        code = main(
            [
                "ci",
                "--config",
                str(config),
                "--provider",
                "github",
                "--workspace",
                str(fixture.root),
                "--repository",
                "owner/repo",
                "--number",
                "1",
                "--head-sha",
                head,
                "--out-dir",
                str(tmp_path / "artifacts"),
            ]
        )
        reviews = list(github.reviews)
        requests = list(github.requests)

    assert code == 1, code  # request changes fails the CI job
    assert len(reviews) == 1, reviews
    review = reviews[0]
    assert review["event"] == "REQUEST_CHANGES"
    assert len(review["comments"]) == 1
    assert "```suggestion" in review["comments"][0]["body"]
    assert "percentile" in review["body"]
    assert any(method == "POST" and path.endswith("/reviews") for method, path, _ in requests)
    assert (tmp_path / "artifacts").is_dir()
    assert list((tmp_path / "artifacts").glob("review-*.json"))

    verdict = json.loads(next((tmp_path / "artifacts").glob("review-*.json")).read_text(encoding="utf-8"))
    assert verdict["head_sha"] == head
    assert verdict["base_sha"] == GitRepo(fixture.root).merge_base(base, head)
