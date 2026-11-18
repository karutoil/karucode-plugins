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
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping, Sequence

from ..config import SastConfig, Settings
from ..errors import ToolError
from ..models import AnalyzerFinding, AttemptStatus, DiffBundle, sha256_hex
from ..stage1.contract import run_bounded

#: Rules shipped with the plugin, relative to the package root.
DEFAULT_RULES = Path(__file__).resolve().parents[2] / "rules" / "semgrep-python.yml"

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
    argv = [
        config.semgrep_bin,
        "scan",
        "--json",
        "--metrics=off",
        "--disable-version-check",
        "--quiet",
        "--timeout",
        str(max(1, int(timeout_s))),
        "--config",
        rules,
        # Scan the changed files themselves.  Passing "." made Semgrep walk the
        # whole repository before applying --include, which is minutes on a
        # monorepo and defeats the CI budget.
        *targets,
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
            timeout_s=timeout_s,
            env=_child_env(environ),
            schema_check=_check,
            read_only_root=root,
            read_only_paths=targets,
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
    raw_sha256 = sha256_hex(run.stdout)
    if run.status is not AttemptStatus.COMPLETED:
        return SastOutcome(
            status=run.status,
            detail=_failure_detail(run.exit_code, run.stderr, parsed),
            raw_sha256=raw_sha256,
        )
    findings = _findings(parsed, root, config.max_findings)
    detail = f"semgrep {run.exit_code}: {len(findings)} finding(s) on {len(targets)} changed file(s)"
    errors = parsed.get("errors")
    if isinstance(errors, list) and errors:
        parts: list[str] = []
        for item in errors[:3]:
            if isinstance(item, dict):
                parts.append(str(item.get("message") or item))
            elif item:
                parts.append(str(item))
        messages = "; ".join(parts)
        if messages:
            detail = f"{detail}; semgrep reported: {messages[:500]}"
    return SastOutcome(findings=findings, status=AttemptStatus.COMPLETED, detail=detail, raw_sha256=raw_sha256)


def _config_value(config: SastConfig) -> str:
    text = (config.semgrep_config or "").strip()
    if not text:
        if not DEFAULT_RULES.is_file():
            raise ToolError(f"the shipped Semgrep rules are missing: {DEFAULT_RULES}")
        return str(DEFAULT_RULES)
    looks_like_path = text.endswith((".yml", ".yaml")) or os.sep in text
    if looks_like_path and not Path(text).exists():
        raise ToolError(f"configured Semgrep config does not exist: {text}")
    return text


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
