"""Durable, chained per-attempt evidence.

One review owns one directory:

    <receipts>/<review_id>/receipt-NNNN.json   (immutable, mode 0600)
    <receipts>/<review_id>/receipts.jsonl      (append-only chain)
    <receipts>/<review_id>/verdict.json        (final payload)

Each receipt carries ``prev_hash`` and its own ``hash`` over the canonical JSON
of every other field, so deleting or editing an attempt breaks verification.
Writes use ``O_EXCL`` for the per-attempt file; an attempt id is never reused.
"""

from __future__ import annotations

import json
import os
import threading
import time
from pathlib import Path
from typing import Any, Iterable

from ..models import AttemptStatus, Receipt, canonical_json, sha256_hex


class ReceiptLog:
    def __init__(self, root: Path, review_id: str):
        self.root = Path(root)
        self.review_id = review_id
        self.dir = self.root / review_id
        self.dir.mkdir(parents=True, exist_ok=True)
        self.chain_path = self.dir / "receipts.jsonl"
        self._lock = threading.Lock()
        self._counter = 0
        self._prev = self._load_tail()

    def _load_tail(self) -> str:
        if not self.chain_path.exists():
            return ""
        last = ""
        for line in self.chain_path.read_text(encoding="utf-8").splitlines():
            if line.strip():
                last = line
        if not last:
            return ""
        try:
            return json.loads(last).get("hash", "")
        except json.JSONDecodeError:
            return ""

    def _next_id(self) -> str:
        self._counter += 1
        return f"receipt-{self._counter:04d}"

    def record(
        self,
        *,
        stage: str,
        status: AttemptStatus,
        started_at: str,
        finished_at: str,
        exit_code: int | None = None,
        input_sha256: str = "",
        input_bytes: int = 0,
        output_sha256: str = "",
        output_bytes: int = 0,
        model: str = "",
        tokens_in: int = 0,
        tokens_out: int = 0,
        cancellation_point: str = "",
        detail: str = "",
        attempt_id: str | None = None,
    ) -> Receipt:
        with self._lock:
            attempt = attempt_id or self._next_id()
            receipt = Receipt(
                attempt_id=attempt,
                review_id=self.review_id,
                stage=stage,
                status=status,
                started_at=started_at,
                finished_at=finished_at,
                exit_code=exit_code,
                input_sha256=input_sha256,
                input_bytes=input_bytes,
                output_sha256=output_sha256,
                output_bytes=output_bytes,
                model=model,
                tokens_in=tokens_in,
                tokens_out=tokens_out,
                cancellation_point=cancellation_point,
                detail=detail[:4000],
                prev_hash=self._prev,
                hash="",
            )
            digest = sha256_hex(canonical_json(receipt))
            receipt = receipt.model_copy(update={"hash": digest})
            body = canonical_json(receipt).encode("utf-8") + b"\n"
            path = self.dir / f"{attempt}.json"
            fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            try:
                os.write(fd, body)
                os.fsync(fd)
            finally:
                os.close(fd)
            with self.chain_path.open("ab") as chain:
                chain.write(body)
                chain.flush()
                os.fsync(chain.fileno())
            self._prev = digest
            return receipt

    def write_json(self, name: str, value: Any) -> Path:
        path = self.dir / name
        tmp = path.with_suffix(path.suffix + ".tmp")
        tmp.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.chmod(tmp, 0o600)
        tmp.replace(path)
        return path


def read_chain(path: Path) -> list[Receipt]:
    out: list[Receipt] = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        if line.strip():
            out.append(Receipt.model_validate_json(line))
    return out


def verify_chain(receipts: Iterable[Receipt]) -> tuple[bool, str]:
    """Recompute every hash and the link to its predecessor."""
    prev = ""
    for receipt in receipts:
        if receipt.prev_hash != prev:
            return False, f"{receipt.attempt_id}: prev_hash does not match the previous receipt"
        unsigned = receipt.model_copy(update={"hash": ""})
        expected = sha256_hex(canonical_json(unsigned))
        if receipt.hash != expected:
            return False, f"{receipt.attempt_id}: hash does not match its contents"
        prev = receipt.hash
    return True, "ok"


class Clock:
    """Wall clock + monotonic budget, injectable so tests never sleep."""

    def __init__(self) -> None:
        self._start = time.monotonic()

    def now_iso(self) -> str:
        return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()) + f".{int(time.time() * 1000) % 1000:03d}Z"

    def monotonic(self) -> float:
        return time.monotonic() - self._start
