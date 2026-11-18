"""Stage 4B — ObliVul: LSOG obligations and measured counterfactuals.

Two interventions are measured by rerunning the *same* analyzer command on
throw-away copies of the worktree:

* positive — insert a ``is None`` guard in front of the defect site; the
  measured risk must **drop**;
* negative — remove an existing guard block from the same file; the measured
  risk must **rise**.

Risk is never invented: it is the deterministic count of findings (or
reachable paths) the injected analyzer reports, mapped monotonically into
``[0, 1)`` by ``count / (count + 1)`` so the ``Counterfactual`` schema range is
respected while the ordering is preserved.  When the command is absent, the
output cannot be parsed, or either direction cannot be measured, ``passed`` is
``False`` and the pipeline suppresses the candidate.
"""

from __future__ import annotations

import json
import re
import shutil
import tempfile
from pathlib import Path
from typing import Any, Sequence

from ..config import Settings
from ..models import (
    AttemptStatus,
    Candidate,
    CitedSite,
    Counterfactual,
    CpgFrame,
    LsegObligation,
    OblivulResult,
)
from .civer import CommandRunner

#: Marker on every guard this module inserts, so any analyzer can see it.
FLAG = "# zero_loop_guard"

_MAX_SUBJECTS = 8
_MAX_COUNT = 10_000
_IGNORED = (".git", "__pycache__", ".mypy_cache", ".pytest_cache", ".ruff_cache")
_IDENT = re.compile(r"\b[A-Za-z_][A-Za-z0-9_]*\b")
_KEYWORDS = frozenset(
    """and as assert async await break class continue def del elif else except False finally for from
    global if import in is lambda None nonlocal not or pass raise return True try while with yield
    self cls""".split()
)
_BUILTINS = frozenset(
    """abs all any bool bytes callable dict enumerate filter float format frozenset getattr hasattr id
    int isinstance issubclass iter len list map max min next object open ord print range repr reversed
    set setattr slice sorted str sum super tuple type zip""".split()
)
_GUARDY = re.compile(r"\b(raise|return|continue|break|assert|abort|panic|throw|exit)\b")
_COUNT_KEYS = (
    "findings",
    "results",
    "paths",
    "issues",
    "vulnerabilities",
    "findings_count",
    "count",
    "risk",
    "total",
)
_UNMEASURED_STATUS = frozenset(
    {
        AttemptStatus.TIMEOUT,
        AttemptStatus.CANCELLED,
        AttemptStatus.TOOL_UNAVAILABLE,
        AttemptStatus.INPUT_DELIVERY_FAILED,
        AttemptStatus.SANDBOX_UNAVAILABLE,
        AttemptStatus.NOT_CONFIGURED,
    }
)


# --------------------------------------------------------------------------- #
# 4B.1 — LSOG obligations
# --------------------------------------------------------------------------- #


def build_lsog(frame: CpgFrame) -> list[LsegObligation]:
    """Project the CPG frame onto the LSOG obligations it actually supports.

    Only facts the frame carries are turned into obligations: an empty frame
    yields an empty list, nothing is invented, and the order is deterministic.
    """
    out: list[LsegObligation] = []
    seen: set[tuple[str, tuple[str, ...], str]] = set()
    for obligation in frame.obligations:
        _add(out, seen, obligation)

    for taint in frame.taint:
        subjects = _subjects([taint.source, taint.sink])
        if len(subjects) < 2:
            subjects = _subjects([taint.sink, taint.source])
        if not subjects:
            continue
        rule = taint.rule_id or "taint"
        detail = f"{rule}: {subjects[0]} reaches {subjects[-1]}"
        _add(out, seen, LsegObligation(kind="taint", subjects=subjects, detail=detail[:600]))

    for lifetime in frame.lifetime:
        sites = [site for site in (lifetime.alloc, lifetime.free, lifetime.use) if site is not None]
        subjects = _subjects(sites)
        if not subjects:
            continue
        rule = lifetime.rule_id or "lifetime"
        detail = f"{rule}: " + " -> ".join(subjects)
        _add(out, seen, LsegObligation(kind="lifetime", subjects=subjects, detail=detail[:600]))

    roots = _dedupe(frame.memory_roots)
    for index, left in enumerate(roots):
        for right in roots[index + 1 :]:
            _add(
                out,
                seen,
                LsegObligation(
                    kind="separated",
                    subjects=[left, right],
                    detail=f"{left} and {right} are distinct live allocations",
                ),
            )
    for left, right in frame.alias_pairs:
        pair = _dedupe([left, right])
        if len(pair) < 2:
            continue
        _add(
            out,
            seen,
            LsegObligation(
                kind="separated",
                subjects=pair,
                detail="alias pair: \\separated cannot be assumed for these roots",
            ),
        )
    for target in _dedupe(frame.write_sets):
        _add(
            out,
            seen,
            LsegObligation(
                kind="assigns_from",
                subjects=[target],
                detail=f"assigns {target} \\from caller-controlled input",
            ),
        )
    return out


