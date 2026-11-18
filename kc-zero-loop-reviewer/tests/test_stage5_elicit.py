"""Stage 5A: the elicitation is policy-free, six-fielded and fails closed."""

from __future__ import annotations

import pytest

from tests.support.llm import ExplodingLLM, ScriptedLLM, answer
from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.errors import FailureKind, LLMError
from zero_loop_reviewer.llm.client import POLICY_TOKENS, Prompt
from zero_loop_reviewer.llm.prompts import risk_only_prompt
from zero_loop_reviewer.models import (
    Candidate,
    Category,
    CitedSite,
    ContextSlice,
    Evidence,
    RiskElicitation,
    StageId,
    Verification,
    VerificationKind,
)
from zero_loop_reviewer.stage5.elicit import (
    ELICITATION_FIELDS,
    EXTRA_POLICY_WORDS,
    assert_policy_free,
    elicit,
    sanitize_prompt,
    schema_fields_ok,
)

CONTEXT = ContextSlice(
    text="src/io.py:42\ndef read_all(stream):\n    return stream.read()\n",
    tokens=24,
    budget=1000,
)

VALID = {
    "defect_present": True,
    "category": "resource",
    "severity_score": 2,
    "failure_probability": 0.8,
    "cited_sites": [{"path": "src/io.py", "line": 42, "symbol": "read_all"}],
    "root_cause": "read() with no size bound grows one buffer until the worker is killed",
}


def _candidate(**overrides) -> Candidate:
    payload = {
        "id": "cand-1",
        "stage": StageId.SAST,
        "category": Category.RESOURCE,
        "title": "unbounded read grows one buffer per byte",
        "root_cause": "read() is called with no size bound",
        "severity": 2,
        "cited_sites": [CitedSite(path="src/io.py", line=42, symbol="read_all")],
        "evidence": Evidence(
            kind="analyzer",
            analyzer="semgrep",
            rule_id="python.lang.correctness.unbounded-read",
        ),
        "verification": Verification(kind=VerificationKind.PROBE),
    }
    payload.update(overrides)
    return Candidate(**payload)


# --------------------------------------------------------------------------- #
# Policy leakage
# --------------------------------------------------------------------------- #


def test_sent_prompt_carries_no_policy_language() -> None:
    prompt = sanitize_prompt(risk_only_prompt(_candidate(), CONTEXT))
    assert prompt is not None
    text = (prompt.system + "\n" + prompt.user).lower()
    assert [token for token in POLICY_TOKENS if token in text] == []
    for word in EXTRA_POLICY_WORDS:
        assert word not in text, word
    for constant in ("c_fr", "c_fa", "tau", "threshold"):
        assert constant not in text, constant
    assert prompt.policy_free is True
    assert prompt.schema_name == "RiskElicitation"
    assert_policy_free(prompt.system + "\n" + prompt.user)  # must not raise


def test_schema_descriptions_are_stripped_from_the_prompt() -> None:
    """The schema block must not carry model docstrings.

    ``RiskElicitation``'s docstring names the policy fields the contract
    deliberately omits; pydantic copies it into ``model_json_schema()``.  The
    builder strips every ``description`` member, so the prompt is clean before
    the elicitation module's own sanitiser sees it.
    """
    raw = risk_only_prompt(_candidate(), CONTEXT)
    assert '"description"' not in raw.user
    assert "cost, ratio, threshold" not in raw.user.lower()
    assert [token for token in POLICY_TOKENS if token in (raw.system + raw.user).lower()] == []
    cleaned = sanitize_prompt(raw)
    assert cleaned is not None
    assert cleaned.user == raw.user


def test_sanitize_prompt_leaves_a_clean_schema_alone() -> None:
    user = 'SCHEMA: RiskElicitation\n{"type":"object"}\n\n<EVIDENCE name=context>\nx\n</EVIDENCE>'
    prompt = Prompt(stage="risk", system="s", user=user, schema_name="RiskElicitation", policy_free=True)
    cleaned = sanitize_prompt(prompt)
    assert cleaned is not None
    assert cleaned.user == user


def test_sanitize_prompt_refuses_an_unparseable_schema() -> None:
    prompt = Prompt(
        stage="risk",
        system="s",
        user='SCHEMA: RiskElicitation\n{"type": }\n\n<EVIDENCE name=context>\nx\n</EVIDENCE>',
        schema_name="RiskElicitation",
        policy_free=True,
    )
    assert sanitize_prompt(prompt) is None


