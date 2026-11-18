"""Stage 2A — the Semgrep delta.

Semgrep runs through :func:`zero_loop_reviewer.stage1.contract.run_bounded`, so
the analyzer gets a hard deadline, a write-flag guard and a typed outcome; the
JSON it prints is validated before anything is believed.  Everything Semgrep
returns is kept: findings on unchanged lines get ``patch_caused=False`` instead
of disappearing, because a silently dropped signal cannot be audited.

Suppressions are host-side and deterministic (path allow-list, an inline marker
on the changed line, or a trusted sanitizer on the finding's own line).  A
suppressed finding is still returned, with ``suppressed=True`` and a reason.
"""

from __future__ import annotations

import json
import os
import re
from functools import lru_cache
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping, Sequence

from ..config import SastConfig, Settings
from ..errors import ToolError
from ..models import AnalyzerFinding, AttemptStatus, DiffBundle, sha256_hex
from ..stage1.contract import run_bounded

#: Rules shipped with the plugin, relative to the package root.
DEFAULT_RULES = Path(__file__).resolve().parents[2] / "rules"

_SEVERITY = {"ERROR": 3, "WARNING": 2, "INFO": 1}
_CWE = re.compile(r"CWE-\d+")
_INLINE_MARKERS = ("nosec", "semgrep-ignore", "zero-loop:ignore", "zero-loop: ignore")
_INJECTION_CWES = ("CWE-78", "CWE-79", "CWE-89", "CWE-94", "CWE-95")
_TRACKS = ("pattern", "taint", "lifetime")


@dataclass
class SastOutcome:
    findings: list[AnalyzerFinding] = field(default_factory=list)
    status: AttemptStatus = AttemptStatus.SKIPPED
    detail: str = ""
    raw_sha256: str = ""


# --------------------------------------------------------------------------- #
# Entry point
# --------------------------------------------------------------------------- #


def run_semgrep(
    repo_root: Path,
    diff: DiffBundle,
    settings: Settings,
    *,
    environ: Mapping[str, str] | None = None,
) -> SastOutcome:
    """Run Semgrep over the changed files of ``repo_root`` and parse the delta."""
    config = settings.sast
    root = Path(repo_root)
    if not config.enabled:
        return SastOutcome(status=AttemptStatus.SKIPPED, detail="sast.enabled is false")
    if not root.is_dir():
        return SastOutcome(
            status=AttemptStatus.INPUT_DELIVERY_FAILED,
            detail=f"the review worktree {root} does not exist",
        )
    targets = _targets(root, diff)
    if not targets:
        return SastOutcome(status=AttemptStatus.SKIPPED, detail="no changed file exists in the worktree to scan")
    try:
        rules = _config_value(config)
    except ToolError as exc:
        return SastOutcome(status=AttemptStatus.TOOL_UNAVAILABLE, detail=str(exc))
    timeout_s = max(1.0, float(config.timeout_s))
    results: list[dict[str, Any]] = []
    errors: list[Any] = []
    raw_parts: list[str] = []
    chunks = _target_chunks(targets)
    # Divide the analyzer budget across chunks; one slow chunk must not consume
    # the whole Stage 2 window and starve the other changed files.
    chunk_timeout = max(5.0, timeout_s / max(1, len(chunks)))
    for index, chunk in enumerate(chunks, 1):
        argv = [
            config.semgrep_bin,
            "scan",
            "--json",
            "--metrics=off",
            "--disable-version-check",
            "--quiet",
            "--timeout",
            str(max(1, int(chunk_timeout))),
            "--config",
            rules,
            # Scan the changed files themselves.  Passing "." made Semgrep walk
            # the whole repository before applying --include, which is minutes
            # on a monorepo and defeats the CI budget.
            *chunk,
        ]
        parsed: dict[str, Any] = {}

        def _check(text: str) -> tuple[bool, str]:
            try:
                doc = json.loads(text)
            except ValueError as exc:
                return False, f"semgrep output is not JSON: {exc}"
            if not isinstance(doc, dict) or not isinstance(doc.get("results"), list):
                return False, "semgrep JSON has no results array"
            parsed.update(doc)
            return True, ""

        try:
            run = run_bounded(
                argv,
                cwd=root,
                timeout_s=chunk_timeout,
                env=_child_env(environ),
                schema_check=_check,
                read_only_root=root,
                read_only_paths=chunk,
            )
        except FileNotFoundError:
            return SastOutcome(
                status=AttemptStatus.TOOL_UNAVAILABLE,
                detail=f"semgrep binary {config.semgrep_bin!r} is not installed",
            )
        except OSError as exc:
            return SastOutcome(
                status=AttemptStatus.PROCESS_CRASH,
                detail=f"could not start semgrep: {exc}",
            )
        raw_parts.append(run.stdout)
        if run.status is not AttemptStatus.COMPLETED:
            return SastOutcome(
                status=run.status,
                detail=f"chunk {index}/{len(chunks)}: {_failure_detail(run.exit_code, run.stderr, parsed)}",
                raw_sha256=sha256_hex("".join(raw_parts)),
            )
        results.extend(parsed.get("results") or [])
        if isinstance(parsed.get("errors"), list):
            errors.extend(parsed["errors"])
    findings = _findings({"results": results, "errors": errors}, root, config.max_findings)
    detail = f"semgrep: {len(findings)} finding(s) on {len(targets)} changed file(s) in {len(chunks)} chunk(s)"
    if errors:
        parts: list[str] = []
        for item in errors[:3]:
            if isinstance(item, dict):
                parts.append(str(item.get("message") or item))
            elif item:
                parts.append(str(item))
        messages = "; ".join(parts)
        if messages:
            detail = f"{detail}; semgrep reported: {messages[:500]}"
    return SastOutcome(
        findings=findings,
        status=AttemptStatus.COMPLETED,
        detail=detail,
        raw_sha256=sha256_hex("".join(raw_parts)),
    )


