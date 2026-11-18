"""The six-property runtime contract, including fault injection."""

from __future__ import annotations

import os
import threading
import time
from dataclasses import replace
from pathlib import Path

import pytest

from zero_loop_reviewer.config import load_settings
from zero_loop_reviewer.errors import ContractViolation
from zero_loop_reviewer.models import AttemptStatus, Receipt
from zero_loop_reviewer.stage1.contract import (
    Delivery,
    check_pool_disjointness,
    classify_cancellation,
    classify_outcome,
    deliver_file,
    enforce_contract,
    run_bounded,
    tree_digest,
    verify_payload_hash,
)
from zero_loop_reviewer.stage1.receipts import ReceiptLog, read_chain, verify_chain

PAYLOAD = b"x" * 4096


# --------------------------------------------------------------------------- #
# 4. Cryptographic input delivery
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("chunk", [41, 2048, len(PAYLOAD) - 1], ids=["one-percent", "half", "one-byte-short"])
def test_truncated_payloads_are_input_delivery_failures(tmp_path, chunk):
    delivery = deliver_file(tmp_path / "payload.bin", PAYLOAD, chunk=chunk)
    assert delivery.ok is False
    assert delivery.status is AttemptStatus.INPUT_DELIVERY_FAILED
    assert delivery.actual_bytes == chunk
    assert delivery.expected_bytes == len(PAYLOAD)
    assert delivery.actual_sha256 != delivery.expected_sha256
    with pytest.raises(ContractViolation):
        delivery.require()


def test_exact_delivery_is_accepted_and_hashed(tmp_path):
    delivery = deliver_file(tmp_path / "payload.bin", PAYLOAD)
    assert delivery.ok is True
    assert delivery.status is AttemptStatus.COMPLETED
    assert delivery.actual_bytes == len(PAYLOAD)
    assert delivery.actual_sha256 == delivery.expected_sha256


def test_verify_payload_hash_detects_one_flipped_byte(tmp_path):
    path = tmp_path / "payload.bin"
    path.write_bytes(PAYLOAD)
    good = verify_payload_hash(path, deliver_file(tmp_path / "copy", PAYLOAD).expected_sha256, len(PAYLOAD))
    assert good.ok is True
    path.write_bytes(PAYLOAD[:-1] + b"y")
    bad = verify_payload_hash(path, good.expected_sha256, len(PAYLOAD))
    assert bad.ok is False
    assert bad.status is AttemptStatus.INPUT_DELIVERY_FAILED


# --------------------------------------------------------------------------- #
# 2 + 5. Cancellation points and typed outcomes
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    ("write_done", "alive", "point"),
    [(False, True, "input_write"), (True, True, "output_read"), (True, False, "reap")],
)
def test_cancellation_points(write_done, alive, point):
    assert classify_cancellation(write_done=write_done, process_alive=alive) == point


def test_cancel_during_input_write_is_cancelled_never_completed(tmp_path):
    cancel = threading.Event()
    payload = b"x" * (2 * 1024 * 1024)
    threading.Timer(0.05, cancel.set).start()
    run = run_bounded(
        ["sh", "-c", "sleep 5"],
        cwd=tmp_path,
        timeout_s=10,
        input_bytes=payload,
        cancel_event=cancel,
    )
    assert run.status is AttemptStatus.CANCELLED
    assert run.cancellation_point == "input_write"
    assert run.status is not AttemptStatus.COMPLETED


def test_cancel_during_output_read_is_cancelled_never_completed(tmp_path):
    cancel = threading.Event()
    threading.Timer(0.15, cancel.set).start()
    run = run_bounded(
        ["sh", "-c", "cat >/dev/null; sleep 5"],
        cwd=tmp_path,
        timeout_s=10,
        input_bytes=b"hello",
        cancel_event=cancel,
    )
    assert run.status is AttemptStatus.CANCELLED
    assert run.cancellation_point == "output_read"


@pytest.mark.parametrize(
    ("exit_code", "stdout", "parsed", "expected"),
    [
        (0, '{"verdict": "safe"}', True, AttemptStatus.COMPLETED),
        (3, '{"verdict": "safe"}', True, AttemptStatus.PROCESS_CRASH),
        (0, "", False, AttemptStatus.EMPTY_RESPONSE),
        (0, "not json", False, AttemptStatus.MALFORMED_OUTPUT),
        (None, "", False, AttemptStatus.PROCESS_CRASH),
    ],
)
def test_typed_failure_semantics(exit_code, stdout, parsed, expected):
    assert (
        classify_outcome(exit_code=exit_code, stdout=stdout, stderr="", parsed_ok=parsed)
        is expected
    )


def test_nonzero_exit_with_valid_json_is_a_process_crash(tmp_path):
    run = run_bounded(
        ["sh", "-c", 'printf \'{"ok":true}\'; exit 3'],
        cwd=tmp_path,
        timeout_s=10,
        schema_check=lambda text: (True, "") if text.strip() else (False, "empty"),
    )
    assert run.exit_code == 3
    assert '"ok":true' in run.stdout
    assert run.status is AttemptStatus.PROCESS_CRASH


def test_zero_exit_with_malformed_output_is_malformed(tmp_path):
    run = run_bounded(
        ["sh", "-c", "printf 'not-json'"],
        cwd=tmp_path,
        timeout_s=10,
        schema_check=lambda text: (False, "not json"),
    )
    assert run.status is AttemptStatus.MALFORMED_OUTPUT


