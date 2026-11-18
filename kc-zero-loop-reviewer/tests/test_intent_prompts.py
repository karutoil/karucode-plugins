"""PR intent is forwarded as structured, host-grounded claims, never raw text."""

from __future__ import annotations

from zero_loop_reviewer.llm.prompts import behavioral_prompt, integration_prompt, triage_prompt
from zero_loop_reviewer.models import ContextSlice, DiffBundle, MciClaim


def _claim() -> MciClaim:
    return MciClaim(
        claim="persist the selected provider before the first request",
        symbols=["save_provider"],
        supported=True,
        evidence="save_provider is called in run()",
    )


def _diff() -> DiffBundle:
    return DiffBundle(base_sha="a" * 40, head_sha="b" * 40, files=[])


def test_triage_and_behavioral_prompts_receive_claims() -> None:
    context = ContextSlice(text="a.py:1: pass", tokens=4, budget=1000)
    triage = triage_prompt(
        _diff(),
        changed_symbols=["save_provider"],
        critical_tokens=[],
        claims=[_claim()],
    )
    assert "pr_intent_claims" in triage.user
    assert "persist the selected provider" in triage.user
    behavioral = behavioral_prompt(_diff(), context, claims=[_claim()])
    assert "pr_intent_claims" in behavioral.user
    assert "persist the selected provider" in behavioral.user


def test_integration_prompt_receives_claims() -> None:
    context = ContextSlice(text="a.py:1: pass", tokens=4, budget=1000)
    prompt = integration_prompt(
        _diff(),
        context,
        edges=[("unit-a", "unit-b")],
        unit_notes=["unit-a files: a.py"],
        claims=[_claim()],
    )
    assert "pr_intent_claims" in prompt.user
    assert "persist the selected provider" in prompt.user
