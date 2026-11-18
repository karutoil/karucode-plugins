"""A scripted JSON-LLM for stage tests.

It implements the same protocol as ``OpenAICompatibleClient`` and records each
call so a test can assert what was sent (messages, schema, temperature).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Callable

from pydantic import BaseModel, ValidationError

from zero_loop_reviewer.errors import FailureKind, LLMError
from zero_loop_reviewer.llm.client import Prompt
from zero_loop_reviewer.models import ModelAnswer, TokenUsage

Responder = dict[str, Any] | BaseException | Callable[[Prompt, type[BaseModel]], Any]


@dataclass
class ScriptedLLM:
    answers: dict[str, list[Responder]] = field(default_factory=dict)
    calls: list[dict[str, Any]] = field(default_factory=list)
    default: Responder | None = None

    def _answer(self, stage: str, prompt: Prompt, schema: type[BaseModel]) -> Any:
        queue = self.answers.get(stage)
        item: Responder | None = None
        if queue:
            item = queue.pop(0)
        elif self.default is not None:
            item = self.default
        else:
            raise LLMError(f"no scripted answer for stage {stage}", kind=FailureKind.MALFORMED_OUTPUT)
        if isinstance(item, BaseException):
            raise item
        if callable(item):
            return item(prompt, schema)
        if isinstance(item, schema):
            return item
        return schema.model_validate(item)

    def complete(
        self,
        stage: str,
        prompt: Prompt,
        schema: type[BaseModel],
        *,
        stage_model: Any = None,
        temperature: float | None = None,
        instance: str = "reviewer",
    ) -> ModelAnswer:
        self.calls.append(
            {
                "stage": stage,
                "prompt": prompt,
                "schema": schema.__name__,
                "temperature": temperature,
                "instance": instance,
            }
        )
        value = self._answer(stage, prompt, schema)
        return ModelAnswer(
            value=value,
            raw=value.model_dump_json() if isinstance(value, BaseModel) else "{}",
            model=getattr(stage_model, "model", "scripted"),
            usage=TokenUsage(input=len(prompt.user) // 4, output=64),
            attempts=1,
        )


def answer(**kwargs: Any) -> dict[str, Any]:
    """Shorthand for a dict answer."""
    return kwargs


class ExplodingLLM:
    def __init__(self, error: BaseException | None = None):
        self.error = error or LLMError("scripted failure", kind=FailureKind.TIMEOUT)
        self.calls = 0

    def complete(self, *args: Any, **kwargs: Any) -> ModelAnswer:
        self.calls += 1
        raise self.error


class ConstantLLM:
    """Always returns the same validated value for every stage."""

    def __init__(self, value: BaseModel):
        self.value = value
        self.calls: list[str] = []

    def complete(self, stage: str, prompt: Prompt, schema: type[BaseModel], **_: Any) -> ModelAnswer:
        self.calls.append(stage)
        try:
            value = schema.model_validate(self.value.model_dump())
        except ValidationError as exc:
            raise LLMError(f"constant answer does not fit {schema.__name__}: {exc}") from None
        return ModelAnswer(value=value, raw=value.model_dump_json(), model="constant", usage=TokenUsage(), attempts=1)
