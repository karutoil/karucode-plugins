"""Stage 5 — independent monitor, coded-Bayes fusion and the decision rule.

Everything here is deterministic and pure: no I/O, no clock, no model.  The
only stochastic part is :func:`monitor_scores`, which asks a *separate*
transport k times and folds the votes into a robust score.

The decision is host policy, never a model opinion::

    tau       = c_fr / (c_fa + c_fr)
    log odds  = prior_log_odds + reviewer_weight*logit(p) + monitor_weight*logit(S)
    p_combined = sigmoid(clip(log odds, +/-config.clip))

    mechanically confirmed                  -> REQUEST_CHANGES
    p_combined >= tau                       -> REQUEST_CHANGES
    |p - S| > disagreement_threshold        -> HUMAN_REVIEW
    otherwise                               -> APPROVE
"""

from __future__ import annotations

import math
import statistics

from pydantic import ValidationError

from ..config import FusionConfig, Settings
from ..errors import FailureKind, LLMError
from ..llm.prompts import monitor_prompt
from ..models import (
    Candidate,
    ContextSlice,
    Decision,
    FusionResult,
    MonitorScore,
    MonitorVote,
    RiskElicitation,
)

#: logit() is undefined at 0 and 1; a 1e-6 flooring keeps the arithmetic finite
#: without letting an extreme elicitation escape the clip.
_LOG_ODDS_EPS = 1e-6

#: A monitor with no votes is not a monitor.
MIN_VOTES = 1


# --------------------------------------------------------------------------- #
# Log-odds arithmetic
# --------------------------------------------------------------------------- #


def logit(p: float) -> float:
    """``ln(p/(1-p))`` with the endpoints floored to a finite value."""
    value = min(max(float(p), _LOG_ODDS_EPS), 1.0 - _LOG_ODDS_EPS)
    return math.log(value / (1.0 - value))


def sigmoid(z: float) -> float:
    """Numerically stable logistic function."""
    z = float(z)
    if z >= 0.0:
        return 1.0 / (1.0 + math.exp(-z))
    exp_z = math.exp(z)
    return exp_z / (1.0 + exp_z)


def tau_of(config: FusionConfig) -> float:
    """``c_fr / (c_fa + c_fr)``: the posterior at which a rejection is cheaper."""
    c_fr = float(config.c_fr)
    c_fa = float(config.c_fa)
    total = c_fa + c_fr
    if total <= 0.0:
        raise ValueError("fusion costs must sum to a positive number")
    tau = c_fr / total
    if not 0.0 < tau < 1.0:
        raise ValueError("fusion costs must yield 0 < tau < 1")
    return tau


def fuse(p: float, s: float, config: FusionConfig) -> float:
    """Combine the elicited probability ``p`` and the monitor score ``s``.

    Independent log-odds are added, the sum is clipped to ``+/-config.clip``
    and mapped back through the logistic function, so the result is always a
    probability in ``[0, 1]``.
    """
    clip = float(config.clip)
    if clip <= 0.0:
        raise ValueError("config.clip must be positive")
    total = (
        float(config.prior_log_odds)
        + float(config.reviewer_weight) * logit(p)
        + float(config.monitor_weight) * logit(s)
    )
    return sigmoid(max(-clip, min(clip, total)))


# --------------------------------------------------------------------------- #
# One candidate
# --------------------------------------------------------------------------- #


def combine(p: float, monitor: MonitorScore, config: FusionConfig) -> FusionResult:
    """Fuse one elicitation with one monitor score and classify it."""
    p = float(p)
    s = float(monitor.score)
    p_combined = fuse(p, s, config)
    tau = tau_of(config)
    disagreement = abs(p - s)
    reasons = [
        f"p={p:.4f} monitor={s:.4f} p_combined={p_combined:.4f} tau={tau:.4f}",
        f"monitor dispersion={monitor.dispersion:.4f} votes={len(monitor.votes)}",
    ]
    if p_combined >= tau:
        reasons.append("the combined probability crosses the coded-Bayes threshold")
        decision = Decision.REQUEST_CHANGES
    elif disagreement > float(config.disagreement_threshold):
        reasons.append(
            f"the reviewer and the monitor disagree by {disagreement:.4f}, "
            f"more than {float(config.disagreement_threshold):.4f}"
        )
        decision = Decision.HUMAN_REVIEW
    else:
        reasons.append("the combined probability stays below the threshold and the monitor agrees")
        decision = Decision.APPROVE
    return FusionResult(
        p=min(max(p, 0.0), 1.0),
        monitor=min(max(s, 0.0), 1.0),
        p_combined=p_combined,
        tau=tau,
        decision=decision,
        reasons=reasons,
    )


# --------------------------------------------------------------------------- #
# The review-level decision
# --------------------------------------------------------------------------- #


