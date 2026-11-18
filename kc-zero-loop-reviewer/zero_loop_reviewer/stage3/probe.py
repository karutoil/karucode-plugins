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

from ..adapters import BUILTIN_ADAPTERS, ProbeAdapter, adapter_for
from ..config import Settings
from ..errors import FailureKind, LLMError, SandboxError
from ..llm.prompts import probe_prompt
from ..models import Candidate, ContextSlice, ProbeSpec
from .sandbox import probe_relative_path

#: The interface caps synthesis at three attempts.
MAX_ATTEMPTS = 3

#: Language-specific test conventions.
_LANGUAGES = {
    ".py": ("python", "pytest"),
    ".pyi": ("python", "pytest"),
    ".rs": ("rust", "cargo"),
    ".go": ("go", "go"),
    ".ts": ("typescript", "node"),
    ".tsx": ("typescript", "node"),
    ".js": ("javascript", "node"),
    ".jsx": ("javascript", "node"),
    ".mjs": ("javascript", "node"),
    ".cjs": ("javascript", "node"),
}


def language_for(path: str) -> tuple[str, str]:
    """Return ``(language, framework)`` for a built-in candidate path."""
    adapter = adapter_for(path, BUILTIN_ADAPTERS)
    if adapter is None:
        return ("unknown", "")
    return (adapter.language, adapter.framework)

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
    low = (content or "").lower()
    return bool(
        _ASSERTION.search(content or "")
        or "assert!" in low
        or "assert_eq!" in low
        or "assert_ne!" in low
        or "assert." in low
        or "assert(" in low
        or "expect(" in low
        or "t.fatal" in low
        or "t.error" in low
    )


def has_boundary(content: str) -> bool:
    low = (content or "").lower()
    return any(token in low for token in _BOUNDARY_TOKENS)


def inspect_probe(spec: ProbeSpec, *, language: str = "", adapter: ProbeAdapter | None = None) -> list[str]:
    """Everything wrong with a proposed probe, as human-readable problems."""
    issues: list[str] = []
    rel_path = ""
    try:
        rel_path = probe_relative_path(
            spec.test_path, extensions=adapter.extensions if adapter is not None else None
        )
    except SandboxError as exc:
        issues.append(str(exc))
    if adapter is None:
        expected = language
        framework = ""
        if not expected:
            builtin = adapter_for(rel_path or spec.test_path, BUILTIN_ADAPTERS)
            expected = builtin.language if builtin is not None else ""
            framework = builtin.framework if builtin is not None else ""
    else:
        expected = adapter.language
        framework = adapter.framework
    if framework and spec.framework in ("", "pytest") and framework != "pytest":
        spec = spec.model_copy(update={"framework": framework})
    content = spec.content or ""
    if adapter is not None and not adapter.builtin:
        if not adapter.accepts(rel_path or spec.test_path):
            issues.append(f"the probe path does not match {adapter.name!r} extensions {list(adapter.extensions)}")
        if adapter.test_marker and adapter.test_marker.lower() not in content.lower():
            issues.append(f"the probe does not contain the {adapter.name!r} test marker {adapter.test_marker!r}")
        if adapter.assertion_marker and adapter.assertion_marker.lower() not in content.lower():
            issues.append(f"the probe does not contain the {adapter.name!r} assertion marker {adapter.assertion_marker!r}")
        if spec.framework != adapter.framework:
            issues.append(f"a {adapter.name!r} probe must use framework={adapter.framework!r}")
    elif expected == "python":
        try:
            compile(content, "<probe>", "exec")
        except SyntaxError as exc:
            issues.append(f"the probe does not compile: {exc.msg} (line {exc.lineno})")
        except ValueError as exc:
            issues.append(f"the probe cannot be compiled: {exc}")
        if spec.framework not in ("pytest", "unittest"):
            issues.append(f"a Python probe cannot use the {spec.framework!r} framework")
    elif expected == "go":
        if spec.framework != "go":
            issues.append("a Go probe must use framework='go'")
        if not rel_path.endswith("_test.go"):
            issues.append("a Go probe must be a *_test.go file")
        if "func Test" not in content:
            issues.append("a Go probe must contain a func Test... function")
    elif expected == "rust":
        if spec.framework != "cargo":
            issues.append("a Rust probe must use framework='cargo'")
        if "#[test]" not in content:
            issues.append("a Rust probe must contain a #[test] function")
        if not rel_path.endswith(".rs") or "/tests/" not in f"/{rel_path}":
            issues.append("a Rust probe must be a tests/*.rs integration test")
    elif expected in ("typescript", "javascript"):
        if spec.framework not in ("node", "tsx"):
            issues.append("a TypeScript/JavaScript probe must use framework='node' or 'tsx'")
        if not any(marker in content for marker in ("node:test", "from 'node:assert'", 'from "node:assert"', "assert.", "expect(")):
            issues.append("a TypeScript/JavaScript probe must assert with node:test/node:assert or expect()")
    else:
        issues.append(f"the probe language is not supported: {rel_path or spec.test_path}")
    if (adapter is None or adapter.builtin) and not has_assertion(content):
        issues.append(
            "the probe has no assertion on a return value or on state; a probe that cannot fail is not a probe"
        )
    if not has_boundary(content):
        issues.append(
            "the probe does not exercise a boundary input (empty, zero, one, None, duplicate, maximum)"
        )
    if rel_path and not content.strip():
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
    language, framework = ("", "")
    adapter: ProbeAdapter | None = None
    if candidate.cited_sites:
        adapter = adapter_for(candidate.cited_sites[0].path, settings.probe_adapters)
        if adapter is not None:
            language, framework = adapter.language, adapter.framework
    problems: list[str] = []
    previous = ""
    for attempt in range(1, attempts + 1):
        prompt = probe_prompt(
            candidate,
            context,
            attempt=attempt,
            previous_failure=previous,
            project_files=files,
            language=language,
            framework=framework,
            adapter=adapter,
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
        if adapter is None:
            adapter = adapter_for(spec.test_path, settings.probe_adapters)
        if adapter is not None:
            language, framework = adapter.language, adapter.framework
        elif not language:
            language, framework = language_for(spec.test_path)
        if framework and spec.framework in ("pytest", "") and framework != "pytest":
            spec = spec.model_copy(update={"framework": framework})
        issues = inspect_probe(spec, language=language, adapter=adapter)
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
        return spec.model_copy(
            update={
                "test_path": probe_relative_path(
                    spec.test_path, extensions=adapter.extensions if adapter is not None else None
                )
            }
        ), problems
    return None, problems
