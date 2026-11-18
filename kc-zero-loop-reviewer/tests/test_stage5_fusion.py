"""Stage 5B: log-odds fusion, the coded-Bayes threshold and the monitor vote."""

from __future__ import annotations

import math

import pytest

from tests.support.llm import ScriptedLLM
from zero_loop_reviewer.config import FusionConfig, LLMConfig, Settings, StageModel
from zero_loop_reviewer.errors import FailureKind, LLMError
from zero_loop_reviewer.models import (
    Candidate,
    Category,
    CitedSite,
    ContextSlice,
    Decision,
    Evidence,
    MonitorScore,
    MonitorVote,
    RiskElicitation,
    StageId,
    Verification,
    VerificationKind,
)
from zero_loop_reviewer.stage5.fusion import (
    combine,
    decide,
    fuse,
    logit,
    monitor_scores,
    monitor_scores_or_none,
    sigmoid,
    tau_of,
)

CONTEXT = ContextSlice(text="src/io.py:42\nreturn stream.read()\n", tokens=12, budget=1000)
DEFAULT = FusionConfig()


def _candidate(candidate_id: str = "cand-1") -> Candidate:
    return Candidate(
        id=candidate_id,
        stage=StageId.SAST,
        category=Category.RESOURCE,
        title="unbounded read",
        root_cause="read() with no size bound",
        severity=2,
        cited_sites=[CitedSite(path="src/io.py", line=42)],
        evidence=Evidence(kind="analyzer", analyzer="semgrep", rule_id="unbounded-read"),
        verification=Verification(kind=VerificationKind.PROBE),
    )


def _elicitation(probability: float) -> RiskElicitation:
    return RiskElicitation(
        defect_present=True,
        category="resource",
        severity_score=2,
        failure_probability=probability,
        cited_sites=[CitedSite(path="src/io.py", line=42)],
        root_cause="read() with no size bound",
    )


def _monitor(score: float) -> MonitorScore:
    return MonitorScore(
        score=score,
        votes=[MonitorVote(score=score)],
        dispersion=0.0,
        model="monitor-x",
        independent=True,
    )


def _settings(votes: int) -> Settings:
    return Settings(llm=LLMConfig(monitor=StageModel(model="monitor-x", temperature=0.8, votes=votes)))


# --------------------------------------------------------------------------- #
# tau and the log-odds arithmetic
# --------------------------------------------------------------------------- #


def test_tau_is_the_cost_ratio() -> None:
    assert tau_of(FusionConfig(c_fr=10, c_fa=1)) == pytest.approx(10 / 11)
    assert tau_of(FusionConfig(c_fr=25, c_fa=5)) == pytest.approx(25 / 30)
    assert tau_of(FusionConfig(c_fr=1, c_fa=1)) == pytest.approx(0.5)
    assert FusionConfig(c_fr=10, c_fa=1).tau == pytest.approx(tau_of(FusionConfig(c_fr=10, c_fa=1)))


def test_tau_refuses_degenerate_costs() -> None:
    with pytest.raises(ValueError):
        tau_of(FusionConfig(c_fr=1, c_fa=0))
    with pytest.raises(ValueError):
        tau_of(FusionConfig(c_fr=0, c_fa=0))


def test_logit_and_sigmoid_are_inverses() -> None:
    for p in (0.01, 0.2, 0.5, 0.8, 0.99):
        assert sigmoid(logit(p)) == pytest.approx(p)


# --------------------------------------------------------------------------- #
# fuse: endpoints, monotonicity, clipping
# --------------------------------------------------------------------------- #


def test_fuse_endpoints_stay_probabilities() -> None:
    floor = fuse(0.0, 0.0, DEFAULT)
    ceiling = fuse(1.0, 1.0, DEFAULT)
    assert 0.0 <= floor < 1e-5
    assert 1.0 >= ceiling > 1.0 - 1e-5
    # symmetric around the prior, not around 0.5
    assert floor + ceiling > 0.99


def test_fuse_is_monotone_in_both_signals() -> None:
    rising = [fuse(p, 0.5, DEFAULT) for p in (0.0, 0.25, 0.5, 0.75, 1.0)]
    assert rising == sorted(rising)
    assert len(set(rising)) == len(rising)
    monitor_rising = [fuse(0.5, s, DEFAULT) for s in (0.0, 0.25, 0.5, 0.75, 1.0)]
    assert monitor_rising == sorted(monitor_rising)


def test_fuse_clips_the_log_odds() -> None:
    clipped = FusionConfig(prior_log_odds=0.0, clip=1.0)
    assert fuse(1.0, 1.0, clipped) == pytest.approx(sigmoid(1.0))
    assert fuse(0.0, 0.0, clipped) == pytest.approx(sigmoid(-1.0))
    # the unclipped sum would be far past the clip
    unclipped = logit(1.0) + 0.5 * logit(1.0)
    assert unclipped > 1.0
    assert fuse(1.0, 1.0, FusionConfig(prior_log_odds=0.0, clip=100.0)) > 0.99


