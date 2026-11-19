"""Stage 5D: publication cap, verified suggestions only, one consolidated review."""

from __future__ import annotations

import pytest

from zero_loop_reviewer.config import FusionConfig
from zero_loop_reviewer.models import (
    Candidate,
    Category,
    CitedSite,
    Decision,
    Evidence,
    Finding,
    FindingStatus,
    Fix,
    MonitorScore,
    MonitorVote,
    ProbeClassification,
    ProbePair,
    ProbeResult,
    ProbeStatus,
    Redaction,
    ReviewVerdict,
    RiskElicitation,
    StageId,
    Verification,
    VerificationKind,
)
from zero_loop_reviewer.providers.base import ReviewPayload
from zero_loop_reviewer.stage5.formatter import (
    MAX_FINDINGS,
    is_redacted_flag,
    publishability,
    render_review,
    select,
    select_with_receipts,
    suggestion_block,
    suggestion_range,
    to_finding,
)

PATCH = """--- a/src/io.py
+++ b/src/io.py
@@ -40,3 +40,3 @@
-    return stream.read()
+    return stream.read(MAX_BYTES)
"""

TAU = FusionConfig().tau


def _candidate(
    candidate_id: str,
    *,
    severity: int = 2,
    p: float = 0.5,
    status: FindingStatus = FindingStatus.VERIFIED,
    fix: str = "verified",
    line: int = 10,
    path: str = "src/io.py",
    security: bool = False,
) -> Candidate:
    verified_fix: Fix | None = None
    if fix == "verified":
        verified_fix = Fix(patch=PATCH, files_changed=[path], verified=True, compile_ok=True)
    elif fix == "unverified":
        verified_fix = Fix(patch=PATCH, files_changed=[path], verified=False)
    return Candidate(
        id=candidate_id,
        stage=StageId.FALSIFY,
        category=Category.SECURITY if security else Category.RESOURCE,
        title=f"defect {candidate_id}",
        root_cause=f"root cause of {candidate_id}",
        severity=severity,
        cited_sites=[CitedSite(path=path, line=line, symbol="read_all")],
        evidence=Evidence(
            kind="analyzer",
            analyzer="semgrep",
            rule_id="python.lang.security.taint-mode.untrusted-input" if security else "unbounded-read",
        ),
        verification=Verification(kind=VerificationKind.PROBE),
        status=status,
        p=p,
        p_combined=p,
        verified_fix=verified_fix,
        security_exploitable=security,
        public_safe=not security,
    )


def _elicitation(probability: float) -> RiskElicitation:
    return RiskElicitation(
        defect_present=True,
        category="resource",
        severity_score=2,
        failure_probability=probability,
        cited_sites=[CitedSite(path="src/io.py", line=10)],
        root_cause="unbounded read",
    )


def _monitor(score: float) -> MonitorScore:
    return MonitorScore(
        score=score,
        votes=[MonitorVote(score=score)],
        dispersion=0.0,
        model="monitor-x",
        independent=True,
    )


def _verdict(findings: list[Finding], *, decision: Decision = Decision.REQUEST_CHANGES, degraded=None,
             suppressed: int = 0, redacted: int = 0, p_combined: float = 0.99) -> ReviewVerdict:
    return ReviewVerdict(
        review_id="rev-1",
        repository="acme/widgets",
        pull_number=7,
        base_sha="a" * 40,
        head_sha="b" * 40,
        decision=decision,
        p_combined=p_combined,
        tau=TAU,
        findings=findings,
        suppressed_count=suppressed,
        redacted_count=redacted,
        degraded=list(degraded or []),
    )


# --------------------------------------------------------------------------- #
# Selection: ranking, the hard cap, and the publishability bar
# --------------------------------------------------------------------------- #


def test_select_ranks_by_severity_then_probability() -> None:
    candidates = [
        _candidate("low-probable", severity=1, p=0.95),
        _candidate("high-low-probability", severity=3, p=0.55),
        _candidate("high-probable", severity=3, p=0.99),
        _candidate("medium", severity=2, p=0.99),
        _candidate("cosmetic", severity=0, p=1.0),
    ]
    assert [c.id for c in select(candidates, 5)] == [
        "high-probable",
        "high-low-probability",
        "medium",
        "low-probable",
        "cosmetic",
    ]
    assert MAX_FINDINGS == 50


