"""Large-PR behaviour: batching, bounded analyzers, and fail-open coverage.

A single evidence block only ever sees the first files of a large diff.  These
tests pin the two properties that make a 2,000-line review honest: every
eligible file is assigned to a behavioural batch, and an incomplete/truncated
context cannot silently approve.
"""

from __future__ import annotations

from pathlib import Path

from tests.support.gitfix import make_repo
from tests.support.llm import ScriptedLLM
from zero_loop_reviewer.config import load_settings
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.llm.prompts import behavioral_prompt
from zero_loop_reviewer.models import Decision
from zero_loop_reviewer.pipeline import (
    ReviewRequest,
    Reviewer,
    _behavioral_batches,
    _context_for_batch,
)
from zero_loop_reviewer.stage1.context import build_context
from zero_loop_reviewer.stage2.sast import _target_chunks


def _large_repo(tmp_path, files: int = 12, functions: int = 30):
    fixture = make_repo(tmp_path, "large")
    fixture.commit(
        "base",
        **{f"pkg/mod{i}.py": "def f(x):\n    return x\n" for i in range(files)},
    )
    head = {}
    for i in range(files):
        head[f"pkg/mod{i}.py"] = (
            "\n".join(f"def func_{i}_{j}(value):\n    return value + {j}" for j in range(functions))
            + "\n"
        )
    fixture.commit("head", **head)
    return fixture


def test_every_eligible_file_reaches_a_behavioural_batch(tmp_path) -> None:
    fixture = _large_repo(tmp_path, files=12, functions=400)
    repo = GitRepo(fixture.root)
    diff = repo.diff("HEAD~1", "HEAD")
    batches, capped, oversized = _behavioral_batches(diff)
    assert len(batches) > 1
    assert capped is False
    assert oversized is False
    assigned = {patch.path for batch in batches for patch in batch}
    assert assigned == {f"pkg/mod{i}.py" for i in range(12)}
    context = build_context(repo, diff, load_settings({}, {}))
    for batch in batches:
        batch_diff = diff.model_copy(update={"files": batch})
        prompt = behavioral_prompt(
            batch_diff,
            _context_for_batch(context, {patch.path for patch in batch}),
        )
        for patch in batch:
            assert patch.path in prompt.user


def test_semgrep_chunks_stay_under_the_kernel_argument_limit() -> None:
    targets = [f"crates/crate_{i // 100}/src/file_{i}.rs" for i in range(1_000)]
    chunks = _target_chunks(targets)
    assert len(chunks) == 5
    assert all(len(chunk) <= 200 for chunk in chunks)
    assert [path for chunk in chunks for path in chunk] == targets


def test_a_truncated_context_cannot_approve(tmp_path) -> None:
    fixture = _large_repo(tmp_path, files=8, functions=200)
    settings = load_settings(
        {
            "review.max_model_tokens": 4000,
            "review.deadline_secs": 60,
            "sandbox.runtime": "local",
            "state.dir": str(tmp_path / "state"),
        },
        {},
    )
    llm = ScriptedLLM(
        answers={
            "TriagePlan": {
                "focus": [],
                "search_symbols": [],
                "risk_hypotheses": [],
                "budget_note": "",
            },
            "SecSemScan": {
                "monologue": "No tainted path is visible in this large refactor.",
                "cwe": "",
                "verdict": "SAFE",
                "cited_lines": [],
                "taint": [],
                "lifetime": [],
                "confidence": 0.4,
            },
            "BehavioralCandidates": {"candidates": []},
        }
    )
    verdict = Reviewer(settings, environ={}, llm=llm).review(
        ReviewRequest(workspace=fixture.root, base_ref="HEAD~1", head_ref="HEAD")
    )
    assert verdict.decision is Decision.HUMAN_REVIEW
    assert any(reason.startswith("context_truncated:") for reason in verdict.degraded)


def test_test_files_are_not_sent_to_the_behavioural_reviewer(tmp_path) -> None:
    fixture = make_repo(tmp_path, "tests-excluded")
    fixture.commit("base", **{"pkg/prod.py": "def f(x):\n    return x\n"})
    fixture.commit(
        "head",
        **{
            "pkg/prod.py": "def f(x):\n    return x + 1\n",
            "tests/test_prod.py": "def test_f():\n    assert f(1) == 2\n",
        },
    )
    diff = GitRepo(fixture.root).diff("HEAD~1", "HEAD")
    batches, _, _ = _behavioral_batches(diff)
    assigned = {patch.path for batch in batches for patch in batch}
    assert assigned == {"pkg/prod.py"}