def test_fuse_refuses_a_non_positive_clip() -> None:
    with pytest.raises(ValueError):
        fuse(0.5, 0.5, FusionConfig(clip=0.0))


def test_fuse_weights_scale_the_evidence() -> None:
    heavy_monitor = FusionConfig(monitor_weight=1.0)
    light_monitor = FusionConfig(monitor_weight=0.0)
    assert fuse(0.5, 1.0, heavy_monitor) > fuse(0.5, 1.0, light_monitor)
    assert fuse(0.5, 0.0, heavy_monitor) < fuse(0.5, 0.0, light_monitor)


# --------------------------------------------------------------------------- #
# combine: the three decision rules
# --------------------------------------------------------------------------- #


def test_combine_requests_changes_above_tau() -> None:
    result = combine(0.9999, _monitor(0.99), DEFAULT)
    assert result.tau == pytest.approx(10 / 11)
    assert result.p_combined >= result.tau
    assert result.decision is Decision.REQUEST_CHANGES
    assert result.p == pytest.approx(0.9999)
    assert result.monitor == pytest.approx(0.99)


def test_combine_approves_when_both_signals_are_low_and_agree() -> None:
    result = combine(0.2, _monitor(0.25), DEFAULT)
    assert result.p_combined < result.tau
    assert abs(result.p - result.monitor) <= DEFAULT.disagreement_threshold
    assert result.decision is Decision.APPROVE


def test_combine_sends_a_disagreement_to_a_human() -> None:
    result = combine(0.9, _monitor(0.05), DEFAULT)
    assert result.p_combined < result.tau
    assert abs(result.p - result.monitor) > DEFAULT.disagreement_threshold
    assert result.decision is Decision.HUMAN_REVIEW
    assert any("disagree" in reason for reason in result.reasons)


def test_combine_treats_the_disagreement_boundary_as_agreement() -> None:
    config = FusionConfig(disagreement_threshold=0.5)
    result = combine(0.75, _monitor(0.25), config)
    assert abs(result.p - result.monitor) == pytest.approx(0.5)
    assert result.p_combined < result.tau
    assert result.decision is Decision.APPROVE


def test_combine_treats_the_tau_boundary_as_request_changes() -> None:
    # a config whose fused posterior lands exactly on tau for p == s == 0.5
    tau = tau_of(FusionConfig())
    at = FusionConfig(prior_log_odds=logit(tau), reviewer_weight=0.0, monitor_weight=0.0)
    boundary = combine(0.5, _monitor(0.5), at)
    assert boundary.p_combined == pytest.approx(tau)
    assert boundary.decision is Decision.REQUEST_CHANGES  # >= tau, not >

    below = FusionConfig(
        prior_log_odds=logit(tau) - 1e-3, reviewer_weight=0.0, monitor_weight=0.0
    )
    under = combine(0.5, _monitor(0.5), below)
    assert under.p_combined < tau
    assert under.decision is Decision.APPROVE


# --------------------------------------------------------------------------- #
# decide: the review-level rule
# --------------------------------------------------------------------------- #


def test_decide_mechanical_confirmation_overrides_everything() -> None:
    candidates = [_candidate()]
    decision, p_combined, reasons = decide(
        candidates,
        {"cand-1": _elicitation(0.01)},
        {"cand-1": _monitor(0.02)},
        DEFAULT,
        mechanically_confirmed=True,
    )
    assert decision is Decision.REQUEST_CHANGES
    assert 0.0 <= p_combined <= 1.0
    assert any("mechanical" in reason for reason in reasons)


def test_decide_requests_changes_above_tau_without_mechanics() -> None:
    decision, p_combined, reasons = decide(
        [_candidate()],
        {"cand-1": _elicitation(0.9999)},
        {"cand-1": _monitor(0.99)},
        DEFAULT,
        mechanically_confirmed=False,
    )
    assert decision is Decision.REQUEST_CHANGES
    assert p_combined >= DEFAULT.tau
    assert any("tau" in reason for reason in reasons)


def test_decide_prefers_request_changes_over_disagreement() -> None:
    decision, _, _ = decide(
        [_candidate()],
        {"cand-1": _elicitation(0.999)},
        {"cand-1": _monitor(0.05)},
        DEFAULT,
        mechanically_confirmed=False,
    )
    assert decision is Decision.REQUEST_CHANGES


def test_decide_sends_disagreement_to_a_human() -> None:
    decision, p_combined, reasons = decide(
        [_candidate()],
        {"cand-1": _elicitation(0.9)},
        {"cand-1": _monitor(0.05)},
        DEFAULT,
        mechanically_confirmed=False,
    )
    assert decision is Decision.HUMAN_REVIEW
    assert p_combined < DEFAULT.tau
    assert any("disagreement" in reason for reason in reasons)


def test_decide_approves_when_nothing_crosses_the_bar() -> None:
    decision, p_combined, _ = decide(
        [_candidate()],
        {"cand-1": _elicitation(0.2)},
        {"cand-1": _monitor(0.25)},
        DEFAULT,
        mechanically_confirmed=False,
    )
    assert decision is Decision.APPROVE
    assert p_combined < DEFAULT.tau


