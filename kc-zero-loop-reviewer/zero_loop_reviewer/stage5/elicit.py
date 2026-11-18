"""Stage 5 — policy-free risk elicitation.

One question, one answer, **no retry**: the model is asked only for a failure
probability and the evidence behind it.  Costs, ratios, thresholds and the
merge decision live in :class:`config.FusionConfig` and are applied by
:mod:`fusion` in host code, never inside a prompt.

Everything here fails closed: a refused prompt, a transport error or a schema
drift returns ``(None, reason)`` so the caller can record the degraded mode and
send the review to a human.  Silence is never "no finding".
"""

from __future__ import annotations

import json
import re

from pydantic import ValidationError

from ..config import Settings
from ..errors import FailureKind, LLMError
from ..llm.client import Prompt
from ..llm.client import assert_policy_free as _assert_client_policy_free
from ..llm.prompts import risk_only_prompt
from ..models import Candidate, ContextSlice, RiskElicitation

#: The exact six fields the elicitation contract allows.  A seventh field would
#: be a hole through which merge policy could reach the model or the host.
ELICITATION_FIELDS = frozenset(
    {
        "defect_present",
        "category",
        "severity_score",
        "failure_probability",
        "cited_sites",
        "root_cause",
    }
)

#: Policy vocabulary that must never steer an elicitation.  ``POLICY_TOKENS``
#: (``llm.client``) is matched as a substring; these are matched as whole words
#: so a candidate that merely mentions, say, "Tauri" is not mistaken for a leak.
EXTRA_POLICY_WORDS = ("c_fr", "c_fa", "tau", "threshold", "log-odds", "log odds")

_EXTRA_POLICY_RE = re.compile(
    r"\b(?:" + "|".join(re.escape(word) for word in EXTRA_POLICY_WORDS) + r")\b",
    re.IGNORECASE,
)


def schema_fields_ok() -> bool:
    """True when ``RiskElicitation`` still has exactly the six contract fields."""
    return frozenset(RiskElicitation.model_fields) == ELICITATION_FIELDS


def assert_policy_free(prompt_text: str) -> None:
    """Refuse any prompt that could steer the elicitation toward a merge policy.

    Delegates the shared vocabulary to :func:`llm.client.assert_policy_free`
    and adds the host-side cost/threshold names it does not know about.
    """
    _assert_client_policy_free(prompt_text)
    hits = sorted({match.group(0).lower() for match in _EXTRA_POLICY_RE.finditer(prompt_text)})
    if hits:
        raise LLMError(
            f"policy-free prompt names a host policy constant: {hits}",
            kind=FailureKind.INVALID_INPUT,
        )


def _reason(prefix: str, detail: str) -> str:
    return f"degraded: {prefix} ({detail}); human review"


#: One JSON object member named "description" (string, null, bool or number).
_DESCRIPTION_MEMBER = re.compile(
    r'"description"\s*:\s*(?:"(?:[^"\\]|\\.)*"|null|true|false|-?\d+(?:\.\d+)?)\s*,?\s*'
)

#: A comma left dangling by a removed member.
_DANGLING_COMMA = re.compile(r",(\s*[}\]])")

#: The heading ``prompts._schema`` puts in front of the JSON schema block.
_SCHEMA_MARK = "SCHEMA: RiskElicitation\n"


def _strip_schema_descriptions(user: str) -> str | None:
    """Drop ``"description"`` members from the schema block of a built prompt.

    ``prompts._schema`` serialises ``RiskElicitation.model_json_schema()``, and
    pydantic copies a model's *docstring* into ``description``.
    ``RiskElicitation``'s docstring names the policy fields it deliberately
    lacks ("cost, ratio, threshold, ACCEPT/REJECT"), so the prompt as built
    trips :func:`assert_policy_free` even though the six-field schema is
    policy-free.  The field set is the contract; the prose is not, so the
    descriptions are removed here.

    Only the schema block is touched.  Returns ``None`` when the block cannot
    be located or no longer parses — the caller then refuses the prompt rather
    than send something it does not understand.  Once ``prompts._schema`` stops
    embedding descriptions this becomes a no-op.
    """
    start = user.find(_SCHEMA_MARK)
    if start < 0:
        return None
    body_start = start + len(_SCHEMA_MARK)
    body_end = user.find("\n\n<EVIDENCE", body_start)
    if body_end < 0:
        body_end = len(user)
    schema_text = user[body_start:body_end]
    cleaned = _DANGLING_COMMA.sub(r"\1", _DESCRIPTION_MEMBER.sub("", schema_text))
    try:
        json.loads(cleaned)
    except (json.JSONDecodeError, ValueError):
        return None
    return user[:body_start] + cleaned + user[body_end:]


def sanitize_prompt(prompt: Prompt) -> Prompt | None:
    """The elicitation prompt with schema prose that leaks policy removed."""
    user = _strip_schema_descriptions(prompt.user)
    if user is None:
        return None
    return Prompt(
        stage=prompt.stage,
        system=prompt.system,
        user=user,
        schema_name=prompt.schema_name,
        policy_free=prompt.policy_free,
        carries_author_text=prompt.carries_author_text,
    )


def elicit(
    candidate: Candidate,
    context: ContextSlice,
    llm,
    settings: Settings | None = None,
) -> tuple[RiskElicitation | None, str]:
    """Ask for the risk-only elicitation of one candidate.

    Returns ``(elicitation, "")`` on success and ``(None, reason)`` on any
    failure; ``reason`` is receipt-able and names the degraded mode.  There is
    no retry that rephrases the question: a second attempt with different words
    would be a different measurement.
    """
    if not schema_fields_ok():
        return None, _reason(
            "RiskElicitation schema drifted",
            "fields=" + ",".join(sorted(RiskElicitation.model_fields)),
        )

    try:
        prompt = risk_only_prompt(candidate, context)
        sanitized = sanitize_prompt(prompt)
        if sanitized is None:
            return None, _reason(
                "elicitation prompt refused",
                "the schema block could not be stripped of policy prose",
            )
        assert_policy_free(sanitized.system + "\n" + sanitized.user)
    except LLMError as exc:
        return None, _reason("elicitation prompt refused", f"{exc.kind.value}: {exc.message}")

    stage_model = settings.llm.reviewer if settings is not None else None
    try:
        answer = llm.complete("risk", sanitized, RiskElicitation, stage_model=stage_model)
    except LLMError as exc:
        return None, _reason("risk elicitation failed", f"{exc.kind.value}: {exc.message}")
    except ValidationError as exc:
        return None, _reason("risk elicitation answer rejected", f"{exc.error_count()} schema error(s)")
    except Exception as exc:  # transport of an unexpected shape is still a failure
        return None, _reason("risk elicitation failed", type(exc).__name__)

    value = getattr(answer, "value", answer)
    try:
        elicitation = value if isinstance(value, RiskElicitation) else RiskElicitation.model_validate(value)
    except ValidationError as exc:
        return None, _reason("risk elicitation answer malformed", f"{exc.error_count()} schema error(s)")
    return elicitation, ""
