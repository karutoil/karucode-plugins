"""The LLM transport: stateless requests, strict JSON, typed failures."""

from __future__ import annotations

import json

import httpx
import pytest

from tests.support.openai_server import FakeOpenAI
from zero_loop_reviewer.config import load_settings
from zero_loop_reviewer.errors import FailureKind, LLMError
from zero_loop_reviewer.llm.client import Prompt, OpenAICompatibleClient, build_llm
from zero_loop_reviewer.models import MonitorVote, RiskElicitation


def _settings(base_url: str, **overrides):
    data = {"llm.base_url": base_url, "llm.api_key_env": "TEST_LLM_KEY"}
    data.update(overrides)
    return load_settings(data, {"TEST_LLM_KEY": "sk-test"})


RISK = {
    "defect_present": True,
    "category": "logic",
    "severity_score": 2,
    "failure_probability": 0.82,
    "cited_sites": [{"path": "a.py", "line": 3}],
    "root_cause": "off-by-one at the boundary",
}


def _client(base_url: str, *, receipt_sink=None, **overrides):
    return OpenAICompatibleClient(
        _settings(base_url, **overrides),
        environ={"TEST_LLM_KEY": "sk-test"},
        receipt_sink=receipt_sink,
    )


def _prompt(policy_free: bool = True, name: str = "RiskElicitation") -> Prompt:
    return Prompt(
        stage="risk",
        system="answer with json",
        user=f"SCHEMA: {name}\n{{}}",
        schema_name=name,
        policy_free=policy_free,
    )


def test_request_is_one_stateless_pair_and_parses_the_answer():
    with FakeOpenAI({"RiskElicitation": RISK}) as fake:
        client = _client(fake.base_url)
        answer = client.complete("risk", _prompt(), RiskElicitation)
    assert isinstance(answer.value, RiskElicitation)
    assert answer.value.failure_probability == 0.82
    assert answer.usage.input == 100
    request = fake.requests[0]
    assert request["path"] == "/v1/chat/completions"
    assert request["headers"]["authorization"] == "Bearer sk-test"
    assert len(request["body"]["messages"]) == 2  # no conversation history
    assert request["body"]["messages"][0]["role"] == "system"
    assert request["body"]["response_format"] == {"type": "json_object"}
    assert request["body"]["model"] == "od/deepseek-v4.1-flash"


def test_temperature_follows_stage_routing():
    with FakeOpenAI({"MonitorVote": {"score": 0.4, "rationale": "r"}}) as fake:
        client = _client(fake.base_url)
        client.complete("monitor", _prompt(False, "MonitorVote"), MonitorVote)
    assert fake.requests[0]["body"]["temperature"] == 0.8  # monitor default
    with FakeOpenAI({"MonitorVote": {"score": 0.4, "rationale": "r"}}) as fake:
        client = _client(fake.base_url)
        client.complete("monitor", _prompt(False, "MonitorVote"), MonitorVote, temperature=0.3)
    assert fake.requests[0]["body"]["temperature"] == 0.3  # an explicit override wins
    with FakeOpenAI({"MonitorVote": {"score": 0.4, "rationale": "r"}}) as fake:
        client = _client(fake.base_url)
        client.complete(
            "monitor",
            _prompt(False, "MonitorVote"),
            MonitorVote,
            stage_model=client.settings.llm.primary,
        )
    assert fake.requests[0]["body"]["temperature"] == 0.0  # the reviewer's deterministic default


def test_transport_failure_is_typed_and_retried():
    with FakeOpenAI({"RiskElicitation": RISK}) as fake:
        fake.failures = [503, 503]
        client = _client(fake.base_url, **{"llm.max_retries": 2})
        answer = client.complete("risk", _prompt(), RiskElicitation)
    assert answer.value.failure_probability == 0.82
    assert len(fake.requests) == 3


def test_auth_failure_is_not_retried():
    with FakeOpenAI({"RiskElicitation": RISK}) as fake:
        fake.failures = [401]
        client = _client(fake.base_url)
        with pytest.raises(LLMError) as error:
            client.complete("risk", _prompt(), RiskElicitation)
    assert error.value.kind is FailureKind.NOT_CONFIGURED
    assert len(fake.requests) == 1


def test_schema_mismatch_fails_closed_after_one_repair_attempt():
    bad = dict(RISK)
    bad["failure_probability"] = 4.2
    with FakeOpenAI({"RiskElicitation": bad}) as fake:
        client = _client(fake.base_url)
        with pytest.raises(LLMError) as error:
            client.complete("risk", _prompt(), RiskElicitation)
    assert error.value.kind is FailureKind.MALFORMED_OUTPUT
    assert len(fake.requests) == 2


def test_policy_language_in_a_policy_free_prompt_is_refused_before_the_request():
    with FakeOpenAI({"RiskElicitation": RISK}) as fake:
        client = _client(fake.base_url)
        prompt = Prompt(
            stage="risk",
            system="system",
            user="a false accept ratio changes the threshold",
            schema_name="RiskElicitation",
            policy_free=True,
        )
        with pytest.raises(LLMError) as error:
            client.complete("risk", prompt, RiskElicitation)
    assert error.value.kind is FailureKind.INVALID_INPUT
    assert fake.requests == []


def test_author_text_is_refused_outside_pmi():
    with FakeOpenAI({"RiskElicitation": RISK}) as fake:
        client = _client(fake.base_url)
        prompt = Prompt(
            stage="behavioral",
            system="system",
            user="user",
            schema_name="RiskElicitation",
            carries_author_text=True,
        )
        with pytest.raises(LLMError) as error:
            client.complete("behavioral", prompt, RiskElicitation)
    assert error.value.kind is FailureKind.INVALID_INPUT


def test_no_endpoint_means_every_call_fails_closed():
    settings = load_settings({}, {})
    llm = build_llm(settings, {})
    with pytest.raises(LLMError) as error:
        llm.complete("risk", _prompt(), RiskElicitation)
    assert error.value.kind is FailureKind.NOT_CONFIGURED


def test_the_review_deadline_caps_every_call_before_it_starts():
    with FakeOpenAI({"RiskElicitation": RISK}) as fake:
        client = _client(fake.base_url)
        client.cap_to(0.5)
        with pytest.raises(LLMError) as error:
            client.complete("risk", _prompt(), RiskElicitation)
    assert error.value.kind is FailureKind.TIMEOUT
    assert fake.requests == []


def test_receipts_record_tokens_hash_and_model():
    seen: list[dict] = []
    with FakeOpenAI({"RiskElicitation": RISK}) as fake:
        client = _client(fake.base_url, receipt_sink=seen.append)
        client.complete("risk", _prompt(), RiskElicitation)
    assert len(seen) == 1
    receipt = seen[0]
    assert receipt["tokens_in"] == 100 and receipt["tokens_out"] == 50
    assert len(receipt["input_sha256"]) == 64 and receipt["input_bytes"] > 0
    assert receipt["model"] == "fake-deepseek"