def test_select_cannot_be_asked_for_more_than_fifty() -> None:
    candidates = [_candidate(f"c-{index}") for index in range(55)]
    assert len(select(candidates, 100)) == 50
    assert len(select(candidates, 0)) == 50


def test_zero_means_all_verified_findings_up_to_the_safety_ceiling() -> None:
    candidates = [_candidate(f"c-{index}") for index in range(20)]
    assert len(select(candidates, 0)) == 20


def test_a_finding_without_a_verified_fix_is_suppressed_with_a_reason() -> None:
    published, suppressed = select_with_receipts([_candidate("no-fix", fix="none")], 3)
    assert published == []
    assert len(suppressed) == 1
    assert suppressed[0].status is FindingStatus.SUPPRESSED
    assert "no verified fix" in suppressed[0].suppression_reason


def test_an_unverified_fix_is_suppressed_with_a_reason() -> None:
    published, suppressed = select_with_receipts([_candidate("weak-fix", fix="unverified")], 3)
    assert published == []
    assert "did not pass CIVER verification" in suppressed[0].suppression_reason


def test_an_unverified_candidate_is_never_published() -> None:
    keep, reason = publishability(_candidate("unverified", status=FindingStatus.UNVERIFIED))
    assert keep is False
    assert "status=unverified" in reason


def test_a_redacted_security_flag_needs_no_fix() -> None:
    candidate = _candidate("sec", fix="none", security=True)
    assert is_redacted_flag(candidate) is True
    keep, reason = publishability(candidate)
    assert keep is True
    assert reason == ""


def test_the_cap_overflow_is_suppressed_with_a_receipt() -> None:
    candidates = [_candidate(f"c-{index}", severity=index % 4) for index in range(55)]
    published, suppressed = select_with_receipts(candidates, 55)
    assert len(published) == 50
    assert len(suppressed) == 5
    assert all("publication cap" in candidate.suppression_reason for candidate in suppressed)
    assert all(candidate.status is FindingStatus.SUPPRESSED for candidate in suppressed)


# --------------------------------------------------------------------------- #
# Finding conversion and the suggestion block
# --------------------------------------------------------------------------- #


def test_suggestion_block_wraps_the_verified_patch() -> None:
    fix = Fix(patch=PATCH, verified=True)
    block = suggestion_block(fix)
    assert block.startswith("```suggestion\n")
    assert block.endswith("\n```")
    # The post-image line is what a one-click suggestion must contain; diff
    # headers would not be committable from a suggestion fence.
    assert "return stream.read(MAX_BYTES)" in block
    assert "+++ b/" not in block
    assert suggestion_range(fix) == (40, 42)


def test_multi_hunk_patch_is_rendered_as_an_applyable_diff() -> None:
    patch = PATCH + """@@ -80,1 +80,2 @@
-old
+new
+more
"""
    block = suggestion_block(Fix(patch=patch, verified=True))
    assert block.startswith("```diff\n")
    assert "+++ b/src/io.py" in block
    assert suggestion_range(Fix(patch=patch, verified=True)) == (None, None)


def test_suggestion_block_refuses_a_fence_in_the_patch() -> None:
    with pytest.raises(ValueError):
        suggestion_block(Fix(patch="--- a\n+++ b\n```\n", verified=True))


def test_to_finding_carries_the_verified_patch_and_the_probabilities() -> None:
    candidate = _candidate("c-1", p=0.5)
    finding = to_finding(candidate, _elicitation(0.8), _monitor(0.7), 0.93, Redaction())

    assert finding.id == "c-1"
    assert finding.title == "defect c-1"
    assert finding.failure_probability == pytest.approx(0.8)
    assert finding.p_combined == pytest.approx(0.93)
    assert finding.verification is VerificationKind.PROBE
    assert finding.suggestion == suggestion_block(candidate.verified_fix)
    assert finding.suggestion_patch == PATCH
    assert finding.redacted is False
    assert finding.private_detail_id == ""
    assert [site.line for site in finding.cited_sites] == [10]
    assert "monitor 0.70" in finding.evidence_summary


