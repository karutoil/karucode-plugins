"""Typed failures shared by every stage.

The pipeline never turns a failure into "no findings".  Every exception that
crosses a stage boundary carries a :class:`FailureKind`, and the pipeline maps
those kinds onto an explicit, recorded outcome.
"""

from __future__ import annotations

from enum import Enum


class FailureKind(str, Enum):
    # Runtime contract (§ Stage 1 A.5)
    PROCESS_CRASH = "process_crash"
    EMPTY_RESPONSE = "empty_response"
    MALFORMED_OUTPUT = "malformed_output"
    INPUT_DELIVERY_FAILED = "input_delivery_failed"
    TIMEOUT = "timeout"
    CANCELLED = "cancelled"
    # Configuration / capability
    NOT_CONFIGURED = "not_configured"
    TOOL_UNAVAILABLE = "tool_unavailable"
    SANDBOX_UNAVAILABLE = "sandbox_unavailable"
    READ_ONLY_VIOLATION = "read_only_violation"
    POOL_NOT_DISJOINT = "pool_not_disjoint"
    # Inputs
    INVALID_INPUT = "invalid_input"
    STALE_HEAD = "stale_head"
    GATE_REFUSED = "gate_refused"


class ReviewError(Exception):
    """Base class for an expected, classified failure."""

    kind: FailureKind = FailureKind.INVALID_INPUT

    def __init__(self, message: str, *, detail: str = ""):
        super().__init__(message)
        self.message = message
        self.detail = detail

    def __str__(self) -> str:  # pragma: no cover - trivial
        return f"{self.kind.value}: {self.message}"


class LLMError(ReviewError):
    """A model call that did not yield schema-valid output."""

    def __init__(
        self,
        message: str,
        *,
        kind: FailureKind = FailureKind.MALFORMED_OUTPUT,
        status_code: int | None = None,
        detail: str = "",
        raw: str = "",
    ):
        super().__init__(message, detail=detail)
        self.kind = kind
        self.status_code = status_code
        self.raw = raw


class SandboxError(ReviewError):
    kind = FailureKind.SANDBOX_UNAVAILABLE


class ToolError(ReviewError):
    kind = FailureKind.TOOL_UNAVAILABLE


class GateRefused(ReviewError):
    kind = FailureKind.GATE_REFUSED


class ContractViolation(ReviewError):
    kind = FailureKind.READ_ONLY_VIOLATION
