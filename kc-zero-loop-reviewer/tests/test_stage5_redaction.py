"""Stage 5C: exploitable security findings are published as a class and nothing else."""

from __future__ import annotations

import json
import stat

from zero_loop_reviewer.config import RedactionConfig
from zero_loop_reviewer.models import (
    Candidate,
    Category,
    CitedSite,
    CpgFrame,
    Evidence,
    Fix,
    StageId,
    TaintTuple,
    Verification,
    VerificationKind,
)
from zero_loop_reviewer.stage5.redaction import (
    has_taint_evidence,
    is_exploitable,
    public_body,
    record_id_for,
    redact,
)

PATCH = """--- a/src/shell.py
+++ b/src/shell.py
@@ -1,3 +1,3 @@
-    os.system("tar -xf " + user_path)
+    subprocess.run(["tar", "-xf", user_path], check=True)
"""

SOURCE = CitedSite(path="src/handler.py", line=11, symbol="user_path", snippet='request.args["t"]')
SINK = CitedSite(path="src/shell.py", line=7, symbol="os.system", snippet='os.system("tar -xf " + user_path)')


def _taint_frame() -> CpgFrame:
    return CpgFrame(path="src/shell.py", function="unpack", taint=[TaintTuple(source=SOURCE, sink=SINK)])


def _candidate(**overrides) -> Candidate:
    payload = {
        "id": "sec-1",
        "stage": StageId.SECSEM,
        "category": Category.SECURITY,
        "title": "shell command built from an untrusted query parameter",
        "root_cause": "user_path reaches os.system, so ../ escapes the extraction directory",
        "severity": 3,
        "cited_sites": [SOURCE, SINK],
        "cwe": "CWE-78",
        "evidence": Evidence(kind="cpg", frames=[_taint_frame()], trace=[SOURCE, SINK]),
        "verification": Verification(kind=VerificationKind.CPG_COUNTERFACTUAL),
        "verified_fix": Fix(patch=PATCH, files_changed=["src/shell.py"], verified=True, compile_ok=True),
        "status": "verified",
    }
    payload.update(overrides)
    return Candidate(**payload)


# --------------------------------------------------------------------------- #
# Classification
# --------------------------------------------------------------------------- #


def test_security_category_with_a_taint_path_is_exploitable() -> None:
    assert has_taint_evidence(_candidate()) is True
    assert is_exploitable(_candidate(), RedactionConfig()) is True


def test_a_listed_cwe_is_enough_without_the_security_category() -> None:
    candidate = _candidate(category=Category.CORRECTNESS, cwe="CWE-89")
    assert is_exploitable(candidate, RedactionConfig()) is True


def test_a_security_finding_without_taint_evidence_is_not_redacted() -> None:
    candidate = _candidate(evidence=Evidence(kind="analyzer", analyzer="semgrep", rule_id="hardcoded-secret"))
    assert has_taint_evidence(candidate) is False
    assert is_exploitable(candidate, RedactionConfig()) is False


def test_an_analyzer_rule_naming_untrusted_input_counts_when_it_is_cited() -> None:
    cited = _candidate(
        evidence=Evidence(
            kind="analyzer", analyzer="semgrep", rule_id="python.lang.security.taint-mode.untrusted-input"
        )
    )
    assert is_exploitable(cited, RedactionConfig()) is True
    uncited = cited.model_copy(update={"cited_sites": []})
    assert has_taint_evidence(uncited) is False
    assert is_exploitable(uncited, RedactionConfig()) is False


def test_a_non_security_cwe_is_not_redacted() -> None:
    candidate = _candidate(category=Category.LOGIC, cwe="CWE-483")
    assert is_exploitable(candidate, RedactionConfig()) is False


def test_redaction_can_be_disabled() -> None:
    assert is_exploitable(_candidate(), RedactionConfig(enabled=False)) is False


# --------------------------------------------------------------------------- #
# The private record
# --------------------------------------------------------------------------- #


def test_redact_writes_a_owner_only_private_record(tmp_path) -> None:
    candidate = _candidate()
    private_dir = tmp_path / "private"
    redaction = redact(candidate, RedactionConfig(), private_dir=private_dir)

    assert redaction.redacted is True
    assert redaction.private_record_id
    assert redaction.cwes == ["CWE-78"]
    records = list(private_dir.iterdir())
    assert len(records) == 1
    record = records[0]
    assert redaction.private_record_id in record.name
    assert stat.S_IMODE(record.stat().st_mode) == 0o600
    assert stat.S_IMODE(private_dir.stat().st_mode) == 0o700

    payload = json.loads(record.read_text(encoding="utf-8"))
    assert payload["record_id"] == redaction.private_record_id
    assert payload["verified_patch"] == PATCH
    assert payload["candidate"]["id"] == candidate.id
    assert payload["class"] == "CWE-78"
    assert payload["reason"]


def test_public_text_names_only_the_class_and_the_gate(tmp_path) -> None:
    candidate = _candidate()
    redaction = redact(candidate, RedactionConfig(), private_dir=tmp_path / "private")

    text = redaction.public_text
    assert "CWE-78" in text
    assert "security gate held" in text
    # neither the patch nor the exploit mechanics may appear in public text
    assert PATCH not in text
    for secret in ("os.system", "subprocess", "user_path", "tar -xf", "../", "src/shell.py", "src/handler.py"):
        assert secret not in text, secret
    assert text.count("\n") == 0


def test_public_body_matches_the_redacted_public_text(tmp_path) -> None:
    candidate = _candidate()
    redaction = redact(candidate, RedactionConfig(), private_dir=tmp_path / "private")
    assert public_body(redaction, candidate) == redaction.public_text


def test_public_body_of_a_plain_finding_names_the_finding(tmp_path) -> None:
    candidate = _candidate(category=Category.LOGIC, cwe="", evidence=Evidence(kind="analyzer"))
    redaction = redact(candidate, RedactionConfig(), private_dir=tmp_path / "private")
    assert redaction.redacted is False
    assert "shell command" in public_body(redaction, candidate)


def test_redaction_is_deterministic(tmp_path) -> None:
    candidate = _candidate()
    first = redact(candidate, RedactionConfig(), private_dir=tmp_path / "a")
    second = redact(candidate, RedactionConfig(), private_dir=tmp_path / "b")
    assert first.private_record_id == second.private_record_id
    assert first.private_record_id == record_id_for(candidate, RedactionConfig())
    assert (tmp_path / "a" / f"security-private-{first.private_record_id}.json").read_bytes() == (
        tmp_path / "b" / f"security-private-{second.private_record_id}.json"
    ).read_bytes()


def test_a_non_exploitable_finding_writes_nothing(tmp_path) -> None:
    candidate = _candidate(category=Category.LOGIC, cwe="CWE-483")
    private_dir = tmp_path / "private"
    redaction = redact(candidate, RedactionConfig(), private_dir=private_dir)
    assert redaction.redacted is False
    assert redaction.private_record_id == ""
    assert redaction.public_text == ""
    assert not private_dir.exists()


def test_redaction_config_selects_the_record_name(tmp_path) -> None:
    config = RedactionConfig(private_log_name="sec.jsonl")
    redaction = redact(_candidate(), config, private_dir=tmp_path)
    assert (tmp_path / f"sec-{redaction.private_record_id}.json").exists()