def test_decide_never_approves_a_candidate_it_cannot_fuse() -> None:
    decision, _, reasons = decide(
        [_candidate()],
        {},
        {"cand-1": _monitor(0.05)},
        DEFAULT,
        mechanically_confirmed=False,
    )
    assert decision is Decision.HUMAN_REVIEW
    assert any("no elicitation" in reason for reason in reasons)

    decision, _, reasons = decide(
        [_candidate()],
        {"cand-1": _elicitation(0.05)},
        {},
        DEFAULT,
        mechanically_confirmed=False,
    )
    assert decision is Decision.HUMAN_REVIEW
    assert any("no monitor score" in reason for reason in reasons)


def test_decide_with_no_candidates_reports_the_prior() -> None:
    decision, p_combined, _ = decide([], {}, {}, DEFAULT, mechanically_confirmed=False)
    assert decision is Decision.APPROVE
    assert p_combined == pytest.approx(sigmoid(DEFAULT.prior_log_odds))
    assert 0.0 < p_combined < DEFAULT.tau


def test_decide_is_deterministic() -> None:
    first = decide([_candidate()], {"cand-1": _elicitation(0.4)}, {"cand-1": _monitor(0.45)}, DEFAULT,
                   mechanically_confirmed=False)
    second = decide([_candidate()], {"cand-1": _elicitation(0.4)}, {"cand-1": _monitor(0.45)}, DEFAULT,
                    mechanically_confirmed=False)
    assert first == second


# --------------------------------------------------------------------------- #
# monitor_scores: k votes, median, dispersion, independence
# --------------------------------------------------------------------------- #


def test_monitor_scores_uses_the_median_and_half_the_range() -> None:
    scores = iter([0.1, 0.9, 0.3])
    monitor_llm = ScriptedLLM(
        answers={"monitor": [lambda prompt, schema: {"score": next(scores), "rationale": "looked"} for _ in range(3)]}
    )
    reviewer = ScriptedLLM()
    result = monitor_scores(_candidate(), CONTEXT, reviewer, monitor_llm, _settings(votes=3))

    assert result.score == pytest.approx(0.3)
    assert result.dispersion == pytest.approx((0.9 - 0.1) / 2)
    assert [vote.score for vote in result.votes] == [0.1, 0.9, 0.3]
    assert len(result.votes) == 3
    assert result.model == "monitor-x"
    assert result.independent is True
    assert len(monitor_llm.calls) == 3
    assert {call["instance"] for call in monitor_llm.calls} == {"monitor"}
    assert reviewer.calls == []  # the reviewer transport is never asked


def test_monitor_scores_defaults_to_one_vote_without_settings() -> None:
    monitor_llm = ScriptedLLM(answers={"monitor": [{"score": 0.7}]})
    result = monitor_scores(_candidate(), CONTEXT, ScriptedLLM(), monitor_llm)
    assert len(result.votes) == 1
    assert result.score == pytest.approx(0.7)
    assert result.dispersion == pytest.approx(0.0)


def test_monitor_scores_are_not_independent_when_the_client_is_shared() -> None:
    shared = ScriptedLLM(answers={"monitor": [{"score": 0.5}]})
    result = monitor_scores(_candidate(), CONTEXT, shared, shared, _settings(votes=1))
    assert result.independent is False


def test_monitor_scores_raises_without_a_monitor_transport() -> None:
    with pytest.raises(LLMError) as excinfo:
        monitor_scores(_candidate(), CONTEXT, ScriptedLLM(), None, _settings(votes=1))
    assert excinfo.value.kind is FailureKind.NOT_CONFIGURED


def test_monitor_scores_raises_when_a_vote_fails() -> None:
    monitor_llm = ScriptedLLM(
        answers={"monitor": [{"score": 0.4}, LLMError("timed out", kind=FailureKind.TIMEOUT)]}
    )
    with pytest.raises(LLMError) as excinfo:
        monitor_scores(_candidate(), CONTEXT, ScriptedLLM(), monitor_llm, _settings(votes=2))
    assert excinfo.value.kind is FailureKind.TIMEOUT
    assert len(monitor_llm.calls) == 2


def test_monitor_scores_or_none_degrades_instead_of_raising() -> None:
    monitor_llm = ScriptedLLM(answers={"monitor": [LLMError("timed out", kind=FailureKind.TIMEOUT)]})
    score, reason = monitor_scores_or_none(
        _candidate(), CONTEXT, ScriptedLLM(), monitor_llm, _settings(votes=1)
    )
    assert score is None
    assert reason.startswith("degraded:")
    assert "human review" in reason
    assert "timeout" in reason


def test_monitor_scores_or_none_passes_a_good_score_through() -> None:
    monitor_llm = ScriptedLLM(answers={"monitor": [{"score": 0.42}]})
    score, reason = monitor_scores_or_none(
        _candidate(), CONTEXT, ScriptedLLM(), monitor_llm, _settings(votes=1)
    )
    assert reason == ""
    assert score is not None
    assert score.score == pytest.approx(0.42)
    assert math.isclose(score.dispersion, 0.0)
