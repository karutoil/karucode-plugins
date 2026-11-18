"""Stage 2B — security monologue, fresh-line reconciliation and candidates.

The model writes an evidence-constrained monologue; the host decides what it is
worth.  A VULNERABLE verdict only becomes *tier 1* when **every** cited line
exists in the analyzer trace for the same path within ±2 lines.  A
high-confidence contextual CWE with no analyzer rule at all is kept, but only as
``UNVERIFIED_TRUST``: it must still go through Stage 3/4 before it can reach
output.  A ``SAFE`` verdict yields no candidates and is still recorded.

Nothing here invents evidence: no analyzer line, no candidate.
"""

from __future__ import annotations

import re
from typing import Any, Sequence

from pydantic import ValidationError

from ..config import RedactionConfig, Settings
from ..errors import LLMError
from ..llm.prompts import security_prompt
from ..models import (
    AnalyzerFinding,
    Candidate,
    Category,
    CitedSite,
    ContextSlice,
    CpgFrame,
    DiffBundle,
    Evidence,
    FindingStatus,
    Reconciliation,
    ReconciliationTier,
    SecSemScan,
    StageId,
    Verification,
    VerificationKind,
    canonical_json,
    sha256_hex,
)

#: A contextual CWE with no analyzer rule needs at least this confidence.
HIGH_CONFIDENCE = 0.8
#: Analyzer lines this close to a cited line count as the same site.
LINE_TOLERANCE = 2

_EXPLOITABLE = frozenset(RedactionConfig().exploitable_cwes)

_CWE_SEVERITY = {
    "CWE-22": 3,
    "CWE-78": 3,
    "CWE-89": 3,
    "CWE-94": 3,
    "CWE-95": 3,
    "CWE-119": 3,
    "CWE-120": 3,
    "CWE-125": 3,
    "CWE-287": 3,
    "CWE-306": 3,
    "CWE-352": 3,
    "CWE-416": 3,
    "CWE-502": 3,
    "CWE-611": 3,
    "CWE-798": 3,
    "CWE-862": 3,
    "CWE-863": 3,
    "CWE-918": 3,
    "CWE-79": 2,
    "CWE-200": 2,
    "CWE-328": 2,
}

_CWE_LABEL = {
    "CWE-78": "OS command injection",
    "CWE-89": "SQL injection",
    "CWE-94": "code injection",
    "CWE-95": "code injection",
    "CWE-502": "unsafe deserialization",
    "CWE-798": "hardcoded credential",
    "CWE-328": "weak hash",
    "CWE-772": "resource leak",
    "CWE-22": "path traversal",
}


# --------------------------------------------------------------------------- #
# Reconciliation
# --------------------------------------------------------------------------- #


