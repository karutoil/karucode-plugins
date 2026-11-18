"""Stage 2B: reconciliation tiers, candidates and the security monologue call."""

from __future__ import annotations

import pytest

from tests.support.gitfix import make_repo
from tests.support.llm import ScriptedLLM
from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.errors import FailureKind, LLMError
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import (
    AnalyzerFinding,
    Candidate,
    ContextSlice,
    FindingStatus,
    ReconciliationTier,
    SecSemScan,
    StageId,
    VerificationKind,
)
from zero_loop_reviewer.stage2.secsem import HIGH_CONFIDENCE, reconcile, run_security, to_candidates

BASE_FILE = "def f(v):\n    return v\n"
HEAD_FILE = "import subprocess\n\n\ndef run(cmd):\n    subprocess.run(cmd, shell=True)\n    return 1\n"
SHELL_LINE = 5
SHELL_RULE = "zero-loop.subprocess-shell-true"


@pytest.fixture(scope="module")
def diff(tmp_path_factory):
    root = tmp_path_factory.mktemp("secsem")
    fixture = make_repo(root, "repo")
    base = fixture.commit("base", **{"pkg__a.py": BASE_FILE})
    head = fixture.commit("head", **{"pkg__vuln.py": HEAD_FILE})
    repo = GitRepo(fixture.root)
    return repo.diff(base, head)


def _context() -> ContextSlice:
    return ContextSlice(text="changed: pkg/vuln.py", tokens=6, budget=1000)


def _finding(
    line: int = SHELL_LINE,
    *,
    rule_id: str = SHELL_RULE,
    cwe: str = "CWE-78",
    patch_caused: bool = True,
    suppressed: bool = False,
) -> AnalyzerFinding:
    return AnalyzerFinding(
        analyzer="semgrep",
        rule_id=rule_id,
        path="pkg/vuln.py",
        line=line,
        end_line=line,
        cwe=cwe,
        track="taint",
        severity=3,
        patch_caused=patch_caused,
        suppressed=suppressed,
        suppression_reason="test" if suppressed else "",
    )


def _scan(**overrides) -> SecSemScan:
    data = {
        "monologue": (
            "cmd comes from the caller and reaches subprocess.run with shell=True at "
            "pkg/vuln.py:5; no quoting or allow-list sits between them."
        ),
        "cwe": "CWE-78",
        "verdict": "VULNERABLE",
        "cited_lines": [{"path": "pkg/vuln.py", "line": SHELL_LINE}],
        "confidence": 0.9,
    }
    data.update(overrides)
    return SecSemScan.model_validate(data)


# --------------------------------------------------------------------------- #
# reconcile
# --------------------------------------------------------------------------- #


def test_tier1_requires_every_cited_line_in_the_trace(diff):
    reconciliation = reconcile(_scan(), [_finding()], diff)
    assert reconciliation.tier is ReconciliationTier.TIER1_ANALYZER
    assert reconciliation.matched_lines == [SHELL_LINE]
    assert "matches the analyzer trace" in reconciliation.detail


def test_tier1_tolerates_two_lines_of_offset(diff):
    reconciliation = reconcile(_scan(cited_lines=[{"path": "pkg/vuln.py", "line": SHELL_LINE + 2}]), [_finding()], diff)
    assert reconciliation.tier is ReconciliationTier.TIER1_ANALYZER


def test_one_unmatched_cited_line_rejects_the_scan(diff):
    scan = _scan(
        cited_lines=[{"path": "pkg/vuln.py", "line": SHELL_LINE}, {"path": "pkg/vuln.py", "line": 40}]
    )
    reconciliation = reconcile(scan, [_finding()], diff)
    assert reconciliation.tier is ReconciliationTier.REJECTED
    assert reconciliation.matched_lines == [SHELL_LINE]


def test_a_rule_that_did_not_fire_on_the_patch_falls_back_to_unverified_trust(diff):
    # A pre-existing or suppressed analyzer hit is not evidence about the patch,
    # so the scan cannot be tier-1; a confident, in-patch citation is trusted
    # only as UNVERIFIED_TRUST.
    assert reconcile(_scan(), [_finding(patch_caused=False)], diff).tier is ReconciliationTier.UNVERIFIED_TRUST
    assert reconcile(_scan(), [_finding(suppressed=True)], diff).tier is ReconciliationTier.UNVERIFIED_TRUST


def test_a_rule_that_fired_elsewhere_in_the_patch_rejects_an_unmatched_claim(diff):
    elsewhere = _finding(line=SHELL_LINE + 20)
    reconciliation = reconcile(_scan(), [elsewhere], diff)
    assert reconciliation.tier is ReconciliationTier.REJECTED
    assert "match the analyzer trace" in reconciliation.detail


def test_no_rule_high_confidence_is_unverified_trust(diff):
    reconciliation = reconcile(_scan(cwe="CWE-94", confidence=HIGH_CONFIDENCE + 0.05), [], diff)
    assert reconciliation.tier is ReconciliationTier.UNVERIFIED_TRUST
    assert "must reach Stage 3/4" in reconciliation.detail


def test_no_rule_low_confidence_is_rejected(diff):
    reconciliation = reconcile(_scan(confidence=0.4), [], diff)
    assert reconciliation.tier is ReconciliationTier.REJECTED


