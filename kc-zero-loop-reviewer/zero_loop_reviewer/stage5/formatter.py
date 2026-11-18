"""Stage 5 — one review payload, top-3 cap, verified suggestions only.

A finding is published only when it is verified **and** either carries a
CIVER-verified fix or is a redacted security flag.  Anything else is dropped
with a receipt-able reason (``status=SUPPRESSED`` +
``suppression_reason``), never silently.

The output is exactly one provider review: a summary body that states the
decision and every degraded mode, plus at most three inline comments at cited
lines.  No threads, no author text, no per-comment conversation.
"""

from __future__ import annotations

import re

from ..models import (
    Candidate,
    Decision,
    Finding,
    FindingStatus,
    Fix,
    MonitorScore,
    Redaction,
    ReviewVerdict,
    RiskElicitation,
    SeverityLevel,
    clamp01,
)
from ..providers.base import ReviewComment, ReviewPayload
from .redaction import public_body

#: A review never shows more than this many findings, whatever the caller asks.
MAX_FINDINGS = 3

#: Per-comment body ceiling, independent of the review body ceiling.
MAX_COMMENT_CHARS = 12_000

#: How the review's decision maps onto a provider review event.
EVENTS = {
    Decision.APPROVE: "APPROVE",
    Decision.REQUEST_CHANGES: "REQUEST_CHANGES",
    Decision.HUMAN_REVIEW: "COMMENT",
}


# --------------------------------------------------------------------------- #
# Ranking and selection
# --------------------------------------------------------------------------- #


def _probability(candidate: Candidate) -> float:
    for value in (candidate.p_combined, candidate.p):
        if value is not None:
            return float(value)
    return 0.0


def is_redacted_flag(candidate: Candidate) -> bool:
    """A security finding already marked as redacted by stage 5 redaction."""
    return bool(candidate.security_exploitable) and not bool(candidate.public_safe)


def publishability(candidate: Candidate) -> tuple[bool, str]:
    """``(keep, reason)`` — the reason is recorded on the suppressed candidate."""
    if candidate.status is not FindingStatus.VERIFIED:
        return False, (
            f"status={candidate.status.value}: an unverified candidate is never published, "
            "however good the reasoning behind it"
        )
    if is_redacted_flag(candidate):
        return True, ""
    fix = candidate.verified_fix
    if fix is None:
        return False, "no verified fix: CIVER produced no patch, so there is nothing to suggest"
    if not fix.verified:
        return False, "the proposed fix did not pass CIVER verification"
    if not fix.patch.strip():
        return False, "the CIVER-verified fix carries an empty patch"
    return True, ""


def select_with_receipts(
    findings: list[Candidate], max_findings: int
) -> tuple[list[Candidate], list[Candidate]]:
    """Split into ``(published, suppressed)``; suppressed candidates carry a reason.

    The cap is the smaller of ``max_findings`` and :data:`MAX_FINDINGS`, so a
    caller cannot widen it.  Ranking is ``(severity, p_combined)`` descending.
    """
    cap = max(0, min(int(max_findings), MAX_FINDINGS))
    publishable: list[Candidate] = []
    suppressed: list[Candidate] = []
    for candidate in findings:
        keep, reason = publishability(candidate)
        if keep:
            publishable.append(candidate)
        else:
            suppressed.append(
                candidate.model_copy(
                    update={"status": FindingStatus.SUPPRESSED, "suppression_reason": reason}
                )
            )
    publishable.sort(key=lambda c: (int(c.severity), _probability(c)), reverse=True)
    published = publishable[:cap]
    for candidate in publishable[cap:]:
        suppressed.append(
            candidate.model_copy(
                update={
                    "status": FindingStatus.SUPPRESSED,
                    "suppression_reason": (
                        f"top-{cap} cap: ranked below {cap} higher-severity or higher-probability findings"
                    ),
                }
            )
        )
    return published, suppressed


def select(findings: list[Candidate], max_findings: int) -> list[Candidate]:
    """The published findings, ranked and capped (see :func:`select_with_receipts`)."""
    return select_with_receipts(findings, max_findings)[0]


# --------------------------------------------------------------------------- #
# Candidate -> public Finding
# --------------------------------------------------------------------------- #


def suggestion_block(fix: Fix) -> str:
    """Render the verified repair as an inline suggestion when possible.

    A single-hunk patch becomes a GitHub-style ```suggestion`` block containing
    the post-image lines, which is directly committable.  A multi-hunk patch
    cannot be expressed as one suggestion, so it is rendered as a ```diff``
    block that is still a verified, applyable artifact.
    """
    patch = (fix.patch or "").rstrip("\n")
    if not patch.strip():
        raise ValueError("an empty patch cannot be suggested")
    if "```" in patch:
        raise ValueError("a patch containing a code fence cannot be suggested inline")
    hunks = _patch_hunks(patch)
    files = [line for line in patch.splitlines() if line.startswith("+++ ")]
    if len(hunks) == 1 and len(files) <= 1:
        post = [line[1:] for line in hunks[0]["body"] if line[:1] in (" ", "+") and not line.startswith("+++")]
        if post and any(line.startswith("+") for line in hunks[0]["body"]):
            return "```suggestion\n" + "\n".join(post) + "\n```"
    return "```diff\n" + patch + "\n```"