def reconcile(scan: SecSemScan, analyzer: list[AnalyzerFinding], diff: DiffBundle) -> Reconciliation:
    """Decide how much of the monologue the host can actually ground."""
    trace = [finding for finding in analyzer if finding.patch_caused and not finding.suppressed]
    cited = list(scan.cited_lines)
    matched: list[int] = []
    for site in cited:
        if any(
            finding.path == site.path and abs(finding.line - site.line) <= LINE_TOLERANCE for finding in trace
        ):
            matched.append(site.line)

    if scan.verdict == "SAFE":
        return Reconciliation(
            tier=ReconciliationTier.REJECTED,
            matched_lines=matched,
            detail=f"verdict SAFE: no vulnerability claimed ({len(matched)}/{len(cited)} cited lines in the trace)",
        )
    if not cited:
        return Reconciliation(
            tier=ReconciliationTier.REJECTED,
            matched_lines=[],
            detail="VULNERABLE without cited_lines: an uncited claim cannot be reconciled",
        )
    if len(matched) == len(cited):
        return Reconciliation(
            tier=ReconciliationTier.TIER1_ANALYZER,
            matched_lines=matched,
            detail=f"every cited line ({len(cited)}) matches the analyzer trace within {LINE_TOLERANCE} lines",
        )

    normalized = _normalize_cwe(scan.cwe)
    rule_for_cwe = any(_normalize_cwe(finding.cwe) == normalized for finding in trace) if normalized else False
    if not rule_for_cwe and scan.confidence >= HIGH_CONFIDENCE and normalized:
        outside = [site for site in cited if not _on_changed_line(site, diff)]
        if outside:
            return Reconciliation(
                tier=ReconciliationTier.REJECTED,
                matched_lines=matched,
                detail=(
                    "no analyzer rule fired and the monologue cites line(s) outside the patch: "
                    + ", ".join(f"{site.path}:{site.line}" for site in outside[:5])
                ),
            )
        return Reconciliation(
            tier=ReconciliationTier.UNVERIFIED_TRUST,
            matched_lines=matched,
            detail=(
                f"no analyzer rule for {normalized}; confidence {scan.confidence:.2f} >= {HIGH_CONFIDENCE} "
                "and every cited line is part of the patch — unverified, must reach Stage 3/4"
            ),
        )
    return Reconciliation(
        tier=ReconciliationTier.REJECTED,
        matched_lines=matched,
        detail=(
            f"only {len(matched)}/{len(cited)} cited lines match the analyzer trace"
            + ("" if rule_for_cwe else " and no analyzer rule for the claimed CWE")
        ),
    )


def _on_changed_line(site: CitedSite, diff: DiffBundle) -> bool:
    patch = diff.file(site.path)
    if patch is None:
        return False
    changed = patch.changed_lines
    if site.line in changed:
        return True
    if site.end_line is not None:
        return any(site.line <= line <= site.end_line for line in changed)
    return False


def _normalize_cwe(value: str) -> str:
    text = (value or "").strip().upper()
    if not text:
        return ""
    match = re.search(r"CWE-?(\d+)", text)
    if match:
        return f"CWE-{match.group(1)}"
    digits = "".join(ch for ch in text if ch.isdigit())
    return f"CWE-{digits}" if digits else ""


# --------------------------------------------------------------------------- #
# Candidates
# --------------------------------------------------------------------------- #


def to_candidates(
    scan: SecSemScan,
    reconciliation: Reconciliation,
    frames: Sequence[CpgFrame],
    diff: DiffBundle,
) -> list[Candidate]:
    """Build the candidate(s) a reconciliation tier permits.  SAFE/REJECTED -> []."""
    if scan is None or reconciliation is None:
        return []
    if scan.verdict != "VULNERABLE" or reconciliation.tier is ReconciliationTier.REJECTED:
        return []
    sites = [site for site in scan.cited_lines if _in_diff(site.path, diff)]
    if not sites:
        return []
    cwe = _normalize_cwe(scan.cwe)
    severity = _severity(cwe, scan.confidence)
    used_frames = [frame for frame in (frames or []) if any(site.path == frame.path for site in sites)]
    tier1 = reconciliation.tier is ReconciliationTier.TIER1_ANALYZER
    exploitable = cwe in _EXPLOITABLE
    identity = canonical_json(
        {
            "stage": StageId.SECSEM.value,
            "cwe": cwe,
            "sites": [[site.path, site.line] for site in sites],
            "monologue": scan.monologue[:400],
        }
    )
    candidate = Candidate(
        id=f"2b-{sha256_hex(identity)[:16]}",
        stage=StageId.SECSEM,
        category=Category.SECURITY,
        title=_title(cwe, sites[0]),
        root_cause=scan.monologue[:4000],
        severity=severity,
        cited_sites=sites,
        cwe=cwe,
        evidence=Evidence(
            kind="analyzer",
            analyzer="secsem",
            rule_id="",
            trace=sites,
            frames=used_frames[:12],
            detail=reconciliation.detail[:600],
        ),
        verification=Verification(
            kind=VerificationKind.ANALYZER_TRACE if tier1 else VerificationKind.UNVERIFIED,
            detail=reconciliation.detail[:600],
        ),
        p=round(float(scan.confidence), 6),
        status=FindingStatus.VERIFIED if tier1 else FindingStatus.UNVERIFIED,
        security_exploitable=exploitable,
        public_safe=not exploitable,
        private_detail=scan.monologue[:8000],
    )
    return [candidate]


