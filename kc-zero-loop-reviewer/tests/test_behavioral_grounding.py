"""Behavioural hypotheses need a mechanical falsifier before they become
candidates (karucode CI smoke test: a workflow-only PR produced two YAML
hypotheses that no probe or rule could ever verify)."""

from __future__ import annotations

from zero_loop_reviewer.models import AnalyzerFinding, CitedSite
from zero_loop_reviewer.pipeline import _groundable


def _site(path: str, line: int = 10) -> CitedSite:
    return CitedSite(path=path, line=line)


def test_python_is_groundable_because_stage3_can_run_a_probe() -> None:
    assert _groundable(_site("scripts/check.py"), []) is True


def test_rust_and_typescript_are_groundable_because_stage3_can_probe_them() -> None:
    assert _groundable(_site("crates/kc-core/src/lib.rs"), []) is True
    assert _groundable(_site("web/src/App.tsx"), []) is True
    assert _groundable(_site("web/src/state/chat.ts"), []) is True
    assert _groundable(_site(".github/workflows/ci.yml"), []) is False


def test_a_nearby_patch_caused_rule_makes_a_non_code_file_groundable() -> None:
    finding = AnalyzerFinding(
        analyzer="semgrep",
        rule_id="yaml-rule",
        path="config.yaml",
        line=11,
        patch_caused=True,
    )
    assert _groundable(_site("config.yaml", 10), [finding]) is True
    assert _groundable(_site("config.yaml", 20), [finding]) is False


def test_a_suppressed_rule_is_not_a_grounding_source() -> None:
    finding = AnalyzerFinding(
        analyzer="semgrep",
        rule_id="yaml-rule",
        path="config.yaml",
        line=11,
        patch_caused=True,
        suppressed=True,
    )
    assert _groundable(_site("config.yaml", 10), [finding]) is False


def _candidate(path: str):
    from zero_loop_reviewer.models import Candidate, Evidence, StageId

    return Candidate(
        id="c",
        stage=StageId.PROBE,
        title="a hypothesis",
        evidence=Evidence(kind="llm_hypothesis"),
        cited_sites=[_site(path)],
    )


def test_python_rust_and_typescript_candidates_reach_probe_synthesis() -> None:
    from zero_loop_reviewer.pipeline import _probe_supported

    assert _probe_supported(_candidate("scripts/check.py")) is True
    assert _probe_supported(_candidate("crates/kc-core/src/lib.rs")) is True
    assert _probe_supported(_candidate("web/src/App.tsx")) is True
    assert _probe_supported(_candidate("README.md")) is False