def test_to_finding_hides_a_redacted_security_finding(tmp_path) -> None:
    from zero_loop_reviewer.config import RedactionConfig
    from zero_loop_reviewer.stage5.redaction import redact

    candidate = _candidate("sec-1", security=True, fix="none")
    redaction = redact(candidate, RedactionConfig(), private_dir=tmp_path / "private")
    finding = to_finding(candidate, _elicitation(0.9), _monitor(0.9), 0.99, redaction)

    assert finding.redacted is True
    assert finding.private_detail_id == redaction.private_record_id
    assert finding.cited_sites == []
    assert finding.suggestion == ""
    assert finding.suggestion_patch == ""
    assert "security gate held" in finding.root_cause
    assert "security gate held" in finding.title


# --------------------------------------------------------------------------- #
# Rendering
# --------------------------------------------------------------------------- #


def test_render_review_is_one_payload_with_at_most_three_comments() -> None:
    findings = [
        to_finding(_candidate(f"c-{index}", line=10 + index, fix="none"), _elicitation(0.8), _monitor(0.75), 0.93, Redaction())
        for index in range(2)
    ]
    findings.append(
        to_finding(_candidate("c-2", line=42), _elicitation(0.8), _monitor(0.75), 0.93, Redaction())
    )
    verdict = _verdict(findings, degraded=["stage 3: sandbox unavailable"], suppressed=2)
    payload = render_review(verdict, bot_login="zero-loop-reviewer[bot]")

    assert isinstance(payload, ReviewPayload)
    assert len(payload.comments) == 3
    assert payload.event == "REQUEST_CHANGES"
    assert "request_changes" in payload.body
    assert "stage 3: sandbox unavailable" in payload.body
    assert "Degraded modes" in payload.body
    assert "zero-loop-reviewer[bot]" in payload.body
    assert [comment.line for comment in payload.comments] == [10, 11, 42]
    assert {comment.path for comment in payload.comments} == {"src/io.py"}

    first = payload.comments[0]
    assert "defect c-0" in first.body
    last = payload.comments[-1]
    assert "```suggestion" in last.body
    assert "return stream.read(MAX_BYTES)" in last.body
    assert last.start_line == 40 and last.line == 42


def test_three_identical_suggestion_ranges_merge_into_one_comment() -> None:
    findings = [
        to_finding(_candidate(f"c-{index}", line=10 + index), _elicitation(0.8), _monitor(0.75), 0.93, Redaction())
        for index in range(3)
    ]
    payload = render_review(_verdict(findings), bot_login="bot")
    assert len(payload.comments) == 1
    body = payload.comments[0].body
    assert all(f"defect c-{index}" in body for index in range(3))


def test_render_review_has_no_threads_and_never_uses_raw_author_text() -> None:
    finding = to_finding(_candidate("c-1"), _elicitation(0.8), _monitor(0.75), 0.93, Redaction())
    payload = render_review(_verdict([finding]), bot_login="bot")
    assert payload.comments and payload.body
    assert payload.body.count("## Automated review") == 1  # one consolidated review
    # the summary states that author text is converted to claims, not copied
    assert "Pull-request text is read only as untrusted intent" in payload.body
    assert "raw author text is never used as instructions" in payload.body
    assert "PR description:" not in payload.body


def test_render_review_skips_an_inline_comment_for_a_redacted_finding(tmp_path) -> None:
    from zero_loop_reviewer.config import RedactionConfig
    from zero_loop_reviewer.stage5.redaction import redact

    candidate = _candidate("sec-1", security=True, fix="none")
    redaction = redact(candidate, RedactionConfig(), private_dir=tmp_path / "private")
    finding = to_finding(candidate, _elicitation(0.9), _monitor(0.9), 0.99, redaction)
    payload = render_review(_verdict([finding], redacted=1), bot_login="bot")

    assert payload.comments == []
    assert "security gate held" in payload.body
    assert "CWE" in payload.body
    assert PATCH not in payload.body