def _in_diff(path: str, diff: DiffBundle) -> bool:
    return diff.file(path) is not None


def _title(cwe: str, site: CitedSite) -> str:
    label = _CWE_LABEL.get(cwe, "security defect")
    title = f"{cwe or 'Security'}: possible {label} at {site.path}:{site.line}"
    return title[:300]


def _severity(cwe: str, confidence: float) -> int:
    if cwe in _CWE_SEVERITY:
        return _CWE_SEVERITY[cwe]
    if confidence >= 0.85:
        return 3
    if confidence >= 0.6:
        return 2
    return 1


# --------------------------------------------------------------------------- #
# Orchestration for one track
# --------------------------------------------------------------------------- #


def run_security(
    diff: DiffBundle,
    context: ContextSlice,
    analyzer: list[AnalyzerFinding],
    frames: list[CpgFrame],
    llm: Any,
    settings: Settings,
    *,
    policy: dict | None,
) -> tuple[SecSemScan | None, Reconciliation | None, list[Candidate], str]:
    """One security monologue.  Any failure returns no scan and a reason."""
    rules = [rule for rule in (policy or {}).get("rules", []) if isinstance(rule, dict)]
    trace = [finding for finding in analyzer if finding.patch_caused and not finding.suppressed]
    prompt = security_prompt(diff, context, analyzer, frames, rules=rules, tier1=bool(trace))
    stage_model = settings.llm.security
    try:
        answer = llm.complete(
            "security",
            prompt,
            SecSemScan,
            stage_model=stage_model,
            temperature=stage_model.temperature,
        )
    except LLMError as exc:
        return None, None, [], f"security scan failed: {exc.kind.value}: {exc.message}"
    except ValidationError as exc:
        return None, None, [], f"security scan answer is not a SecSemScan: {exc}"
    except Exception as exc:  # a transport double may raise anything; never lose the reason
        return None, None, [], f"security scan failed: {type(exc).__name__}: {exc}"
    value = getattr(answer, "value", None)
    try:
        scan = value if isinstance(value, SecSemScan) else SecSemScan.model_validate(value)
    except (ValidationError, TypeError, ValueError) as exc:
        return None, None, [], f"security scan answer is not a SecSemScan: {exc}"
    reconciliation = reconcile(scan, analyzer, diff)
    candidates = to_candidates(scan, reconciliation, frames, diff)
    # Carry the analyzer rule that actually matched a cited line onto the
    # candidate.  Without it, CIVER has no rule to rerun and a redacted finding
    # reaches the private log without its verified patch.
    trace = [finding for finding in analyzer if finding.patch_caused and not finding.suppressed]
    enriched: list[Candidate] = []
    for candidate in candidates:
        matched = [
            finding
            for finding in trace
            if any(site.path == finding.path and abs(site.line - finding.line) <= 2 for site in candidate.cited_sites)
        ]
        if not matched:
            enriched.append(candidate)
            continue
        finding = matched[0]
        evidence = candidate.evidence.model_copy(
            update={"rule_id": finding.rule_id, "analyzer": finding.analyzer or candidate.evidence.analyzer}
        )
        enriched.append(
            candidate.model_copy(
                update={
                    "evidence": evidence,
                    "cwe": candidate.cwe or finding.cwe,
                }
            )
        )
    candidates = enriched
    detail = (
        f"verdict={scan.verdict} tier={reconciliation.tier.value} "
        f"candidates={len(candidates)}"
    )
    if scan.verdict == "VULNERABLE":
        detail += f" note={reconciliation.detail[:200]}"
    return scan, reconciliation, candidates, detail