def test_no_rule_with_a_line_outside_the_patch_is_rejected(diff):
    reconciliation = reconcile(_scan(cited_lines=[{"path": "pkg/vuln.py", "line": 400}]), [], diff)
    assert reconciliation.tier is ReconciliationTier.REJECTED
    assert "outside the patch" in reconciliation.detail


def test_safe_verdict_is_recorded_but_never_becomes_a_candidate(diff):
    scan = _scan(verdict="SAFE", confidence=0.2)
    reconciliation = reconcile(scan, [_finding()], diff)
    assert reconciliation.tier is ReconciliationTier.REJECTED
    assert "SAFE" in reconciliation.detail
    assert to_candidates(scan, reconciliation, [], diff) == []


def test_vulnerable_without_citations_is_rejected(diff):
    reconciliation = reconcile(_scan(cited_lines=[]), [_finding()], diff)
    assert reconciliation.tier is ReconciliationTier.REJECTED
    assert "cited_lines" in reconciliation.detail


# --------------------------------------------------------------------------- #
# to_candidates
# --------------------------------------------------------------------------- #


def test_tier1_candidate_is_an_analyzer_evidence_2b_candidate(diff):
    scan = _scan()
    reconciliation = reconcile(scan, [_finding()], diff)
    candidates = to_candidates(scan, reconciliation, [], diff)
    assert len(candidates) == 1
    candidate = candidates[0]
    assert isinstance(candidate, Candidate)
    assert candidate.stage is StageId.SECSEM
    assert candidate.evidence.kind == "analyzer"
    assert candidate.evidence.analyzer == "secsem"
    assert candidate.evidence.trace == scan.cited_lines
    assert candidate.verification.kind is VerificationKind.ANALYZER_TRACE
    assert candidate.status is FindingStatus.VERIFIED
    assert candidate.cwe == "CWE-78"
    assert candidate.severity == 3
    assert candidate.p == pytest.approx(0.9)
    assert candidate.security_exploitable is True
    assert candidate.public_safe is False
    assert candidate.id.startswith("2b-")


def test_unverified_trust_candidate_is_marked_unverified(diff):
    scan = _scan(cwe="CWE-94", confidence=0.95)
    reconciliation = reconcile(scan, [], diff)
    candidates = to_candidates(scan, reconciliation, [], diff)
    assert len(candidates) == 1
    assert candidates[0].verification.kind is VerificationKind.UNVERIFIED
    assert candidates[0].status is FindingStatus.UNVERIFIED


def test_candidates_are_deterministic_and_ignore_sites_outside_the_diff(diff):
    scan = _scan()
    reconciliation = reconcile(scan, [_finding()], diff)
    first = to_candidates(scan, reconciliation, [], diff)
    second = to_candidates(scan, reconciliation, [], diff)
    assert [c.id for c in first] == [c.id for c in second]
    outside = _scan(cited_lines=[{"path": "pkg/a.py", "line": 1}])
    assert to_candidates(outside, reconcile(outside, [], diff), [], diff) == []


# --------------------------------------------------------------------------- #
# run_security
# --------------------------------------------------------------------------- #


def test_run_security_uses_the_security_prompt_and_returns_a_candidate(diff):
    llm = ScriptedLLM(answers={"security": [_scan()]})
    scan, reconciliation, candidates, detail = run_security(
        diff, _context(), [_finding()], [], llm, Settings(), policy={"rules": []}
    )
    assert scan is not None and scan.verdict == "VULNERABLE"
    assert reconciliation is not None and reconciliation.tier is ReconciliationTier.TIER1_ANALYZER
    assert len(candidates) == 1
    assert "tier1_analyzer" in detail
    assert llm.calls and llm.calls[0]["stage"] == "security"
    assert llm.calls[0]["schema"] == "SecSemScan"


def test_run_security_records_safe_scans_with_no_candidates(diff):
    llm = ScriptedLLM(answers={"security": [_scan(verdict="SAFE", confidence=0.1)]})
    scan, reconciliation, candidates, _detail = run_security(
        diff, _context(), [_finding()], [], llm, Settings(), policy=None
    )
    assert scan is not None
    assert reconciliation is not None
    assert candidates == []


def test_run_security_degrades_on_llm_failure(diff):
    llm = ScriptedLLM(answers={"security": [LLMError("endpoint down", kind=FailureKind.TIMEOUT)]})
    scan, reconciliation, candidates, detail = run_security(
        diff, _context(), [_finding()], [], llm, Settings(), policy={"rules": []}
    )
    assert (scan, reconciliation, candidates) == (None, None, [])
    assert "timeout" in detail and "endpoint down" in detail


def test_run_security_degrades_on_a_malformed_answer(diff):
    llm = ScriptedLLM(answers={"security": [{"cwe": "CWE-78", "verdict": "VULNERABLE"}]})
    scan, reconciliation, candidates, detail = run_security(diff, _context(), [], [], llm, Settings(), policy=None)
    assert (scan, reconciliation, candidates) == (None, None, [])
    assert "not a SecSemScan" in detail


def test_run_security_degrades_on_a_transport_exception(diff):
    llm = ScriptedLLM(answers={"security": [RuntimeError("socket closed")]})
    scan, _reconciliation, candidates, detail = run_security(diff, _context(), [], [], llm, Settings(), policy=None)
    assert scan is None and candidates == []
    assert "RuntimeError" in detail and "socket closed" in detail