_HUNK_RE = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")


def _patch_hunks(patch: str) -> list[dict[str, object]]:
    hunks: list[dict[str, object]] = []
    current: dict[str, object] | None = None
    for line in patch.splitlines():
        match = _HUNK_RE.match(line)
        if match:
            current = {
                "new_start": int(match.group(3)),
                "new_lines": int(match.group(4) or "1"),
                "body": [],
            }
            hunks.append(current)
            continue
        if current is not None and line[:1] in (" ", "+", "-", "\\"):
            current["body"].append(line)  # type: ignore[union-attr]
    return hunks


def suggestion_range(fix: Fix) -> tuple[int | None, int | None]:
    """The comment range a single-hunk suggestion must cover."""
    hunks = _patch_hunks(fix.patch or "")
    files = [line for line in (fix.patch or "").splitlines() if line.startswith("+++ ")]
    if len(hunks) != 1 or len(files) > 1:
        return None, None
    start = int(hunks[0]["new_start"])
    length = int(hunks[0]["new_lines"])
    if length <= 0:
        return None, None
    return start, start + length - 1


def _severity_label(severity: int) -> str:
    return SeverityLevel(min(3, max(0, int(severity)))).name


def _evidence_summary(candidate: Candidate, monitor: MonitorScore | None) -> str:
    evidence = candidate.evidence
    parts = [f"verification: {candidate.verification.kind.value}"]
    if evidence.rule_id:
        parts.append(f"rule {evidence.rule_id}")
    if evidence.analyzer:
        parts.append(f"analyzer {evidence.analyzer}")
    if monitor is not None:
        parts.append(
            f"monitor {monitor.score:.2f} (dispersion {monitor.dispersion:.2f}, votes {len(monitor.votes)})"
        )
    if evidence.detail:
        parts.append(evidence.detail[:200])
    return "; ".join(parts)


def to_finding(
    candidate: Candidate,
    elicitation: RiskElicitation | None,
    monitor: MonitorScore | None,
    p_combined: float,
    redaction: Redaction | None,
) -> Finding:
    """Convert one candidate into the public finding shape.

    A redacted finding keeps only the class label and the gate status: its
    cited lines, patch and root cause stay in the private record.
    """
    redacted = bool(redaction is not None and redaction.redacted)
    if elicitation is not None:
        failure_probability = float(elicitation.failure_probability)
    elif candidate.p is not None:
        failure_probability = float(candidate.p)
    else:
        failure_probability = 0.0

    if redacted:
        label = (redaction.cwes[0] if redaction.cwes else "") or candidate.cwe or "CWE-unclassified"
        return Finding(
            id=candidate.id,
            title=f"Security ({label}): security gate held",
            category=candidate.category,
            severity=candidate.severity,
            root_cause=public_body(redaction, candidate),
            cited_sites=[],
            cwe=candidate.cwe,
            evidence_summary="full diagnostic and verified patch recorded privately",
            verification=candidate.verification.kind,
            failure_probability=clamp01(failure_probability),
            p_combined=clamp01(p_combined),
            suggestion="",
            suggestion_patch="",
            redacted=True,
            private_detail_id=redaction.private_record_id,
        )

    fix = candidate.verified_fix
    suggestion = ""
    patch = ""
    start_line = None
    end_line = None
    if fix is not None and fix.verified:
        patch = fix.patch
        try:
            suggestion = suggestion_block(fix)
            start_line, end_line = suggestion_range(fix)
        except ValueError:
            suggestion = ""
    return Finding(
        id=candidate.id,
        title=candidate.title,
        category=candidate.category,
        severity=candidate.severity,
        root_cause=candidate.root_cause or candidate.title,
        cited_sites=list(candidate.cited_sites),
        cwe=candidate.cwe,
        evidence_summary=_evidence_summary(candidate, monitor),
        verification=candidate.verification.kind,
        failure_probability=clamp01(failure_probability),
        p_combined=clamp01(p_combined),
        suggestion=suggestion,
        suggestion_patch=patch,
        suggestion_start_line=start_line,
        suggestion_end_line=end_line,
        redacted=False,
        private_detail_id="",
    )


# --------------------------------------------------------------------------- #
# Rendering
# --------------------------------------------------------------------------- #


def _comment_body(finding: Finding) -> str:
    lines = [
        f"**[{_severity_label(finding.severity)}] {finding.title}**",
        "",
        f"Combined failure probability: {finding.p_combined:.4f} "
        f"(elicited {finding.failure_probability:.4f}).",
        "",
        finding.root_cause,
    ]
    if finding.evidence_summary:
        lines.extend(["", f"_{finding.evidence_summary}_"])
    if finding.suggestion:
        lines.extend(["", finding.suggestion])
    body = "\n".join(lines)
    if len(body) > MAX_COMMENT_CHARS:
        body = body[: MAX_COMMENT_CHARS - 40] + "\n\n...[comment truncated by the reviewer]..."
    return body


