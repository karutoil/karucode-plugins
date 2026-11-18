"""Stage 3 — probe synthesis with reflexion (≤3 attempts).

A probe is a *reproduction test*: it must fail while the defect is present and
pass once the defect is repaired.  The model is asked for one, host code checks
it, and a rejected attempt is fed back verbatim to the next attempt (Reflexion).
Host checks, in order:

1. the test path is a simple relative path that cannot leave the worktree;
2. the probe compiles (``compile(content, "<probe>", "exec")``) — a syntax error
   is an attempt failure, never a returned probe;
3. the probe contains at least one assertion (``assert`` / ``self.assert*`` /
   ``pytest.raises``) so it can only fail behaviourally;
4. the probe exercises a boundary input (empty, zero, one, ``None``, duplicate,
   maximum) — the prompt demands return/state assertions on boundaries and the
   host refuses a probe that ignores them;
5. an optional ``validate`` callback (usually "does this probe pass at head?")
   may reject a candidate probe; its message is fed back too.
"""

from __future__ import annotations

import re
from typing import Any, Callable, Sequence

from ..config import Settings
from ..errors import FailureKind, LLMError, SandboxError
from ..llm.prompts import probe_prompt
from ..models import Candidate, ContextSlice, ProbeSpec
from .sandbox import probe_relative_path

#: The interface caps synthesis at three attempts.
MAX_ATTEMPTS = 3

#: A probe must assert something a defect can break.
_ASSERTION = re.compile(
    r"(?m)^\s*(?:assert\b|self\.assert\w*\s*\(|pytest\.raises\s*\(|with\s+pytest\.raises)"
)

#: Boundary inputs the contract asks the probe to exercise.
_BOUNDARY_TOKENS: tuple[str, ...] = (
    "[]",
    "{}",
    '""',
    "''",
    "none",
    "empty",
    "duplicate",
    "boundary",
    "maxsize",
    "negative",
    "(0",
    " 0",
    "0)",
    "0,",
    "[0",
    "(1",
    " 1",
    "1)",
    "1,",
    "-1",
    "range(",
)

#: Failure kinds that will not improve by asking again.
_FATAL_KINDS = (FailureKind.TIMEOUT, FailureKind.CANCELLED, FailureKind.NOT_CONFIGURED)


def has_assertion(content: str) -> bool:
    return bool(_ASSERTION.search(content or ""))


def has_boundary(content: str) -> bool:
    low = (content or "").lower()
    return any(token in low for token in _BOUNDARY_TOKENS)


def inspect_probe(spec: ProbeSpec) -> list[str]:
    """Everything wrong with a proposed probe, as human-readable problems."""
    issues: list[str] = []
    rel_path = ""
    try:
        rel_path = probe_relative_path(spec.test_path)
    except SandboxError as exc:
        issues.append(str(exc))
    content = spec.content or ""
    try:
        compile(content, "<probe>", "exec")
    except SyntaxError as exc:
        issues.append(f"the probe does not compile: {exc.msg} (line {exc.lineno})")
    except ValueError as exc:
        issues.append(f"the probe cannot be compiled: {exc}")
    if not has_assertion(content):
        issues.append(
            "the probe has no assertion on a return value or on state; a probe that cannot fail is not a probe"
        )
    if not has_boundary(content):
        issues.append(
            "the probe does not exercise a boundary input (empty, zero, one, None, duplicate, maximum)"
        )
    if rel_path and spec.framework == "pytest" and not content.strip():
        issues.append("the probe is empty")
    return issues


def _as_spec(answer: Any) -> ProbeSpec | None:
    value = getattr(answer, "value", answer)
    if isinstance(value, ProbeSpec):
        return value
    try:
        return ProbeSpec.model_validate(value)
    except Exception:
        return None


def synthesize(
    candidate: Candidate,
    context: ContextSlice,
    llm,
    settings: Settings,
    *,
    project_files: Sequence[str] = (),
    max_attempts: int = MAX_ATTEMPTS,
    validate: Callable[[ProbeSpec], str] | None = None,
) -> tuple[ProbeSpec | None, list[str]]:
    """Ask for a probe, check it, and retry with the failure fed back.

    Returns ``(spec, problems)``.  ``spec`` is ``None`` when no attempt produced
    a probe that compiled and asserted; ``problems`` records every attempt so
    the receipt can say exactly why the stage produced no mechanical signal.
    """
    configured = int(getattr(settings, "probe_attempts", MAX_ATTEMPTS) or MAX_ATTEMPTS)
    attempts = max(1, min(MAX_ATTEMPTS, int(max_attempts), configured))
    files = [str(path) for path in (project_files or ())][:200]
    problems: list[str] = []
    previous = ""
    for attempt in range(1, attempts + 1):
        prompt = probe_prompt(
            candidate,
            context,
            attempt=attempt,
            previous_failure=previous,
            project_files=files,
        )
        try:
            answer = llm.complete("probe", prompt, ProbeSpec, stage_model=settings.llm.probe)
        except LLMError as exc:
            problems.append(f"attempt {attempt}: the model call failed ({exc.kind.value}: {exc.message})")
            previous = f"attempt {attempt} returned no probe ({exc.kind.value}); do not repeat that answer."
            if exc.kind in _FATAL_KINDS:
                break
            continue
        except Exception as exc:  # a broken transport is an attempt failure, not a crash
            problems.append(f"attempt {attempt}: the model call raised {type(exc).__name__}: {exc}")
            previous = f"attempt {attempt} returned no probe ({type(exc).__name__}); do not repeat that answer."
            continue
        spec = _as_spec(answer)
        if spec is None:
            previous = "the answer was not a ProbeSpec object"
            problems.append(f"attempt {attempt}: {previous}")
            continue
        issues = inspect_probe(spec)
        if validate is not None and not issues:
            try:
                verdict = str(validate(spec) or "").strip()
            except Exception as exc:  # a validator bug is an attempt failure, not a crash
                verdict = f"the validator raised {type(exc).__name__}: {exc}"
            if verdict:
                issues.append(verdict)
        if issues:
            previous = "; ".join(issues)
            problems.append(f"attempt {attempt}: {previous}")
            continue
        return spec.model_copy(update={"test_path": probe_relative_path(spec.test_path)}), problems
    return None, problems