def _config_value(config: SastConfig) -> str:
    text = (config.semgrep_config or "").strip()
    if not text:
        if not DEFAULT_RULES.is_dir():
            raise ToolError(f"the shipped Semgrep rules are missing: {DEFAULT_RULES}")
        return str(DEFAULT_RULES)
    looks_like_path = text.endswith((".yml", ".yaml")) or os.sep in text
    if looks_like_path and not Path(text).exists():
        raise ToolError(f"configured Semgrep config does not exist: {text}")
    return text


def _target_chunks(
    targets: Sequence[str],
    *,
    max_files: int = 200,
    max_chars: int = 120_000,
    max_chunks: int = 12,
) -> list[list[str]]:
    """Split a large changed-file list so argv never hits the kernel limit.

    A PR with thousands of files could exceed ``ARG_MAX`` in one Semgrep
    invocation and fail the whole static track.  Twelve chunks of at most 200
    paths keep the command line far below the limit while preserving the
    read-only digest and per-chunk timeout.
    """
    chunks: list[list[str]] = []
    current: list[str] = []
    chars = 0
    for path in targets:
        size = len(path) + 1
        if current and (len(current) >= max_files or chars + size > max_chars):
            chunks.append(current)
            current = []
            chars = 0
        current.append(path)
        chars += size
    if current:
        chunks.append(current)
    if len(chunks) <= max_chunks:
        return chunks
    # Redistribute evenly rather than leaving the tail without a chunk.
    step = max(1, (len(targets) + max_chunks - 1) // max_chunks)
    return [list(targets[i : i + step]) for i in range(0, len(targets), step)][:max_chunks]


def _targets(root: Path, diff: DiffBundle) -> list[str]:
    out: set[str] = set()
    for patch in diff.files:
        if patch.is_deleted or patch.is_binary:
            continue
        rel = patch.path.replace("\\", "/")
        if not rel or rel.startswith("/") or rel.split("/")[0] == "..":
            continue
        if not (root / rel).is_file():
            continue
        out.add(rel)
    return sorted(out)


def _child_env(environ: Mapping[str, str] | None) -> dict[str, str]:
    env = {"SEMGREP_SEND_METRICS": "off", "SEMGREP_ENABLE_VERSION_CHECK": "0"}
    if environ:
        for key, value in environ.items():
            upper = key.upper()
            if key.startswith("SEMGREP_") and "KEY" not in upper and "TOKEN" not in upper and "SECRET" not in upper:
                env[key] = value
    return env


def _failure_detail(exit_code: int | None, stderr: str, parsed: Mapping[str, Any]) -> str:
    errors = parsed.get("errors")
    if isinstance(errors, list) and errors:
        first = errors[0]
        message = first.get("message") if isinstance(first, dict) else str(first)
        if message:
            return f"semgrep exited {exit_code}: {str(message)[:500]}"
    tail = (stderr or "").strip().splitlines()
    return f"semgrep exited {exit_code}: " + (" ".join(tail[-3:])[:500] if tail else "no detail")


# --------------------------------------------------------------------------- #
# Parsing
# --------------------------------------------------------------------------- #


def _findings(doc: Mapping[str, Any], root: Path, limit: int) -> list[AnalyzerFinding]:
    results = doc.get("results")
    if not isinstance(results, list):
        return []
    findings: list[AnalyzerFinding] = []
    seen: set[tuple[str, str, int]] = set()
    for item in results:
        if not isinstance(item, dict):
            continue
        rule_id = _short_rule_id(str(item.get("check_id") or ""))
        path = _relative_path(str(item.get("path") or ""), root)
        start = item.get("start") if isinstance(item.get("start"), dict) else {}
        line = _as_int(start.get("line"))
        if not rule_id or not path or line is None or line < 1:
            continue
        key = (rule_id, path, line)
        if key in seen:
            continue
        seen.add(key)
        end = item.get("end") if isinstance(item.get("end"), dict) else {}
        end_line = _as_int(end.get("line"))
        extra = item.get("extra") if isinstance(item.get("extra"), dict) else {}
        metadata = extra.get("metadata") if isinstance(extra.get("metadata"), dict) else {}
        findings.append(
            AnalyzerFinding(
                analyzer="semgrep",
                rule_id=rule_id,
                path=path,
                line=line,
                end_line=end_line if end_line is not None and end_line >= line else None,
                message=str(extra.get("message") or "")[:2000],
                severity=_SEVERITY.get(str(extra.get("severity") or "").upper(), 1),
                track=_track(metadata),
                cwe=_cwe(metadata.get("cwe")),
                patch_caused=True,
                test_context=_in_test_context(root, path, line),
            )
        )
        if len(findings) >= max(1, limit):
            break
    return findings


def _short_rule_id(check_id: str) -> str:
    marker = "zero-loop."
    if marker in check_id:
        return check_id.rsplit(marker, 1)[-1]
    return check_id


def _relative_path(path: str, root: Path) -> str:
    if not path:
        return ""
    candidate = Path(path)
    if candidate.is_absolute():
        try:
            candidate = candidate.resolve().relative_to(root.resolve())
        except (ValueError, OSError):
            return ""
    text = candidate.as_posix()
    while text.startswith("./"):
        text = text[2:]
    if not text or text.startswith("../"):
        return ""
    return text


def _as_int(value: Any) -> int | None:
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, int):
        return value
    try:
        return int(str(value))
    except (TypeError, ValueError):
        return None