def test_timeout_is_typed(tmp_path):
    run = run_bounded(["sh", "-c", "sleep 5"], cwd=tmp_path, timeout_s=0.2)
    assert run.status is AttemptStatus.TIMEOUT
    assert run.timed_out is True


# --------------------------------------------------------------------------- #
# 3. Read-only enforcement
# --------------------------------------------------------------------------- #


def test_write_enabling_flags_are_refused_before_spawning(tmp_path):
    with pytest.raises(ContractViolation):
        run_bounded(["semgrep", "scan", "--fix"], cwd=tmp_path, timeout_s=5)
    with pytest.raises(ContractViolation):
        run_bounded(["ruff", "-w", "."], cwd=tmp_path, timeout_s=5)


def test_a_command_that_modifies_a_read_only_tree_is_refused(tmp_path):
    (tmp_path / "a.py").write_text("x = 1\n", encoding="utf-8")
    with pytest.raises(ContractViolation):
        run_bounded(
            ["sh", "-c", "echo y > a.py"],
            cwd=tmp_path,
            timeout_s=5,
            read_only_root=tmp_path,
        )


def test_a_read_only_command_leaves_the_digest_unchanged(tmp_path):
    (tmp_path / "a.py").write_text("x = 1\n", encoding="utf-8")
    before = tree_digest(tmp_path)
    run = run_bounded(["sh", "-c", "cat a.py >/dev/null"], cwd=tmp_path, timeout_s=5, read_only_root=tmp_path)
    assert run.status is AttemptStatus.COMPLETED
    assert tree_digest(tmp_path) == before


# --------------------------------------------------------------------------- #
# 1. Pool disjointness
# --------------------------------------------------------------------------- #


def _gate_settings(**kwargs):
    return load_settings({"gate.cross_provider_audit": True, **kwargs}, {})


def test_same_reviewer_and_authoring_key_is_refused():
    settings = _gate_settings(**{"gate.authoring_pool_env": "AUTHOR_KEY", "gate.reviewer_pool_env": "REVIEW_KEY"})
    check = check_pool_disjointness(settings, {"AUTHOR_KEY": "sk-same", "REVIEW_KEY": "sk-same"})
    assert check.status == "same_pool"
    assert check.refused is True
    with pytest.raises(ContractViolation):
        enforce_contract(settings, environ={"AUTHOR_KEY": "sk-same", "REVIEW_KEY": "sk-same"})


def test_different_keys_are_disjoint():
    settings = _gate_settings(**{"gate.authoring_pool_env": "AUTHOR_KEY", "gate.reviewer_pool_env": "REVIEW_KEY"})
    check = check_pool_disjointness(settings, {"AUTHOR_KEY": "sk-a", "REVIEW_KEY": "sk-b"})
    assert check.status == "disjoint"
    report = enforce_contract(settings, environ={"AUTHOR_KEY": "sk-a", "REVIEW_KEY": "sk-b"})
    assert report.degraded == []


def test_same_variable_name_is_refused_without_reading_the_value():
    settings = _gate_settings(**{"gate.authoring_pool_env": "SAME_KEY", "gate.reviewer_pool_env": "SAME_KEY"})
    check = check_pool_disjointness(settings, {"SAME_KEY": "sk-a"})
    assert check.status == "same_env"


def test_unconfigured_authoring_pool_is_recorded_not_silently_trusted():
    settings = _gate_settings()
    check = check_pool_disjointness(settings, {})
    assert check.status == "not_configured"
    report = enforce_contract(settings, environ={})
    assert any("pool_disjointness_unverified" in reason for reason in report.degraded)


def test_cross_provider_audit_can_be_disabled():
    settings = load_settings({"gate.cross_provider_audit": False}, {})
    assert check_pool_disjointness(settings, {}).status == "not_configured"


# --------------------------------------------------------------------------- #
# 6. Durable, tamper-evident receipts
# --------------------------------------------------------------------------- #


def test_receipts_are_immutable_and_chained(tmp_path):
    log = ReceiptLog(tmp_path, "review-1")
    first = log.record(
        stage="1.context",
        status=AttemptStatus.COMPLETED,
        started_at="2026-01-01T00:00:00.000Z",
        finished_at="2026-01-01T00:00:01.000Z",
        input_sha256="a" * 64,
        input_bytes=10,
    )
    second = log.record(
        stage="3.probe",
        status=AttemptStatus.CANCELLED,
        started_at="2026-01-01T00:00:01.000Z",
        finished_at="2026-01-01T00:00:02.000Z",
        cancellation_point="output_read",
    )
    assert first.prev_hash == ""
    assert second.prev_hash == first.hash
    chain = read_chain(log.chain_path)
    assert len(chain) == 2
    ok, why = verify_chain(chain)
    assert ok, why
    # The per-attempt file is created exclusively: a second write cannot happen.
    with pytest.raises(FileExistsError):
        os.open(log.dir / "receipt-0001.json", os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)


def test_receipt_tampering_breaks_the_chain(tmp_path):
    log = ReceiptLog(tmp_path, "review-2")
    log.record(
        stage="1.context",
        status=AttemptStatus.COMPLETED,
        started_at="a",
        finished_at="b",
    )
    log.record(stage="3.probe", status=AttemptStatus.COMPLETED, started_at="b", finished_at="c")
    chain = read_chain(log.chain_path)
    tampered = chain[0].model_copy(update={"detail": "edited after the fact"})
    ok, why = verify_chain([tampered, chain[1]])
    assert ok is False
    assert "hash does not match" in why
