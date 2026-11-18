"""Stage 5 — tiered security redaction.

An *exploitable* finding is a security finding (or a CWE on the operator's
list) that can cite a real untrusted-input path.  Those findings are published
as a class plus a gate status and nothing else: no exploit mechanics, no
line-by-line bypass map, no patch.  The full diagnostic and the verified patch
are written to a private, 0600 JSON record whose id goes back into the public
finding so a maintainer can retrieve it.

Redaction is a publishability decision, not a finding decision: a redacted
finding is still a finding, and it is never dropped.
"""

from __future__ import annotations

import os
from pathlib import Path

from ..config import RedactionConfig
from ..models import Candidate, Category, Fix, Redaction, canonical_json, sha256_hex

#: Written into every private record so a reader knows what it is looking at.
RECORD_SCHEMA_VERSION = 1

#: The private directory and every record in it are owner-only.
PRIVATE_DIR_MODE = 0o700
PRIVATE_FILE_MODE = 0o600


# --------------------------------------------------------------------------- #
# Classification
# --------------------------------------------------------------------------- #


def exploitable_cwes(config: RedactionConfig) -> frozenset[str]:
    """The configured CWE list, normalised for comparison."""
    return frozenset(cwe.strip().upper() for cwe in config.exploitable_cwes if cwe.strip())


def _cwe(candidate: Candidate) -> str:
    return (candidate.cwe or "").strip().upper()


def has_taint_evidence(candidate: Candidate) -> bool:
    """True when the candidate cites an untrusted-input path.

    Two shapes count, and both must be *cited*:

    * a CPG frame that carries at least one taint tuple (its source and sink are
      ``CitedSite`` values, so the path is already located in the diff);
    * an analyzer/CPG trace whose rule or detail names taint, untrusted input or
      injection **and** the candidate carries at least one cited site.
    """
    evidence = candidate.evidence
    if any(frame.taint for frame in evidence.frames):
        return True
    words = " ".join((evidence.rule_id, evidence.detail, evidence.analyzer)).lower()
    named = any(token in words for token in ("taint", "untrusted", "injection", "sanitiz", "escape"))
    return bool(named and candidate.cited_sites)


def is_exploitable(candidate: Candidate, config: RedactionConfig) -> bool:
    """Security class (or listed CWE) **and** a cited untrusted-input path."""
    if not config.enabled:
        return False
    security_class = candidate.category is Category.SECURITY or _cwe(candidate) in exploitable_cwes(config)
    if not security_class:
        return False
    return has_taint_evidence(candidate)


# --------------------------------------------------------------------------- #
# Private record
# --------------------------------------------------------------------------- #


def _class_label(candidate: Candidate) -> str:
    cwe = _cwe(candidate)
    return cwe or "CWE-unclassified"


def _public_text(candidate: Candidate) -> str:
    """The only sentence a redacted finding may say in public."""
    return f"Security: {_class_label(candidate)} — security gate held."


def _private_payload(candidate: Candidate, config: RedactionConfig) -> dict:
    fix: Fix | None = candidate.verified_fix
    return {
        "schema_version": RECORD_SCHEMA_VERSION,
        "kind": "security_redaction",
        "class": _class_label(candidate),
        "category": candidate.category.value,
        "value": candidate.cwe,
        "exploitable_cwes": sorted(exploitable_cwes(config)),
        "reason": _redaction_reason(candidate),
        "public_text": _public_text(candidate),
        "candidate": candidate.model_dump(mode="json"),
        "verified_patch": fix.patch if fix is not None else "",
        "fix_verified": bool(fix is not None and fix.verified),
    }


def _redaction_reason(candidate: Candidate) -> str:
    class_note = (
        "category is security"
        if candidate.category is Category.SECURITY
        else f"cwe {_cwe(candidate)} is on the exploitable list"
    )
    if any(frame.taint for frame in candidate.evidence.frames):
        evidence_note = "a CPG frame cites a source-to-sink taint path"
    else:
        evidence_note = "the analyzer trace names untrusted input and the finding cites its sites"
    return f"{class_note}; {evidence_note}"


def record_id_for(candidate: Candidate, config: RedactionConfig) -> str:
    """Deterministic id of the private record (content-addressed, no clock)."""
    return sha256_hex(canonical_json(_private_payload(candidate, config)))[:16]


def _record_path(private_dir: Path, config: RedactionConfig, record_id: str) -> Path:
    stem = Path(config.private_log_name).stem or "security-private"
    return Path(private_dir) / f"{stem}-{record_id}.json"


def _write_private(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=PRIVATE_DIR_MODE)
    try:
        os.chmod(path.parent, PRIVATE_DIR_MODE)
    except OSError:  # a directory we do not own: still write, never crash
        pass
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, PRIVATE_FILE_MODE)
    try:
        os.fchmod(descriptor, PRIVATE_FILE_MODE)
        os.write(descriptor, (canonical_json(payload) + "\n").encode("utf-8"))
    finally:
        os.close(descriptor)


# --------------------------------------------------------------------------- #
# Entry points
# --------------------------------------------------------------------------- #


def redact(candidate: Candidate, config: RedactionConfig, *, private_dir: Path) -> Redaction:
    """Classify one candidate and, when exploitable, write its private record.

    A non-exploitable candidate yields ``redacted=False`` and writes nothing;
    an exploitable one yields the public class label plus the id of a 0600
    record under ``private_dir`` holding the diagnostic and the verified patch.
    """
    if not is_exploitable(candidate, config):
        return Redaction(
            redacted=False,
            reason="not an exploitable untrusted-input finding; nothing to redact",
        )

    payload = _private_payload(candidate, config)
    record_id = record_id_for(candidate, config)
    payload["record_id"] = record_id
    _write_private(_record_path(private_dir, config, record_id), payload)

    return Redaction(
        redacted=True,
        reason=_redaction_reason(candidate),
        public_text=_public_text(candidate),
        private_record_id=record_id,
        cwes=[_class_label(candidate)],
    )


def public_body(redaction: Redaction, candidate: Candidate) -> str:
    """The public text for one finding.

    For a redacted finding this is the *entire* public record: a class name and
    the gate status.  For any other finding it is a one-line identifier; the
    real body belongs to the formatter, which may safely quote the code.
    """
    if redaction.redacted:
        return redaction.public_text or _public_text(candidate)
    return f"{candidate.title} ({candidate.category.value}, severity {candidate.severity})"
