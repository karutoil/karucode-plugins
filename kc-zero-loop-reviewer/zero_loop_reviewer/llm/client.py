"""OpenAI-compatible model access, one stateless call per request.

There is deliberately **no conversation history** anywhere in this module: a
re-push is a fresh pair of messages.  That is what stops an adversarial PR
thread from steering a later review.
"""

from __future__ import annotations

import json
import os
import re
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Mapping, Protocol, TypeVar

import httpx
from pydantic import BaseModel, ValidationError

from ..config import Settings, StageModel
from ..errors import FailureKind, LLMError
from ..models import AttemptStatus, ModelAnswer, TokenUsage, canonical_json, sha256_hex

T = TypeVar("T", bound=BaseModel)

#: Prompt fragments that must never reach a policy-free elicitation.
POLICY_TOKENS = (
    "false accept",
    "false reject",
    "false-accept",
    "false-reject",
    "cost of",
    "penalty",
    "threshold",
    "ratio",
    "tau",
    "merge decision",
    "accept/reject",
    "request changes",
    "approve the",
    "block the merge",
)


@dataclass(frozen=True)
class Prompt:
    stage: str
    system: str
    user: str
    schema_name: str
    policy_free: bool = False
    carries_author_text: bool = False

    def payload(self) -> bytes:
        return canonical_json({"system": self.system, "user": self.user}).encode("utf-8")

    @property
    def sha256(self) -> str:
        return sha256_hex(self.payload())

    def messages(self) -> list[dict[str, str]]:
        return [
            {"role": "system", "content": self.system},
            {"role": "user", "content": self.user},
        ]


class JSONLLM(Protocol):
    def complete(
        self,
        stage: str,
        prompt: Prompt,
        schema: type[T],
        *,
        stage_model: StageModel | None = None,
        temperature: float | None = None,
        instance: str = "reviewer",
    ) -> ModelAnswer: ...


ReceiptSink = Callable[[dict[str, Any]], None]


def _now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()) + f".{int(time.time() * 1000) % 1000:03d}Z"


def assert_policy_free(text: str) -> None:
    """Refuse to send policy steering inside a probability-elicitation prompt.

    Single-word tokens match on a word boundary: ``rationale`` is a legitimate
    schema field and must not be mistaken for ``ratio``.  Multi-word phrases
    match as substrings.
    """
    low = text.lower()
    hits: list[str] = []
    for token in POLICY_TOKENS:
        if " " in token or "/" in token:
            if token in low:
                hits.append(token)
        elif re.search(rf"\b{re.escape(token)}\b", low):
            hits.append(token)
    if hits:
        raise LLMError(
            f"policy-free prompt contains policy language: {hits}",
            kind=FailureKind.INVALID_INPUT,
        )


def _strip_fences(text: str) -> str:
    stripped = text.strip()
    if stripped.startswith("```"):
        lines = stripped.splitlines()
        if lines:
            lines = lines[1:]
        if lines and lines[-1].strip().startswith("```"):
            lines = lines[:-1]
        stripped = "\n".join(lines).strip()
    return stripped


def extract_json_object(text: str) -> dict[str, Any]:
    body = _strip_fences(text)
    try:
        value = json.loads(body)
        if not isinstance(value, dict):
            raise ValueError("not a JSON object")
        return value
    except (json.JSONDecodeError, ValueError):
        start = body.find("{")
        end = body.rfind("}")
        if start < 0 or end <= start:
            raise LLMError("answer contained no JSON object", kind=FailureKind.MALFORMED_OUTPUT, raw=text[:2000])
        try:
            value = json.loads(body[start : end + 1])
        except json.JSONDecodeError as exc:
            raise LLMError(
                f"answer is not valid JSON: {exc.msg}",
                kind=FailureKind.MALFORMED_OUTPUT,
                raw=text[:2000],
            ) from None
        if not isinstance(value, dict):
            raise LLMError("answer is not a JSON object", kind=FailureKind.MALFORMED_OUTPUT, raw=text[:2000])
        return value


