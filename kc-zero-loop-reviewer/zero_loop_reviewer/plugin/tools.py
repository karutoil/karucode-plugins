"""Tool and panel handlers exposed by the plugin."""

from __future__ import annotations

import os
import re
from dataclasses import replace
from pathlib import Path
from typing import Any, Mapping

from ..config import Settings, load_settings, settings_from_plugin_env
from ..models import ReviewVerdict, StackedBranch
from ..pipeline import ReviewRequest, Reviewer
from ..providers.base import GitProvider, ProviderError, build_provider
from ..stage5 import formatter as formatter_mod
from .protocol import HostClient, HostError, ToolResult

_SAFE_ID = re.compile(r"^[A-Za-z0-9_.-]{1,120}$")
_RECORD_ID = re.compile(r"^[a-f0-9]{8,32}$")


def resolve_settings(host: HostClient | None = None, config_file: Path | None = None) -> Settings:
    """Host settings (authoritative) over the environment blob over defaults."""
    merged: dict[str, Any] = dict(settings_from_plugin_env())
    if host is not None:
        try:
            answer = host.call("plugin/settings/get", {}, timeout=10.0)
            if isinstance(answer, dict) and isinstance(answer.get("settings"), dict):
                merged.update(answer["settings"])
        except (TimeoutError, HostError, OSError):
            pass
    return load_settings(merged, os.environ, config_file)