def _track(metadata: Mapping[str, Any]) -> str:
    value = str(metadata.get("zero_loop_track") or "").strip().lower()
    return value if value in _TRACKS else "pattern"


def _cwe(value: Any) -> str:
    candidates: Sequence[Any] = value if isinstance(value, (list, tuple)) else [value]
    for item in candidates:
        if not isinstance(item, str):
            continue
        match = _CWE.search(item)
        if match:
            return match.group(0)
    return ""


# --------------------------------------------------------------------------- #
# Patch causation and suppressions
# --------------------------------------------------------------------------- #


def patch_caused(findings: list[AnalyzerFinding], diff: DiffBundle) -> list[AnalyzerFinding]:
    """Flag each finding against the diff's added lines.  Nothing is dropped."""
    changed = diff.changed_line_map()
    out: list[AnalyzerFinding] = []
    for finding in findings:
        lines = changed.get(finding.path, set())
        hit = finding.line in lines
        if not hit and finding.end_line is not None:
            hit = any(finding.line <= line <= finding.end_line for line in lines)
        out.append(finding.model_copy(update={"patch_caused": hit}))
    return out


def apply_suppressions(
    findings: list[AnalyzerFinding], diff: DiffBundle, settings: SastConfig
) -> list[AnalyzerFinding]:
    """Mark (never remove) findings the host considers noise."""
    texts = _added_line_text(diff)
    out: list[AnalyzerFinding] = []
    for finding in findings:
        reason = ""
        if not finding.suppressed:
            matched = _suppressed_prefix(finding.path, settings.suppressed_paths)
            text = texts.get(finding.path, {}).get(finding.line, "")
            if matched:
                reason = f"path is on the analyzer suppression list ({matched})"
            elif finding.test_context:
                reason = "finding is inside test code"
            elif any(marker in text.lower() for marker in _INLINE_MARKERS):
                reason = "inline suppression marker on the finding line"
            elif finding.cwe in _INJECTION_CWES:
                sanitizer = _sanitizer(text, settings.trusted_sanitizers)
                if sanitizer:
                    reason = f"trusted sanitizer {sanitizer!r} is applied on the finding line"
        if reason:
            out.append(finding.model_copy(update={"suppressed": True, "suppression_reason": reason}))
        else:
            out.append(finding)
    return out