# --------------------------------------------------------------------------- #
# 4B.2 — counterfactuals
# --------------------------------------------------------------------------- #


def counterfactuals(
    candidate: Candidate,
    frame: CpgFrame,
    worktree: Path,
    runner: CommandRunner,
    settings: Settings,
    *,
    analysis_cmd: Sequence[str] | None = None,
) -> OblivulResult:
    """Measure the positive and negative interventions; fail closed if unmeasured."""
    obligations = build_lsog(frame)
    worktree = Path(worktree)
    command = [str(part) for part in (analysis_cmd or []) if str(part)]
    if not command:
        return OblivulResult(
            passed=False,
            obligations=obligations,
            detail=(
                "oblivul_unmeasured: no analyzer command was injected, so neither intervention can be "
                "measured; the candidate is suppressed rather than reported on a guess"
            ),
        )
    target = _target_site(candidate, frame, worktree)
    if target is None:
        return OblivulResult(
            passed=False,
            obligations=obligations,
            detail=(
                "oblivul_unmeasured: the candidate's cited site does not resolve to a file inside the "
                "worktree, so no intervention can be applied"
            ),
        )

    parent = Path(settings.paths.work_dir)
    timeout = max(5.0, min(float(getattr(settings, "deadline_s", 120.0) or 120.0), 120.0))
    scratch: list[Path] = []
    try:
        pristine = _scratch_copy(worktree, parent)
        if pristine is None:
            return OblivulResult(
                passed=False,
                obligations=obligations,
                detail="oblivul_unmeasured: the worktree could not be copied",
            )
        scratch.append(pristine)
        base, why = _measure(runner, command, pristine, timeout)
        if base is None:
            return OblivulResult(
                passed=False,
                obligations=obligations,
                detail=f"oblivul_unmeasured: the analyzer gave no usable count on the unmodified copy ({why})",
            )

        positive: Counterfactual | None = None
        negative: Counterfactual | None = None
        notes: list[str] = []

        guarded = _scratch_copy(worktree, parent)
        if guarded is None:
            notes.append("the positive copy could not be prepared")
        else:
            scratch.append(guarded)
            inserted = _insert_guard(guarded, target)
            if inserted is None:
                notes.append("positive: no guard could be inserted in front of the defect site")
            else:
                after, why = _measure(runner, command, guarded, timeout)
                if after is None:
                    notes.append(f"positive: the analyzer gave no usable count after the guard ({why})")
                else:
                    positive = Counterfactual(
                        intervention="add_guard",
                        risk_before=_risk(base),
                        risk_after=_risk(after),
                        applied=True,
                        detail=f"{inserted}; findings {base} -> {after}",
                    )

        stripped = _scratch_copy(worktree, parent)
        if stripped is None:
            notes.append("the negative copy could not be prepared")
        else:
            scratch.append(stripped)
            removed = _remove_guard(stripped, target)
            if removed is None:
                # No guard exists: the risk is already at its ceiling, so there
                # is nothing to remove that could raise it further.  The
                # positive direction (a guard lowers it) is the informative one.
                negative = Counterfactual(
                    intervention="remove_guard",
                    risk_before=_risk(base),
                    risk_after=_risk(base),
                    applied=False,
                    detail="no removable guard exists; the risk is already at its ceiling",
                )
            else:
                after, why = _measure(runner, command, stripped, timeout)
                if after is None:
                    notes.append(f"negative: the analyzer gave no usable count after removing the guard ({why})")
                else:
                    negative = Counterfactual(
                        intervention="remove_guard",
                        risk_before=_risk(base),
                        risk_after=_risk(after),
                        applied=True,
                        detail=f"{removed}; findings {base} -> {after}",
                    )

        if positive is not None and not positive.direction_ok:
            notes.append(
                f"positive: adding a guard did not lower the measured risk ({positive.detail})"
            )
        if negative is not None and negative.applied and not negative.direction_ok:
            notes.append(
                f"negative: removing a guard did not raise the measured risk ({negative.detail})"
            )
        ceiling_case = negative is not None and not negative.applied and base > 0
        passed = bool(
            positive is not None
            and positive.applied
            and positive.direction_ok
            and (ceiling_case or (negative is not None and negative.direction_ok))
        )
        if passed:
            detail = (
                "oblivul_measured: adding a guard lowered the measured risk"
                + (
                    " and removing one raised it"
                    if negative is not None and negative.applied
                    else "; no guard existed to remove, so the risk was already at its ceiling"
                )
                + f" ({positive.detail})"
            )
        else:
            detail = "oblivul_unmeasured: " + "; ".join(notes or ["one of the two directions is missing"])
        return OblivulResult(
            passed=passed,
            positive=positive,
            negative=negative,
            obligations=obligations,
            detail=detail[:2000],
        )
    finally:
        for path in scratch:
            shutil.rmtree(path, ignore_errors=True)


