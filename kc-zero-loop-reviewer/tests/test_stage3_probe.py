"""Stage 3: probe synthesis with reflexion — attempt accounting, compile
validation, assertion/boundary requirements and failure feedback."""

from __future__ import annotations

import pytest

from tests.support.llm import ConstantLLM, ExplodingLLM, ScriptedLLM
from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.errors import FailureKind, LLMError
from zero_loop_reviewer.models import Candidate, Category, ContextSlice, Evidence, ProbeSpec, StageId
from zero_loop_reviewer.stage3.probe import (
    MAX_ATTEMPTS,
    has_assertion,
    has_boundary,
    inspect_probe,
    synthesize,
)

GOOD = 'from calc import add\n\n\ndef test_add():\n    assert add(2, 3) == 5\n    assert add(0, 0) == 0\n'
NO_ASSERTION = "from calc import add\n\n\ndef test_add():\n    add(2, 3)\n"
NO_BOUNDARY = 'from greet import greet\n\n\ndef test_greet():\n    assert greet("bob") == "hello bob"\n'
BROKEN = "def test_add(:\n    assert add(2, 3) == 5\n"


def _candidate() -> Candidate:
    return Candidate(
        id="c-1",
        stage=StageId.SECSEM,
        category=Category.LOGIC,
        title="add() subtracts instead of adding",
        root_cause="calc.add returns a - b, so every sum is wrong.",
        severity=2,
        evidence=Evidence(kind="analyzer", analyzer="semgrep", rule_id="zero-loop.add"),
    )


def _context() -> ContextSlice:
    return ContextSlice(text="calc.py:\n1: def add(a, b):\n2:     return a - b", tokens=20, budget=128_000)


def _spec(**overrides) -> dict:
    payload = {
        "test_path": ".probe/test_add.py",
        "content": GOOD,
        "target_symbols": ["add"],
        "framework": "pytest",
        "rationale": "add() returns the wrong value on every input",
    }
    payload.update(overrides)
    return payload


def test_first_attempt_is_accepted_and_the_prompt_demands_boundaries():
    llm = ScriptedLLM(answers={"probe": [_spec()]})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert problems == []
    assert isinstance(spec, ProbeSpec)
    assert spec.test_path == ".probe/test_add.py"
    assert len(llm.calls) == 1
    prompt = llm.calls[0]["prompt"].user
    assert "boundary" in prompt.lower()
    assert "RETURN VALUES" in prompt
    assert "STATE" in prompt
    assert llm.calls[0]["schema"] == "ProbeSpec"


def test_a_syntax_error_is_an_attempt_failure_fed_back_to_the_next_attempt():
    llm = ScriptedLLM(answers={"probe": [_spec(content=BROKEN), _spec()]})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert spec is not None
    assert len(problems) == 1
    assert "does not compile" in problems[0]
    assert len(llm.calls) == 2
    second = llm.calls[1]["prompt"].user
    assert "previous_probe_failure" in second
    assert "does not compile" in second


def test_a_probe_without_an_assertion_is_refused():
    llm = ScriptedLLM(answers={"probe": [_spec(content=NO_ASSERTION), _spec()]})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert spec is not None
    assert "no assertion" in problems[0]


def test_a_probe_without_a_boundary_input_is_refused():
    llm = ScriptedLLM(answers={"probe": [_spec(content=NO_BOUNDARY), _spec()]})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert spec is not None
    assert "boundary" in problems[0]


def test_an_unsafe_test_path_is_refused():
    llm = ScriptedLLM(answers={"probe": [_spec(test_path="../../escape.py"), _spec()]})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert spec is not None
    assert "inside the worktree" in problems[0]


def test_the_validate_callback_can_reject_a_probe():
    calls: list[ProbeSpec] = []

    def validate(spec: ProbeSpec) -> str:
        calls.append(spec)
        return "the probe passes on the head commit already" if len(calls) == 1 else ""

    llm = ScriptedLLM(answers={"probe": [_spec(), _spec()]})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings(), validate=validate)
    assert spec is not None
    assert "passes on the head commit" in problems[0]
    assert "passes on the head commit" in llm.calls[1]["prompt"].user


def test_a_raising_validator_is_an_attempt_failure_not_a_crash():
    calls: list[int] = []

    def validate(spec: ProbeSpec) -> str:
        calls.append(1)
        if len(calls) == 1:
            raise RuntimeError("validator exploded")
        return ""

    llm = ScriptedLLM(answers={"probe": [_spec(), _spec()]})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings(), validate=validate)
    assert spec is not None
    assert "validator raised RuntimeError" in problems[0]


def test_three_bad_attempts_return_no_probe_and_three_problems():
    llm = ScriptedLLM(answers={"probe": [_spec(content=BROKEN), _spec(content=NO_ASSERTION), _spec(content=BROKEN)]})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert spec is None
    assert len(problems) == MAX_ATTEMPTS
    assert len(llm.calls) == MAX_ATTEMPTS


def test_attempts_are_capped_at_three_even_when_asked_for_more():
    llm = ScriptedLLM(answers={"probe": [_spec(content=BROKEN)] * 5})
    spec, problems = synthesize(_candidate(), _context(), llm, Settings(), max_attempts=9)
    assert spec is None
    assert len(llm.calls) == MAX_ATTEMPTS


def test_settings_can_lower_the_attempt_budget():
    settings = Settings(probe_attempts=1)
    llm = ScriptedLLM(answers={"probe": [_spec(content=BROKEN), _spec()]})
    spec, problems = synthesize(_candidate(), _context(), llm, settings)
    assert spec is None
    assert len(llm.calls) == 1
    assert len(problems) == 1


def test_a_timeout_stops_the_loop_without_raising():
    llm = ExplodingLLM(LLMError("upstream timed out", kind=FailureKind.TIMEOUT))
    spec, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert spec is None
    assert len(problems) == 1
    assert "timeout" in problems[0]
    assert llm.calls == 1


def test_a_malformed_answer_is_retried():
    llm = ScriptedLLM(
        answers={
            "probe": [
                LLMError("answer did not match the schema", kind=FailureKind.MALFORMED_OUTPUT),
                _spec(),
            ]
        }
    )
    spec, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert spec is not None
    assert "malformed_output" in problems[0]


def test_constant_llm_returns_the_same_probe_every_time():
    spec = ProbeSpec(**_spec())
    llm = ConstantLLM(spec)
    found, problems = synthesize(_candidate(), _context(), llm, Settings())
    assert found == spec
    assert problems == []


def test_inspect_probe_reports_every_problem_at_once():
    issues = inspect_probe(ProbeSpec(test_path="../../x.py", content="def test(:\n    pass\n"))
    joined = " | ".join(issues)
    assert "inside the worktree" in joined
    assert "does not compile" in joined
    assert "no assertion" in joined


@pytest.mark.parametrize(
    "content, assertion, boundary",
    [
        (GOOD, True, True),
        (NO_ASSERTION, False, False),
        (NO_BOUNDARY, True, False),
        ("def test_x():\n    self.assertEqual(f(0), 1)\n", True, True),
        ("with pytest.raises(ValueError):\n    assert f(None)\n", True, True),
    ],
)
def test_assertion_and_boundary_detectors(content, assertion, boundary):
    assert has_assertion(content) is assertion
    assert has_boundary(content) is boundary