def test_render_review_merges_two_findings_on_the_same_line() -> None:
    first = to_finding(_candidate("c-1", line=10), _elicitation(0.8), _monitor(0.75), 0.93, Redaction())
    second = to_finding(_candidate("c-2", line=10), _elicitation(0.7), _monitor(0.7), 0.9, Redaction())
    payload = render_review(_verdict([first, second]), bot_login="bot")
    assert len(payload.comments) == 1
    assert "defect c-1" in payload.comments[0].body
    assert "defect c-2" in payload.comments[0].body


def test_render_review_maps_each_decision_to_an_event() -> None:
    finding = to_finding(_candidate("c-1"), _elicitation(0.2), _monitor(0.2), 0.1, Redaction())
    assert render_review(_verdict([finding], decision=Decision.APPROVE), bot_login="bot").event == "APPROVE"
    assert render_review(_verdict([finding], decision=Decision.HUMAN_REVIEW), bot_login="bot").event == "COMMENT"
    assert (
        render_review(_verdict([finding], decision=Decision.REQUEST_CHANGES), bot_login="bot").event
        == "REQUEST_CHANGES"
    )


def test_render_review_states_that_nothing_degraded() -> None:
    payload = render_review(_verdict([], decision=Decision.APPROVE), bot_login="bot")
    assert "Degraded modes" in payload.body
    assert "- none" in payload.body
    assert payload.comments == []


def test_render_review_states_the_decision_basis() -> None:
    finding = to_finding(_candidate("c-1"), _elicitation(0.2), _monitor(0.2), 0.1, Redaction())
    mechanical = render_review(_verdict([finding], p_combined=0.2), bot_login="bot")
    assert "Decision basis: a verified finding is published" in mechanical.body
    crossed = render_review(_verdict([finding], p_combined=0.99), bot_login="bot")
    assert "crossed the gate" in crossed.body
    human = render_review(_verdict([finding], decision=Decision.HUMAN_REVIEW), bot_login="bot")
    assert "disagreed, or a candidate could not be fused" in human.body
    approved = render_review(_verdict([finding], decision=Decision.APPROVE, p_combined=0.1), bot_login="bot")
    assert "monitor agreed" in approved.body


def test_render_review_puts_an_uncited_finding_in_the_body() -> None:
    finding = to_finding(_candidate("c-1"), _elicitation(0.8), _monitor(0.75), 0.93, Redaction())
    uncited = finding.model_copy(update={"cited_sites": []})
    payload = render_review(_verdict([uncited]), bot_login="bot")
    assert payload.comments == []
    assert "defect c-1" in payload.body
    assert "root cause of c-1" in payload.body


def test_render_review_bounds_the_body() -> None:
    finding = to_finding(_candidate("c-1"), _elicitation(0.8), _monitor(0.75), 0.93, Redaction())
    payload = render_review(_verdict([finding]), bot_login="bot", max_body=200)
    assert len(payload.body) <= 200
    assert "truncated" in payload.body


def test_an_advisory_both_fail_finding_is_published_without_a_fix() -> None:
    candidate = _candidate("advisory", status=FindingStatus.ADVISORY, fix="none")
    keep, reason = publishability(candidate)
    assert keep is True
    assert reason == ""
    published, suppressed = select_with_receipts([candidate], 10)
    assert published == [candidate]
    assert suppressed == []


def test_a_both_fail_finding_carries_the_test_repair_note() -> None:
    candidate = _candidate("advisory", status=FindingStatus.ADVISORY, fix="none")
    candidate = candidate.model_copy(
        update={
            "verification": Verification(
                kind=VerificationKind.PROBE,
                probe=ProbePair(
                    base=ProbeResult(status=ProbeStatus.FAIL_BEHAVIORAL),
                    head=ProbeResult(status=ProbeStatus.FAIL_BEHAVIORAL),
                    classification=ProbeClassification.BOTH_FAIL,
                ),
            )
        }
    )
    finding = to_finding(candidate, _elicitation(0.5), _monitor(0.5), 0.5, Redaction())
    assert "tests need repair" in finding.evidence_summary
    assert finding.suggestion == ""