def _suppressed_prefix(path: str, prefixes: Sequence[str]) -> str:
    for prefix in prefixes:
        if not prefix:
            continue
        normalized = prefix.replace("\\", "/").rstrip("/")
        if path == normalized or path.startswith(normalized + "/"):
            return prefix
    return ""


def _sanitizer(text: str, sanitizers: Sequence[str]) -> str:
    for token in sanitizers:
        if token and token in text:
            return token
    return ""


def _added_line_text(diff: DiffBundle) -> dict[str, dict[int, str]]:
    """New-file line numbers of added lines, with their text."""
    out: dict[str, dict[int, str]] = {}
    for patch in diff.files:
        lines: dict[int, str] = {}
        for hunk in patch.hunks:
            cursor = hunk.new_start
            for raw in hunk.lines:
                if raw.startswith("+") and not raw.startswith("+++"):
                    lines[cursor] = raw[1:]
                    cursor += 1
                elif raw.startswith("-") and not raw.startswith("---"):
                    continue
                elif raw.startswith("\\"):
                    continue
                else:
                    cursor += 1
        out[patch.path] = lines
    return out


def _in_test_context(root: Path, path: str, line: int) -> bool:
    """True when a finding sits in test code rather than production code.

    Test doubles intentionally unwrap, assert and index without the production
    guards.  Reporting those as security defects fills the top-3 with noise and
    hides the production finding behind it.
    """
    if _test_path(path):
        return True
    try:
        lines = _source_lines(str((Path(root) / path).resolve()))
    except OSError:
        return False
    suffix = Path(path).suffix.lower()
    if suffix == ".rs":
        return _rust_test_line(lines, line)
    if suffix in (".py", ".pyi"):
        return _python_test_line(lines, line)
    return False


def _test_path(path: str) -> bool:
    low = path.lower().replace("\\", "/")
    name = low.rsplit("/", 1)[-1]
    return (
        low.startswith("tests/")
        or low.startswith("test/")
        or "/tests/" in low
        or "/test/" in low
        or name.startswith("test_")
        or ".test." in low
        or ".spec." in low
        or name.endswith("_test.py")
        or name.endswith("_test.rs")
    )


@lru_cache(maxsize=512)
def _source_lines(path: str) -> tuple[str, ...]:
    return tuple(Path(path).read_text(encoding="utf-8", errors="replace").splitlines())


def _brace_end(lines: Sequence[str], start: int) -> int:
    depth = 0
    opened = False
    for index in range(start, len(lines)):
        depth += lines[index].count("{") - lines[index].count("}")
        opened = opened or "{" in lines[index]
        if opened and depth <= 0:
            return index + 1
    return len(lines)


def _rust_test_line(lines: Sequence[str], line: int) -> bool:
    for index, text in enumerate(lines):
        if "#[cfg(test)]" in text:
            for cursor in range(index, min(index + 5, len(lines))):
                if re.search(r"\bmod\s+\w+", lines[cursor]):
                    start = cursor + 1
                    end = _brace_end(lines, cursor)
                    if start <= line <= end:
                        return True
                    break
        if "#[test]" in text:
            for cursor in range(index, min(index + 4, len(lines))):
                if re.search(r"\bfn\s+\w+", lines[cursor]):
                    start = cursor + 1
                    end = _brace_end(lines, cursor)
                    if start <= line <= end:
                        return True
                    break
    return False


def _python_test_line(lines: Sequence[str], line: int) -> bool:
    for index, text in enumerate(lines):
        match = re.match(r"^(\s*)def\s+test_", text)
        if not match:
            continue
        indent = len(match.group(1))
        end = len(lines)
        for cursor in range(index + 1, len(lines)):
            body = lines[cursor]
            if not body.strip():
                continue
            if len(body) - len(body.lstrip()) <= indent:
                end = cursor
                break
        if index + 1 <= line <= end:
            return True
    return False