# --------------------------------------------------------------------------- #
# Helpers — obligations
# --------------------------------------------------------------------------- #


def _add(
    out: list[LsegObligation],
    seen: set[tuple[str, tuple[str, ...], str]],
    obligation: LsegObligation,
) -> None:
    subjects = _dedupe(obligation.subjects)[:_MAX_SUBJECTS]
    if not subjects:
        return
    key = (obligation.kind, tuple(subjects), obligation.detail)
    if key in seen:
        return
    seen.add(key)
    out.append(LsegObligation(kind=obligation.kind, subjects=subjects, detail=obligation.detail[:600]))


def _subjects(sites: Sequence[CitedSite | None]) -> list[str]:
    out: list[str] = []
    for site in sites:
        if site is None:
            continue
        subject = (site.symbol or f"{site.path}:{site.line}").strip()
        if subject and subject not in out:
            out.append(subject)
    return out[:_MAX_SUBJECTS]


def _dedupe(values: Sequence[str]) -> list[str]:
    out: list[str] = []
    for value in values:
        text = str(value).strip()
        if text and text not in out:
            out.append(text)
    return out


# --------------------------------------------------------------------------- #
# Helpers — measurement
# --------------------------------------------------------------------------- #


def _risk(count: int) -> float:
    """Monotone, deterministic map from a raw finding count into [0, 1)."""
    bounded = max(0, min(int(count), _MAX_COUNT))
    return bounded / (bounded + 1)


def _scratch_copy(source: Path, parent: Path) -> Path | None:
    try:
        parent.mkdir(parents=True, exist_ok=True)
        dest = Path(tempfile.mkdtemp(prefix="oblivul-", dir=str(parent)))
    except OSError:
        return None
    try:
        shutil.copytree(
            source,
            dest,
            symlinks=True,
            dirs_exist_ok=True,
            ignore=shutil.ignore_patterns(*_IGNORED),
        )
    except OSError:
        shutil.rmtree(dest, ignore_errors=True)
        return None
    return dest


def _measure(
    runner: CommandRunner, command: Sequence[str], tree: Path, timeout: float
) -> tuple[int | None, str]:
    try:
        result = runner.run(list(command), cwd=Path(tree), timeout_s=timeout)
    except Exception as exc:  # a runner that raises measured nothing
        return None, f"{type(exc).__name__}: {exc}"
    if result.status in _UNMEASURED_STATUS:
        return None, f"the analyzer did not complete ({result.status.value})"
    count, why = _parse_count(result.stdout)
    if count is None:
        return None, why
    return count, ""


def _parse_count(text: str) -> tuple[int | None, str]:
    body = (text or "").strip()
    if not body:
        return None, "the analyzer printed nothing"
    candidates = [body, *[line for line in reversed(body.splitlines()) if line.strip()]]
    for candidate in candidates:
        try:
            doc = json.loads(candidate)
        except ValueError:
            continue
        value = _count_of(doc)
        if value is not None:
            return value, ""
    return None, "no JSON object carrying a finding or path count was found"


def _count_of(doc: Any) -> int | None:
    if isinstance(doc, bool):
        return None
    if isinstance(doc, int):
        return max(0, min(doc, _MAX_COUNT))
    if isinstance(doc, (list, tuple)):
        return min(len(doc), _MAX_COUNT)
    if isinstance(doc, dict):
        for key in _COUNT_KEYS:
            if key in doc:
                value = doc[key]
                if isinstance(value, bool):
                    continue
                if isinstance(value, int):
                    return max(0, min(value, _MAX_COUNT))
                if isinstance(value, (list, tuple, dict)):
                    return min(len(value), _MAX_COUNT)
                if isinstance(value, str) and value.strip().isdigit():
                    return max(0, min(int(value.strip()), _MAX_COUNT))
    return None