class OpenAICompatibleClient:
    """One client per (endpoint, key, instance).  Holds no conversation state."""

    def __init__(
        self,
        settings: Settings,
        *,
        environ: Mapping[str, str] | None = None,
        client: httpx.Client | None = None,
        sleeper: Callable[[float], None] = time.sleep,
        receipt_sink: ReceiptSink | None = None,
    ):
        self.settings = settings
        self.environ = environ if environ is not None else os.environ
        self.base_url = settings.llm_base_url(self.environ)
        self.api_key = settings.api_key(self.environ)
        self._client = client
        self._sleeper = sleeper
        self._receipt_sink = receipt_sink
        # A per-review cap, set by the pipeline from its remaining budget.  A
        # single call can never outlive the review's deadline.
        self.timeout_cap: float | None = None

    def cap_to(self, seconds: float) -> None:
        self.timeout_cap = max(0.0, float(seconds))

    # -- endpoint ---------------------------------------------------------
    @property
    def configured(self) -> bool:
        return bool(self.base_url)

    def _http(self) -> httpx.Client:
        if self._client is not None:
            return self._client
        self._client = httpx.Client(
            timeout=httpx.Timeout(self.settings.llm.timeout_s, connect=10.0),
            follow_redirects=False,
        )
        return self._client

    def _headers(self) -> dict[str, str]:
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": "zero-loop-reviewer/1.0",
        }
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        headers.update(dict(self.settings.llm.extra_headers))
        return headers

    # -- call -------------------------------------------------------------
    def complete(
        self,
        stage: str,
        prompt: Prompt,
        schema: type[T],
        *,
        stage_model: StageModel | None = None,
        temperature: float | None = None,
        instance: str = "reviewer",
    ) -> ModelAnswer:
        if not self.configured:
            raise LLMError(
                "no OpenAI-compatible endpoint is configured (set llm.base_url or ZERO_LOOP_LLM_BASE_URL)",
                kind=FailureKind.NOT_CONFIGURED,
            )
        if prompt.policy_free:
            assert_policy_free(prompt.system + "\n" + prompt.user)
        if prompt.carries_author_text and stage != "pmi":
            raise LLMError(
                "author text reached a non-PMI prompt; refusing to send",
                kind=FailureKind.INVALID_INPUT,
            )
        model_cfg = stage_model or self._stage_config(stage, instance)
        temp = model_cfg.temperature if temperature is None else temperature
        timeout_s = min(self.settings.llm.timeout_s, self.timeout_cap) if self.timeout_cap is not None else self.settings.llm.timeout_s
        if timeout_s <= 1.0:
            raise LLMError(
                "the review deadline left no time for a model call",
                kind=FailureKind.TIMEOUT,
            )
        payload = prompt.payload()
        body: dict[str, Any] = {
            "model": model_cfg.model,
            "messages": prompt.messages(),
            "max_tokens": model_cfg.max_tokens,
            "stream": False,
        }
        if temp is not None:
            body["temperature"] = temp
        if model_cfg.reasoning_effort:
            body["reasoning_effort"] = model_cfg.reasoning_effort
        if self.settings.llm.response_format:
            body["response_format"] = {"type": "json_object"}
        schema_json = json.dumps(schema.model_json_schema(), separators=(",", ":"))[:6000]
        attempts = 0
        last_error: LLMError | None = None
        started = _now()
        started_monotonic = time.monotonic()
        attempt_bodies = [body]
        repair_body = json.loads(json.dumps(body))
        # One repair attempt is a schema-shape correction, not a second opinion:
        # it never asks the model to reconsider its judgement.
        repair_body["messages"] = [
            *prompt.messages()[:1],
            {
                "role": "user",
                "content": prompt.user
                + "\n\nYour previous answer did not match this JSON schema:\n"
                + schema_json
                + "\nReturn one JSON object that matches it exactly, with no other text.",
            },
        ]
        attempt_bodies.append(repair_body)
        use_repair = False

        while attempts < self.settings.llm.max_retries + 2:
            current = repair_body if use_repair else body
            attempts += 1
            try:
                response = self._http().post(
                    self.base_url.rstrip("/") + "/chat/completions",
                    headers=self._headers(),
                    json=current,
                    timeout=timeout_s,
                )
            except httpx.TimeoutException as exc:
                last_error = LLMError(f"model call timed out: {exc}", kind=FailureKind.TIMEOUT)
                self._record(stage, last_error.kind, model_cfg.model, payload, "", TokenUsage(), started, attempts)
                raise last_error from None
            except httpx.HTTPError as exc:
                last_error = LLMError(f"transport error: {exc}", kind=FailureKind.TIMEOUT)
                if attempts <= self.settings.llm.max_retries:
                    self._sleeper(min(2.0, 0.2 * 2**attempts))
                    continue
                self._record(stage, last_error.kind, model_cfg.model, payload, "", TokenUsage(), started, attempts)
                raise last_error from None

            if response.status_code == 429 or response.status_code >= 500:
                last_error = LLMError(
                    f"endpoint answered {response.status_code}",
                    kind=FailureKind.TIMEOUT,
                    status_code=response.status_code,
                    detail=response.text[:1000],
                )
                if attempts <= self.settings.llm.max_retries:
                    self._sleeper(min(4.0, 0.25 * 2**attempts))
                    continue
                self._record(stage, last_error.kind, model_cfg.model, payload, response.text, TokenUsage(), started, attempts)
                raise last_error
            if response.status_code >= 400:
                error = LLMError(
                    f"endpoint rejected the call with {response.status_code}",
                    kind=FailureKind.NOT_CONFIGURED if response.status_code in (401, 403, 404) else FailureKind.MALFORMED_OUTPUT,
                    status_code=response.status_code,
                    detail=response.text[:1000],
                )
                self._record(stage, error.kind, model_cfg.model, payload, response.text, TokenUsage(), started, attempts)
                raise error

            try:
                doc = response.json()
            except ValueError:
                last_error = LLMError("endpoint returned a non-JSON HTTP body", kind=FailureKind.MALFORMED_OUTPUT)
                self._record(stage, last_error.kind, model_cfg.model, payload, response.text, TokenUsage(), started, attempts)
                raise last_error from None
            usage = self._usage(doc)
            content = self._content(doc)
            if not content.strip():
                last_error = LLMError("model returned an empty answer", kind=FailureKind.EMPTY_RESPONSE)
                if not use_repair:
                    use_repair = True
                    continue
                self._record(stage, last_error.kind, model_cfg.model, payload, "", usage, started, attempts)
                raise last_error
            try:
                raw_obj = extract_json_object(content)
            except LLMError as exc:
                last_error = exc
                if not use_repair:
                    use_repair = True
                    continue
                self._record(stage, exc.kind, model_cfg.model, payload, content, usage, started, attempts)
                raise
            try:
                value = schema.model_validate(raw_obj)
            except ValidationError as exc:
                last_error = LLMError(
                    f"answer failed {schema.__name__}: {exc.errors()[:3]}",
                    kind=FailureKind.MALFORMED_OUTPUT,
                    raw=content[:2000],
                )
                if not use_repair:
                    use_repair = True
                    continue
                self._record(stage, last_error.kind, model_cfg.model, payload, content, usage, started, attempts)
                raise last_error from None
            actual_model = str(doc.get("model") or model_cfg.model)
            self._record(stage, AttemptStatus.COMPLETED, actual_model, payload, content, usage, started, attempts)
            return ModelAnswer(
                value=value,
                raw=content,
                model=actual_model,
                usage=usage,
                attempts=attempts,
            )
        raise last_error or LLMError("model call failed", kind=FailureKind.MALFORMED_OUTPUT)

    # -- helpers ----------------------------------------------------------
    def _stage_config(self, stage: str, instance: str) -> StageModel:
        """Route a stage to its configured model without the caller spelling it."""
        llm = self.settings.llm
        if instance == "monitor" or stage == "monitor":
            return llm.monitor
        return {
            "triage": llm.triage,
            "security": llm.security,
            "probe": llm.probe,
            "fallback": llm.reviewer,
            "behavioral": llm.reviewer,
            "risk": llm.reviewer,
            "civer": llm.reviewer,
            "pmi": llm.triage,
        }.get(stage, llm.primary)

    def _usage(self, doc: Mapping[str, Any]) -> TokenUsage:
        usage = doc.get("usage")
        if not isinstance(usage, dict):
            return TokenUsage()
        return TokenUsage(
            input=int(usage.get("prompt_tokens") or usage.get("input_tokens") or 0),
            output=int(usage.get("completion_tokens") or usage.get("output_tokens") or 0),
        )

    def _content(self, doc: Mapping[str, Any]) -> str:
        choices = doc.get("choices")
        if not isinstance(choices, list) or not choices:
            raise LLMError("endpoint returned no choices", kind=FailureKind.MALFORMED_OUTPUT)
        message = choices[0].get("message") if isinstance(choices[0], dict) else None
        if not isinstance(message, dict):
            raise LLMError("endpoint returned no message", kind=FailureKind.MALFORMED_OUTPUT)
        content = message.get("content")
        if isinstance(content, str):
            return content
        if isinstance(content, list):
            parts = [part.get("text", "") for part in content if isinstance(part, dict)]
            return "".join(parts)
        return ""

    def _record(
        self,
        stage: str,
        status: AttemptStatus | FailureKind,
        model: str,
        payload: bytes,
        response_text: str,
        usage: TokenUsage,
        started: str,
        attempts: int,
    ) -> None:
        if self._receipt_sink is None:
            return
        self._receipt_sink(
            {
                "stage": stage,
                "status": status if isinstance(status, AttemptStatus) else _map_failure(status),
                "started_at": started,
                "finished_at": _now(),
                "model": model,
                "input_sha256": sha256_hex(payload),
                "input_bytes": len(payload),
                "output_sha256": sha256_hex(response_text) if response_text else "",
                "output_bytes": len(response_text.encode("utf-8")),
                "tokens_in": usage.input,
                "tokens_out": usage.output,
                "detail": f"attempts={attempts}",
            }
        )