def decide(
    candidates: list[Candidate],
    elicitations: dict[str, RiskElicitation],
    monitors: dict[str, MonitorScore],
    config: FusionConfig,
    *,
    mechanically_confirmed: bool,
) -> tuple[Decision, float, list[str]]:
    """Fold every candidate into one decision, the worst combined p and reasons.

    ``p_combined`` is the worst (largest) candidate posterior; with no
    candidates it is the prior alone, so an empty review carries the prior
    rather than a fabricated zero.  A candidate that has no elicitation or no
    monitor score cannot be fused, and an unfusable candidate is never
    approved: the review goes to a human.
    """
    tau = tau_of(config)
    reasons: list[str] = []
    p_worst = fuse(0.5, 0.5, config)  # prior only: logit(0.5) == 0
    missing: list[str] = []
    disagreements: list[str] = []

    for candidate in candidates:
        elicitation = elicitations.get(candidate.id)
        monitor = monitors.get(candidate.id)
        if elicitation is None or monitor is None:
            absent = "elicitation" if elicitation is None else "monitor score"
            missing.append(candidate.id)
            reasons.append(f"{candidate.id}: no {absent}; the candidate cannot be fused")
            continue
        result = combine(elicitation.failure_probability, monitor, config)
        p_worst = max(p_worst, result.p_combined)
        reasons.append(
            f"{candidate.id}: p={elicitation.failure_probability:.4f} "
            f"monitor={monitor.score:.4f} p_combined={result.p_combined:.4f} "
            f"({result.decision.value})"
        )
        if abs(float(elicitation.failure_probability) - float(monitor.score)) > float(
            config.disagreement_threshold
        ):
            disagreements.append(candidate.id)

    if mechanically_confirmed:
        reasons.append("a mechanical check (probe or counterfactual) confirmed the defect")
        return Decision.REQUEST_CHANGES, p_worst, reasons
    if p_worst >= tau:
        reasons.append(f"worst p_combined {p_worst:.4f} >= tau {tau:.4f}")
        return Decision.REQUEST_CHANGES, p_worst, reasons
    if missing:
        reasons.append(
            "unfusable candidate(s): " + ", ".join(missing) + "; a human decides rather than approving on silence"
        )
        return Decision.HUMAN_REVIEW, p_worst, reasons
    if disagreements:
        reasons.append("reviewer/monitor disagreement on: " + ", ".join(disagreements))
        return Decision.HUMAN_REVIEW, p_worst, reasons
    reasons.append("no candidate crosses the threshold and the monitor agrees with every elicitation")
    return Decision.APPROVE, p_worst, reasons


# --------------------------------------------------------------------------- #
# Independent monitor
# --------------------------------------------------------------------------- #


def vote_count(settings: Settings | None) -> int:
    """k for the monitor ensemble: ``settings.llm.monitor.votes``, at least 1."""
    if settings is None:
        return MIN_VOTES
    raw = getattr(getattr(settings, "llm", None), "monitor", None)
    try:
        k = int(getattr(raw, "votes", MIN_VOTES) or MIN_VOTES)
    except (TypeError, ValueError):
        k = MIN_VOTES
    return max(MIN_VOTES, k)


def _vote(answer) -> MonitorVote:
    value = getattr(answer, "value", answer)
    if isinstance(value, MonitorVote):
        return value
    return MonitorVote.model_validate(value)


def monitor_scores(
    candidate: Candidate,
    context: ContextSlice,
    llm,
    monitor_llm,
    settings: Settings | None = None,
) -> MonitorScore:
    """Ask the independent monitor k times and return one robust score.

    ``monitor_llm`` is the transport the monitor must use; ``llm`` is the
    reviewer transport, accepted for interface symmetry and deliberately never
    called — independence is the whole point of this stage.

    A missing monitor or a failed vote raises :class:`LLMError`; the caller
    degrades to a human review.  Returning a score computed from fewer votes
    would silently weaken the independence claim the fusion relies on.
    """
    if monitor_llm is None:
        raise LLMError(
            "no independent monitor model is configured; the review cannot be fused",
            kind=FailureKind.NOT_CONFIGURED,
        )
    k = vote_count(settings)
    stage_model = settings.llm.monitor if settings is not None else None
    prompt = monitor_prompt(candidate, context)
    votes: list[MonitorVote] = []
    model_name = getattr(stage_model, "model", "") or ""
    for index in range(k):
        try:
            answer = monitor_llm.complete(
                "monitor", prompt, MonitorVote, stage_model=stage_model, instance="monitor"
            )
            vote = _vote(answer)
        except LLMError as exc:
            raise LLMError(
                f"monitor vote {index + 1}/{k} failed: {exc.message}",
                kind=exc.kind,
                status_code=exc.status_code,
                detail=exc.detail,
            ) from None
        except ValidationError as exc:
            raise LLMError(
                f"monitor vote {index + 1}/{k} did not match MonitorVote",
                kind=FailureKind.MALFORMED_OUTPUT,
                detail=f"{exc.error_count()} schema error(s)",
            ) from None
        except Exception as exc:  # a transport failure is still a typed failure
            raise LLMError(
                f"monitor vote {index + 1}/{k} failed: {type(exc).__name__}",
                kind=FailureKind.PROCESS_CRASH,
            ) from None
        votes.append(vote)
        if not model_name:
            model_name = str(getattr(answer, "model", "") or "")

    scores = [float(vote.score) for vote in votes]
    dispersion = (max(scores) - min(scores)) / 2.0
    return MonitorScore(
        score=max(0.0, min(1.0, statistics.median(scores))),
        votes=votes,
        dispersion=max(0.0, min(1.0, dispersion)),
        model=model_name,
        independent=monitor_llm is not llm,
    )


def monitor_scores_or_none(
    candidate: Candidate,
    context: ContextSlice,
    llm,
    monitor_llm,
    settings: Settings | None = None,
) -> tuple[MonitorScore | None, str]:
    """:func:`monitor_scores`, but a failure becomes ``(None, reason)``.

    The reason is receipt-able and names the degraded mode; the pipeline turns
    a missing monitor score into ``HUMAN_REVIEW`` rather than an approval.
    """
    try:
        return monitor_scores(candidate, context, llm, monitor_llm, settings), ""
    except LLMError as exc:
        return None, f"degraded: independent monitor unavailable ({exc.kind.value}: {exc.message}); human review"