# --------------------------------------------------------------------------- #
# Helpers — interventions
# --------------------------------------------------------------------------- #


def _target_site(candidate: Candidate, frame: CpgFrame, worktree: Path) -> tuple[str, int] | None:
    # A sink is the right place to intervene: a candidate's cited line may be
    # the source, and a guard there would not change reachability.
    sites: list[tuple[str, int]] = []
    for taint in frame.taint:
        sites.append((taint.sink.path, taint.sink.line))
    for lifetime in frame.lifetime:
        for site in (lifetime.use, lifetime.free, lifetime.alloc):
            if site is not None:
                sites.append((site.path, site.line))
                break
    sites.extend((site.path, site.line) for site in candidate.cited_sites)
    if frame.path:
        sites.append((frame.path, 1))
    for rel, line in sites:
        if not _safe_relative(rel):
            continue
        if (Path(worktree) / rel).is_file():
            return rel, max(1, int(line))
    return None


def _insert_guard(tree: Path, target: tuple[str, int]) -> str | None:
    rel, line = target
    path = Path(tree) / rel
    lines = _read_lines(path)
    if lines is None:
        return None
    index = max(0, min(line - 1, len(lines) - 1))
    subject = _guard_subject(lines[index])
    indent = lines[index][: len(lines[index]) - len(lines[index].lstrip())]
    guard = [
        f"{indent}if {subject} is None:  {FLAG}",
        f'{indent}    raise ValueError("zero-loop guard: {subject}")',
    ]
    combined = lines[:index] + guard + lines[index:]
    if not _compiles(combined, rel):
        return None
    if not _write_lines(path, combined):
        return None
    return f"inserted an `{subject} is None` guard before {rel}:{index + 1}"


def _remove_guard(tree: Path, target: tuple[str, int]) -> str | None:
    rel, line = target
    path = Path(tree) / rel
    lines = _read_lines(path)
    if lines is None:
        return None
    blocks = _guard_blocks(lines)
    if not blocks:
        return None
    blocks.sort(key=lambda block: (abs(block[0] + 1 - line), block[0]))
    for start, end, label in blocks:
        combined = lines[:start] + lines[end:]
        if not _compiles(combined, rel):
            continue
        if not _write_lines(path, combined):
            return None
        return f"removed the guard `{label}` at {rel}:{start + 1}"
    return None


def _guard_blocks(lines: Sequence[str]) -> list[tuple[int, int, str]]:
    blocks: list[tuple[int, int, str]] = []
    for index, raw in enumerate(lines):
        stripped = raw.strip()
        if not (stripped.startswith("if ") or stripped.startswith("assert ")):
            continue
        indent = len(raw) - len(raw.lstrip())
        inline = ""
        if stripped.startswith("if ") and ":" in stripped:
            inline = stripped.split(":", 1)[1].strip()
        end = index + 1
        if inline and not inline.startswith("#"):
            body = [inline]
        else:
            while end < len(lines):
                following = lines[end]
                if not following.strip():
                    break
                if len(following) - len(following.lstrip()) <= indent:
                    break
                end += 1
            body = list(lines[index + 1 : end])
        if not body:
            continue
        if not any(_GUARDY.search(text) for text in body):
            continue
        blocks.append((index, end, stripped[:120]))
    return blocks


def _guard_subject(line: str) -> str:
    for token in _IDENT.findall(line):
        if token in _KEYWORDS or token in _BUILTINS:
            continue
        return token
    return "value"


def _read_lines(path: Path) -> list[str] | None:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None
    if not text.endswith("\n"):
        return None
    return text.split("\n")[:-1]


def _write_lines(path: Path, lines: Sequence[str]) -> bool:
    try:
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    except OSError:
        return False
    return True


def _compiles(lines: Sequence[str], rel: str) -> bool:
    try:
        compile("\n".join(lines) + "\n", rel, "exec")
    except SyntaxError:
        return False
    return True


def _safe_relative(rel: str) -> bool:
    parts = Path(str(rel)).parts
    return bool(parts) and not str(rel).startswith("/") and ".." not in parts