@pytest.mark.parametrize(
    "leak",
    [
        "Weigh the cost of a false reject before answering.",
        "Recall that tau = c_fr / (c_fa + c_fr).",
        "Anything above the threshold should be reported.",
        "Decide whether to request changes.",
    ],
)
def test_assert_policy_free_rejects_leaks(leak: str) -> None:
    with pytest.raises(LLMError) as excinfo:
        assert_policy_free(leak)
    assert excinfo.value.kind is FailureKind.INVALID_INPUT


def test_assert_policy_free_tolerates_ordinary_prose() -> None:
    assert_policy_free(
        "Score how likely the changed code is to fail on realistic inputs: "
        "empty, zero, duplicate, and concurrent callers."
    )


# --------------------------------------------------------------------------- #
# The six-field contract
# --------------------------------------------------------------------------- #


def test_elicitation_schema_is_exactly_the_contract() -> None:
    assert set(RiskElicitation.model_fields) == {
        "defect_present",
        "category",
        "severity_score",
        "failure_probability",
        "cited_sites",
        "root_cause",
    }
    assert len(RiskElicitation.model_fields) == 6
    assert set(RiskElicitation.model_fields) == set(ELICITATION_FIELDS)
    assert schema_fields_ok() is True


def test_elicitation_schema_has_no_policy_field() -> None:
    forbidden = {"threshold", "tau", "c_fr", "c_fa", "ratio", "decision", "accept", "reject"}
    assert forbidden.isdisjoint(RiskElicitation.model_fields)


def test_elicit_returns_the_validated_answer() -> None:
    llm = ScriptedLLM(answers={"risk": [answer(**VALID)]})
    value, reason = elicit(_candidate(), CONTEXT, llm, Settings())

    assert reason == ""
    assert isinstance(value, RiskElicitation)
    assert set(value.model_fields_set) == set(ELICITATION_FIELDS)
    assert value.failure_probability == pytest.approx(0.8)
    assert len(llm.calls) == 1
    assert llm.calls[0]["stage"] == "risk"
    assert llm.calls[0]["schema"] == "RiskElicitation"
    assert llm.calls[0]["instance"] == "reviewer"


@pytest.mark.parametrize("probability", [0.0, 1.0])
def test_elicit_accepts_the_probability_endpoints(probability: float) -> None:
    answer_payload = dict(VALID, failure_probability=probability)
    llm = ScriptedLLM(answers={"risk": [answer_payload]})
    value, reason = elicit(_candidate(), CONTEXT, llm, Settings())
    assert reason == ""
    assert value is not None
    assert value.failure_probability == probability


@pytest.mark.parametrize(
    "payload",
    [
        dict(VALID, threshold=0.9),  # a policy field the schema must reject
        dict(VALID, failure_probability=1.4),  # out of range
        dict(VALID, cited_sites=[]),  # defect_present without a citation
    ],
)
def test_elicit_degrades_on_schema_drift(payload: dict) -> None:
    llm = ScriptedLLM(answers={"risk": [payload]})
    value, reason = elicit(_candidate(), CONTEXT, llm, Settings())
    assert value is None
    assert reason.startswith("degraded:")
    assert "human review" in reason
    assert len(llm.calls) == 1  # no retry that rephrases the question


def test_elicit_degrades_on_transport_failure_without_retrying() -> None:
    llm = ExplodingLLM()
    value, reason = elicit(_candidate(), CONTEXT, llm, Settings())
    assert value is None
    assert "degraded:" in reason
    assert "timeout" in reason
    assert llm.calls == 1


def test_elicit_refuses_a_leaking_prompt(monkeypatch: pytest.MonkeyPatch) -> None:
    import zero_loop_reviewer.stage5.elicit as module

    def leaky(candidate, context):
        returned = risk_only_prompt(candidate, context)
        return returned.__class__(
            stage=returned.stage,
            system=returned.system + " Weigh the cost of a false reject.",
            user=returned.user,
            schema_name=returned.schema_name,
            policy_free=returned.policy_free,
        )

    monkeypatch.setattr(module, "risk_only_prompt", leaky)
    llm = ScriptedLLM(answers={"risk": [answer(**VALID)]})
    value, reason = elicit(_candidate(), CONTEXT, llm, Settings())
    assert value is None
    assert "degraded:" in reason
    assert llm.calls == []