def _truncate(text: str, max_body: int) -> str:
    if len(text) <= max_body:
        return text
    marker = "\n\n...[review body truncated by the reviewer]..."
    keep = max(0, max_body - len(marker))
    return text[:keep] + marker


def _decision_basis(verdict: ReviewVerdict) -> str:
    """Why the review reached its decision, in words a PR author may read."""
    if verdict.decision is Decision.REQUEST_CHANGES:
        if verdict.p_combined < verdict.tau:
            return "a verified finding is published, so the review cannot approve on probability alone"
        return "the combined failure probability crossed the gate"
    if verdict.decision is Decision.HUMAN_REVIEW:
        return "the reviewer and the independent monitor disagreed, or a candidate could not be fused"
    return "no finding crossed the gate and the independent monitor agreed with the reviewer"


def _summary(
    verdict: ReviewVerdict,
    findings: list[Finding],
    bot_login: str,
) -> str:
    decision = verdict.decision.value
    lines = [
        f"## Automated review — decision: `{decision}`",
        "",
        f"Combined failure probability: {verdict.p_combined:.4f} "
        "(the gate threshold and the cost model stay host-side).",
        (
            f"Findings published: {len(findings)}; suppressed by receipt: "
            f"{verdict.suppressed_count}; redacted: {verdict.redacted_count}."
        ),
    ]
    if verdict.coverage_lines:
        lines.append(
            f"Coverage: {verdict.coverage_covered}/{verdict.coverage_lines} "
            "production changed lines assigned to a behavioural batch."
        )

    published = [finding for finding in findings if not finding.redacted]
    redacted = [finding for finding in findings if finding.redacted]
    if published:
        lines.extend(["", "### Published findings"])
        for finding in published:
            site = finding.cited_sites[0] if finding.cited_sites else None
            if site is not None:
                location = f" `{site.path}:{site.line}` —"
                detail = ""
            else:
                # nothing to anchor an inline comment to, so the body carries it
                location = ""
                detail = " " + finding.root_cause[:300]
            lines.append(
                f"- **[{_severity_label(finding.severity)}] {finding.title}** —{location} "
                f"p_combined {finding.p_combined:.4f}.{detail}"
            )
    if redacted:
        lines.extend(["", "### Security (details recorded privately)"])
        for finding in redacted:
            lines.append(f"- {finding.root_cause}")
    if not findings:
        lines.extend(["", "No finding met the publishability bar for this push."])

    lines.extend(
        [
            "",
            f"Decision basis: {_decision_basis(verdict)}.",
            "",
            "_An approval means no mechanically grounded finding was produced; it is not a proof of correctness._",
        ]
    )

    lines.extend(["", "### Degraded modes"])
    degraded = verdict.degraded
    if verdict.redacted_count:
        # A redacted review must not leak execution detail through the degraded
        # list: keep the category, drop the free-form tail that may quote code.
        degraded = [reason.split(":", 1)[0] for reason in degraded]
    if degraded:
        lines.extend(f"- {reason}" for reason in degraded[:20])
    else:
        lines.append("- none")

    lines.extend(
        [
            "",
            "This review is generated from code evidence only: no pull-request description, "
            "comment thread or embedded instruction was read.",
            f"Posted by `{bot_login}`.",
        ]
    )
    return "\n".join(lines)


def render_review(verdict: ReviewVerdict, *, bot_login: str, max_body: int = 60000) -> ReviewPayload:
    """Render exactly one review: one summary body plus at most three comments.

    Comments are rooted at the first cited line of each published finding and
    carry the verified suggestion.  Two findings on the same line share one
    comment.  Redacted findings never get an inline comment: their public text
    is the class label and the gate status only.
    """
    findings = list(verdict.findings)[:MAX_FINDINGS]
    seen: dict[tuple[str, int], int] = {}
    comments: list[ReviewComment] = []

    for finding in findings:
        if finding.redacted or not finding.cited_sites:
            continue
        site = finding.cited_sites[0]
        line = site.line
        start_line = None
        if (
            finding.suggestion_start_line is not None
            and finding.suggestion_end_line is not None
            and finding.suggestion_end_line > finding.suggestion_start_line
        ):
            start_line = finding.suggestion_start_line
            line = finding.suggestion_end_line
        key = (site.path, line)
        body = _comment_body(finding)
        if key in seen:
            index = seen[key]
            existing = comments[index]
            merged = f"{existing.body}\n\n---\n\n{body}"
            comments[index] = ReviewComment(
                path=existing.path,
                line=existing.line,
                body=merged,
                side=existing.side,
                start_line=existing.start_line,
                start_side=existing.start_side,
            )
            continue
        seen[key] = len(comments)
        comments.append(
            ReviewComment(
                path=site.path,
                line=line,
                body=body,
                start_line=start_line,
                start_side="RIGHT" if start_line is not None else None,
            )
        )

    body = _summary(verdict, findings, bot_login)
    return ReviewPayload(
        body=_truncate(body, max_body),
        comments=comments,
        event=EVENTS.get(verdict.decision, "COMMENT"),
    )