def _map_failure(kind: FailureKind) -> AttemptStatus:
    return {
        FailureKind.TIMEOUT: AttemptStatus.TIMEOUT,
        FailureKind.CANCELLED: AttemptStatus.CANCELLED,
        FailureKind.EMPTY_RESPONSE: AttemptStatus.EMPTY_RESPONSE,
        FailureKind.MALFORMED_OUTPUT: AttemptStatus.MALFORMED_OUTPUT,
        FailureKind.NOT_CONFIGURED: AttemptStatus.NOT_CONFIGURED,
        FailureKind.INPUT_DELIVERY_FAILED: AttemptStatus.INPUT_DELIVERY_FAILED,
        FailureKind.TOOL_UNAVAILABLE: AttemptStatus.TOOL_UNAVAILABLE,
    }.get(kind, AttemptStatus.MALFORMED_OUTPUT)


class NullLLM:
    """Used when no endpoint is configured: every call fails closed."""

    def complete(self, stage: str, prompt: Prompt, schema: type[T], **_: Any) -> ModelAnswer:
        raise LLMError(
            "no model endpoint configured; LLM stages are unavailable",
            kind=FailureKind.NOT_CONFIGURED,
        )


def build_llm(settings: Settings, environ: Mapping[str, str] | None = None, receipt_sink: ReceiptSink | None = None) -> JSONLLM:
    client = OpenAICompatibleClient(settings, environ=environ, receipt_sink=receipt_sink)
    if not client.configured:
        return NullLLM()
    return client


@dataclass
class VoteSet:
    answers: list[ModelAnswer] = field(default_factory=list)

    @property
    def values(self) -> list[BaseModel]:
        return [a.value for a in self.answers]