def _workspace(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> Path:
    raw = str(arguments.get("workspace") or context.get("workspace") or "").strip()
    if not raw:
        raise ValueError("workspace is required (pass it explicitly or run from a project session)")
    path = Path(raw).expanduser().resolve()
    if not path.is_dir():
        raise ValueError(f"workspace does not exist: {path}")
    return path


def _provider(settings: Settings) -> GitProvider | None:
    if settings.ci.provider == "none":
        return None
    return build_provider(settings, os.environ)


def render_for_posting(verdict: ReviewVerdict, settings: Settings) -> tuple[str, Any]:
    """Render one review; an approve decision becomes a COMMENT, not an approval.

    A bot must never satisfy a branch-protection approval requirement by
    approving its own review: only a human can approve.
    """
    payload = formatter_mod.render_review(verdict, bot_login=settings.gate.bot_login)
    if payload.event == "APPROVE":
        payload = replace(payload, event="COMMENT")
    return payload.body, payload


def summary(verdict: ReviewVerdict, *, body: str = "") -> str:
    lines = [
        f"Review `{verdict.review_id}` — **{verdict.decision.value.upper()}**",
        "",
        f"- base `{verdict.base_sha[:12]}` -> head `{verdict.head_sha[:12]}`",
        f"- p_combined `{verdict.p_combined:.4f}`; threshold applied in host code",
        f"- findings published: {len(verdict.findings)}; suppressed: {verdict.suppressed_count}; "
        f"redacted: {verdict.redacted_count}",
        f"- elapsed: {verdict.elapsed_ms} ms" + (" (budget exhausted)" if verdict.budget_exceeded else ""),
        f"- receipts: {len(verdict.receipts)}",
    ]
    if verdict.degraded:
        lines.append("")
        lines.append("Degraded modes:")
        lines.extend(f"- {reason}" for reason in verdict.degraded[:20])
    if body.strip():
        lines.extend(["", "---", "", body.strip()])
    return "\n".join(lines)


def _verdict_meta(verdict: ReviewVerdict) -> dict[str, Any]:
    return {
        "review_id": verdict.review_id,
        "decision": verdict.decision.value,
        "base_sha": verdict.base_sha,
        "head_sha": verdict.head_sha,
        "p_combined": verdict.p_combined,
        "tau": verdict.tau,
        "findings": [finding.model_dump(mode="json") for finding in verdict.findings],
        "suppressed_count": verdict.suppressed_count,
        "redacted_count": verdict.redacted_count,
        "receipts": len(verdict.receipts),
        "degraded": verdict.degraded,
        "elapsed_ms": verdict.elapsed_ms,
        "budget_exceeded": verdict.budget_exceeded,
        "private_log_path": verdict.private_log_path,
        "input_sha256": verdict.input_sha256,
    }


def tool_review_pr(arguments: dict[str, Any], context: dict[str, Any], host: HostClient) -> ToolResult:
    try:
        settings = resolve_settings(host)
        workspace = _workspace(arguments, context)
        provider = _provider(settings)
        post = bool(arguments.get("post", False))
        repository = str(arguments.get("repository", "") or "")
        pull_number = int(arguments.get("pull_number", 0) or 0)
        if post and (provider is None or not repository or not pull_number):
            return ToolResult(
                ok=False,
                content="post=true requires ci.provider, repository and pull_number",
            )
        stacked: list[StackedBranch] = []
        for row in arguments.get("stacked_branches") or []:
            if not isinstance(row, dict):
                continue
            branch = str(row.get("branch", "") or "").strip()
            if not branch:
                continue
            stacked.append(
                StackedBranch(
                    branch=branch,
                    sha=str(row.get("sha", "") or "").strip(),
                    role="intermediate",
                )
            )
        request = ReviewRequest(
            workspace=workspace,
            base_ref=str(arguments.get("base", "") or ""),
            head_ref=str(arguments.get("head", "HEAD") or "HEAD"),
            expected_head_sha=str(arguments.get("expected_head_sha", "") or ""),
            repository=repository,
            pull_number=pull_number,
            description=str(arguments.get("description", "") or ""),
            actor_login=str(context.get("actor_login", "") or ""),
            stacked=tuple(stacked),
            provider=provider,
        )
        reviewer = Reviewer(settings, environ=os.environ)
        verdict = reviewer.review(request)
        body, payload = render_for_posting(verdict, settings)
        posted = False
        if post:
            provider.post_review(repository, pull_number, payload)
            posted = True
        meta = _verdict_meta(verdict)
        meta["posted"] = posted
        details = []
        for finding in verdict.findings:
            if finding.suggestion:
                details.append(f"### {finding.title}\n{finding.root_cause}\n\n{finding.suggestion}")
        content = summary(verdict, body=body)
        if details:
            content += "\n\n## Committable suggestions\n\n" + "\n\n".join(details)
        return ToolResult(ok=True, content=content[:20_000], meta=meta)
    except (ProviderError, ValueError) as exc:
        return ToolResult(ok=False, content=f"review_pr: {type(exc).__name__}: {exc}")
    except Exception as exc:  # never let a tool crash the plugin
        return ToolResult(ok=False, content=f"review_pr failed: {type(exc).__name__}: {exc}")


def _latest_review(receipts_dir: Path) -> Path | None:
    if not receipts_dir.is_dir():
        return None
    candidates = [path for path in receipts_dir.iterdir() if path.is_dir() and (path / "verdict.json").is_file()]
    if not candidates:
        return None
    return max(candidates, key=lambda path: path.stat().st_mtime)


def tool_review_status(arguments: dict[str, Any], context: dict[str, Any], host: HostClient) -> ToolResult:
    try:
        settings = resolve_settings(host)
        review_id = str(arguments.get("review_id", "") or "").strip()
        if review_id and not _SAFE_ID.match(review_id):
            return ToolResult(ok=False, content="invalid review_id")
        directory = settings.paths.receipts_dir / review_id if review_id else _latest_review(settings.paths.receipts_dir)
        if directory is None or not (directory / "verdict.json").is_file():
            return ToolResult(ok=False, content="no review verdict found")
        verdict = ReviewVerdict.model_validate_json((directory / "verdict.json").read_text(encoding="utf-8"))
        from ..stage1.receipts import read_chain, verify_chain

        chain_path = directory / "receipts.jsonl"
        chain_ok, why = (False, "no receipts")
        if chain_path.is_file():
            chain_ok, why = verify_chain(read_chain(chain_path))
        return ToolResult(
            ok=True,
            content=summary(verdict) + f"\n\nReceipt chain: {'valid' if chain_ok else 'BROKEN'} ({why})",
            meta={**_verdict_meta(verdict), "receipt_chain_valid": chain_ok},
        )
    except Exception as exc:
        return ToolResult(ok=False, content=f"review_status failed: {type(exc).__name__}: {exc}")


def tool_explain_finding(arguments: dict[str, Any], context: dict[str, Any], host: HostClient) -> ToolResult:
    try:
        settings = resolve_settings(host)
        review_id = str(arguments.get("review_id", "") or "")
        finding_id = str(arguments.get("finding_id", "") or "")
        if not _SAFE_ID.match(review_id) or not _RECORD_ID.match(finding_id):
            return ToolResult(ok=False, content="invalid review_id or finding_id")
        private_dir = settings.paths.state_dir / "private"
        stem = Path(settings.redaction.private_log_name).stem or "security-private"
        path = private_dir / f"{stem}-{finding_id}.json"
        if not path.is_file():
            return ToolResult(ok=False, content="no private record with that id")
        mode = oct(path.stat().st_mode & 0o777)
        return ToolResult(
            ok=True,
            content=path.read_text(encoding="utf-8")[:20_000],
            meta={"path": str(path), "mode": mode},
        )
    except Exception as exc:
        return ToolResult(ok=False, content=f"explain_finding failed: {type(exc).__name__}: {exc}")


def panel_review_status(params: dict[str, Any]) -> dict[str, Any]:
    settings = resolve_settings(None)
    latest = _latest_review(settings.paths.receipts_dir)
    if latest is None:
        return {"title": "Zero-loop review", "data": {"reviews": 0, "message": "no review has run yet"}}
    try:
        verdict = ReviewVerdict.model_validate_json((latest / "verdict.json").read_text(encoding="utf-8"))
        data = {
            "review_id": verdict.review_id,
            "decision": verdict.decision.value,
            "head_sha": verdict.head_sha,
            "p_combined": verdict.p_combined,
            "findings": len(verdict.findings),
            "suppressed": verdict.suppressed_count,
            "redacted": verdict.redacted_count,
            "elapsed_ms": verdict.elapsed_ms,
            "degraded": verdict.degraded[:8],
        }
    except Exception as exc:
        data = {"review_id": latest.name, "error": f"{type(exc).__name__}: {exc}"}
    return {"title": "Zero-loop review", "data": data}


def tools() -> dict[str, Any]:
    return {
        "review_pr": tool_review_pr,
        "review_status": tool_review_status,
        "explain_finding": tool_explain_finding,
    }


def panels() -> dict[str, Any]:
    return {"review_status": panel_review_status}
