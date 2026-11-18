"""Stage 1A — the six-property runtime contract.

1. Resource-pool disjointness (reviewer vs. authoring key).
2. Bounded lifetime and reap tracking, with cancellation points.
3. Read-only capability enforcement.
4. Cryptographic input delivery (byte length + SHA-256).
5. Typed failure semantics (exit code *and* schema validity).
6. Durable per-attempt evidence (never fail silently).
"""

from __future__ import annotations

import hashlib
import os
import signal
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Mapping, Sequence

from ..config import Settings
from ..errors import ContractViolation
from ..models import AttemptStatus, sha256_hex

# Flags that would let an analyzer rewrite the tree.  A reviewer is never
# allowed to pass them, even when a project config asks for it.
WRITE_FLAGS = (
    "--fix",
    "--fix-only",
    "--write",
    "--in-place",
    "--apply",
    "--autofix",
    "--update",
    "--output",
    "-w",
    "--save",
)


class DeliveryFailure(ContractViolation):
    kind = ContractViolation.kind  # type: ignore[assignment]


# --------------------------------------------------------------------------- #
# 1. Pool disjointness
# --------------------------------------------------------------------------- #


@dataclass(frozen=True)
class PoolCheck:
    status: str  # disjoint | same_pool | not_configured | same_env
    detail: str = ""

    @property
    def refused(self) -> bool:
        return self.status in ("same_pool", "same_env")


def check_pool_disjointness(
    settings: Settings, environ: Mapping[str, str] | None = None
) -> PoolCheck:
    """Refuse the launch when the reviewer reuses the authoring key.

    Only *configured* pools can be compared.  An unconfigured authoring pool is
    recorded as ``not_configured`` and surfaced as a degraded reason: refusing
    every review on an install that has not set the variable would make the
    reviewer useless, while silently claiming disjointness would be a lie.
    """
    env = environ if environ is not None else os.environ
    authoring = settings.gate.authoring_pool_env
    reviewer = settings.gate.reviewer_pool_env
    if not settings.gate.cross_provider_audit:
        return PoolCheck("not_configured", "cross-provider auditing disabled by config")
    if not authoring:
        return PoolCheck("not_configured", "no authoring pool environment variable is configured")
    if not reviewer:
        return PoolCheck("not_configured", "no reviewer pool environment variable is configured")
    if authoring == reviewer:
        return PoolCheck("same_env", f"reviewer and authoring pools both read ${authoring}")
    a = settings.pool_fingerprint(authoring, env)
    r = settings.pool_fingerprint(reviewer, env)
    if not a or not r:
        return PoolCheck("not_configured", "one of the pool variables is unset")
    if a == r:
        return PoolCheck("same_pool", f"${reviewer} and ${authoring} hold the same credential")
    return PoolCheck("disjoint", f"${reviewer} differs from ${authoring}")


# --------------------------------------------------------------------------- #
# 3. Read-only enforcement
# --------------------------------------------------------------------------- #


def check_read_only_argv(argv: Sequence[str]) -> None:
    for token in argv:
        low = token.lower()
        for flag in WRITE_FLAGS:
            if low == flag or low.startswith(flag + "="):
                raise ContractViolation(
                    f"analyzer command contains a write-enabling flag {token!r}; "
                    "the reviewer workspace is read-only"
                )


def tree_digest(
    root: Path,
    limit: int = 20_000,
    *,
    paths: Sequence[str | Path] | None = None,
) -> str:
    """Digest of path names plus content hashes, ignoring VCS metadata.

    Used before and after a read-only command: any change is a contract
    violation, whether or not the flag check caught it.  ``paths`` narrows the
    digest to the files a command was actually allowed to touch; hashing a live
    monorepo's whole tree races with every other process and turns an unrelated
    docs build into a false read-only violation.
    """
    root = Path(root)
    digest = hashlib.sha256()
    count = 0
    if paths is None:
        entries = sorted(root.rglob("*"))
    else:
        entries = [root / path for path in paths]
    for path in entries:
        try:
            rel = path.relative_to(root)
        except ValueError:
            continue
        if ".git" in rel.parts or "__pycache__" in rel.parts:
            continue
        if path.is_symlink():
            digest.update(b"L" + str(rel).encode() + b"\0")
            continue
        if not path.is_file():
            continue
        count += 1
        if count > limit:
            break
        digest.update(b"F" + str(rel).encode() + b"\0")
        try:
            with path.open("rb") as fh:
                for chunk in iter(lambda: fh.read(1 << 16), b""):
                    digest.update(chunk)
        except OSError:
            digest.update(b"<unreadable>")
    return digest.hexdigest()


# --------------------------------------------------------------------------- #
# 4. Cryptographic input delivery
# --------------------------------------------------------------------------- #


@dataclass(frozen=True)
class Delivery:
    ok: bool
    expected_sha256: str
    actual_sha256: str
    expected_bytes: int
    actual_bytes: int
    detail: str = ""
    status: AttemptStatus = AttemptStatus.COMPLETED

    def require(self) -> "Delivery":
        if not self.ok:
            raise ContractViolation(
                f"input delivery failed: {self.detail}",
            )
        return self


def deliver_file(path: Path, payload: bytes, *, chunk: int | None = None) -> Delivery:
    """Write ``payload`` and verify the bytes that actually landed.

    ``chunk`` exists for fault injection: writing 1% or all-but-one byte is
    exactly the failure the contract must catch.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    expected = sha256_hex(payload)
    limit = len(payload) if chunk is None else max(0, min(chunk, len(payload)))
    written = 0
    with path.open("wb") as fh:
        while written < limit:
            step = min(4096, limit - written)
            fh.write(payload[written : written + step])
            written += step
    actual = path.read_bytes()
    actual_sha = sha256_hex(actual)
    ok = len(actual) == len(payload) and actual_sha == expected
    detail = "" if ok else f"wrote {len(actual)} of {len(payload)} bytes (sha {actual_sha[:12]})"
    return Delivery(
        ok=ok,
        expected_sha256=expected,
        actual_sha256=actual_sha,
        expected_bytes=len(payload),
        actual_bytes=len(actual),
        detail=detail,
        status=AttemptStatus.COMPLETED if ok else AttemptStatus.INPUT_DELIVERY_FAILED,
    )


# --------------------------------------------------------------------------- #
# 2 + 5. Bounded subprocess with typed outcomes
# --------------------------------------------------------------------------- #


@dataclass
class BoundedRun:
    argv: list[str]
    exit_code: int | None
    stdout: str
    stderr: str
    status: AttemptStatus
    cancellation_point: str = ""
    duration_ms: int = 0
    input_sha256: str = ""
    input_bytes: int = 0
    timed_out: bool = False

    @property
    def completed(self) -> bool:
        return self.status is AttemptStatus.COMPLETED

    @property
    def signal(self) -> int | None:
        return -self.exit_code if self.exit_code is not None and self.exit_code < 0 else None


def classify_cancellation(*, write_done: bool, process_alive: bool) -> str:
    """Where a cancelled run was interrupted, for the receipt."""
    if not write_done:
        return "input_write"
    if process_alive:
        return "output_read"
    return "reap"


def classify_outcome(
    *,
    exit_code: int | None,
    stdout: str,
    stderr: str,
    parsed_ok: bool,
    cancelled: bool = False,
    cancellation_point: str = "",
    timed_out: bool = False,
) -> AttemptStatus:
    """Typed failure semantics.

    A nonzero exit is a crash even if the process printed a valid JSON verdict:
    the process's own exit status and its payload must both say "fine".
    """
    if cancelled:
        return AttemptStatus.CANCELLED
    if timed_out:
        return AttemptStatus.TIMEOUT
    if exit_code is None:
        return AttemptStatus.PROCESS_CRASH
    if exit_code != 0:
        return AttemptStatus.PROCESS_CRASH
    if not stdout.strip() and not stderr.strip():
        return AttemptStatus.EMPTY_RESPONSE
    if not stdout.strip():
        return AttemptStatus.EMPTY_RESPONSE
    if not parsed_ok:
        return AttemptStatus.MALFORMED_OUTPUT
    return AttemptStatus.COMPLETED


def _reader(stream, sink: list[bytes]) -> None:
    try:
        while True:
            chunk = stream.read(65536)
            if not chunk:
                break
            sink.append(chunk)
    except (ValueError, OSError):
        pass
    finally:
        try:
            stream.close()
        except OSError:
            pass


def run_bounded(
    argv: Sequence[str],
    *,
    cwd: Path,
    timeout_s: float,
    input_bytes: bytes = b"",
    env: Mapping[str, str] | None = None,
    cancel_event: threading.Event | None = None,
    read_only_root: Path | None = None,
    read_only_paths: Sequence[str | Path] | None = None,
    guard_writes: bool = True,
    schema_check=None,
) -> BoundedRun:
    """Run one command with a hard deadline, cancellation points and receipts.

    ``schema_check`` is called with stdout after a clean exit; it returns
    ``(parsed_ok, detail)``.  The returned status is authoritative: a cancelled
    run is never ``completed``.
    """
    started = time.monotonic()
    if guard_writes:
        check_read_only_argv(argv)
    before = (
        tree_digest(read_only_root, paths=read_only_paths)
        if read_only_root is not None
        else ""
    )
    # Only send the variables a tool needs; never inherit the reviewer key.
    child_env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": os.environ.get("HOME", "/tmp")}
    if env:
        child_env.update({k: v for k, v in env.items() if "KEY" not in k.upper() and "TOKEN" not in k.upper()})
    proc = subprocess.Popen(
        list(argv),
        cwd=str(cwd),
        env=child_env,
        stdin=subprocess.PIPE if input_bytes else subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    out_chunks: list[bytes] = []
    err_chunks: list[bytes] = []
    out_thread = threading.Thread(target=_reader, args=(proc.stdout, out_chunks), daemon=True)
    err_thread = threading.Thread(target=_reader, args=(proc.stderr, err_chunks), daemon=True)
    out_thread.start()
    err_thread.start()

    write_done = threading.Event()
    write_error: list[str] = []

    def _write() -> None:
        try:
            assert proc.stdin is not None
            view = memoryview(input_bytes)
            off = 0
            while off < len(input_bytes):
                if cancel_event is not None and cancel_event.is_set():
                    write_error.append("cancelled during input write")
                    return
                step = min(65536, len(input_bytes) - off)
                proc.stdin.write(view[off : off + step])
                proc.stdin.flush()
                off += step
            write_done.set()
            proc.stdin.close()
        except (BrokenPipeError, OSError, ValueError) as exc:
            write_error.append(str(exc))
        finally:
            write_done.set()

    writer = threading.Thread(target=_write, daemon=True) if input_bytes else None
    if writer is not None:
        writer.start()
    else:
        write_done.set()

    deadline = started + timeout_s
    cancelled = False
    cancellation_point = ""
    killed = False
    while True:
        rc = proc.poll()
        if rc is not None:
            break
        if cancel_event is not None and cancel_event.is_set():
            cancelled = True
            alive = proc.poll() is None
            cancellation_point = classify_cancellation(
                write_done=write_done.is_set(), process_alive=alive
            )
            if alive:
                _kill_group(proc)
                killed = True
            break
        if time.monotonic() >= deadline:
            _kill_group(proc)
            killed = True
            break
        time.sleep(0.01)

    try:
        exit_code = proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        _kill_group(proc)
        exit_code = proc.wait(timeout=5)
    if cancel_event is not None and cancel_event.is_set() and not cancelled:
        # Cancellation arrived while the process was exiting: still cancelled,
        # at the reap point, never completed.
        cancelled = True
        cancellation_point = "reap"
    out_thread.join(timeout=5)
    err_thread.join(timeout=5)
    if writer is not None:
        writer.join(timeout=5)

    stdout = b"".join(out_chunks).decode("utf-8", "replace")
    stderr = b"".join(err_chunks).decode("utf-8", "replace")
    if write_error and not stdout.strip() and not cancelled:
        # The payload never fully reached the child; that is a delivery
        # failure, not an empty response.
        return BoundedRun(
            argv=list(argv),
            exit_code=exit_code,
            stdout=stdout,
            stderr=stderr + "\n" + "; ".join(write_error),
            status=AttemptStatus.INPUT_DELIVERY_FAILED,
            cancellation_point=cancellation_point,
            duration_ms=int((time.monotonic() - started) * 1000),
            input_sha256=sha256_hex(input_bytes),
            input_bytes=len(input_bytes),
        )
    timed_out = (not cancelled) and (killed or time.monotonic() >= deadline) and exit_code != 0
    parsed_ok = False
    detail = ""
    if schema_check is not None and exit_code == 0 and stdout.strip():
        try:
            parsed_ok, detail = schema_check(stdout)
        except Exception as exc:  # schema checker must never crash the runner
            parsed_ok, detail = False, f"schema check raised: {exc}"
    if schema_check is None:
        # A command without a verdict contract (git, compileall, semgrep JSON is
        # checked by its own caller) is judged by its exit status alone; empty
        # stdout is normal for a successful command.
        if cancelled:
            status = AttemptStatus.CANCELLED
        elif timed_out:
            status = AttemptStatus.TIMEOUT
        elif exit_code != 0:
            status = AttemptStatus.PROCESS_CRASH
        else:
            status = AttemptStatus.COMPLETED
    else:
        status = classify_outcome(
            exit_code=exit_code,
            stdout=stdout,
            stderr=stderr,
            parsed_ok=parsed_ok,
            cancelled=cancelled,
            cancellation_point=cancellation_point,
            timed_out=timed_out,
        )
    if status is AttemptStatus.MALFORMED_OUTPUT and detail:
        stderr = f"{stderr}\n{detail}".strip()
    if read_only_root is not None:
        after = tree_digest(read_only_root, paths=read_only_paths)
        if before and after != before:
            raise ContractViolation(
                f"read-only workspace was modified by {argv[0]!r} ({before[:12]} -> {after[:12]})"
            )
    return BoundedRun(
        argv=list(argv),
        exit_code=exit_code,
        stdout=stdout,
        stderr=stderr,
        status=status,
        cancellation_point=cancellation_point,
        duration_ms=int((time.monotonic() - started) * 1000),
        input_sha256=sha256_hex(input_bytes),
        input_bytes=len(input_bytes),
        timed_out=timed_out,
    )


def _kill_group(proc: subprocess.Popen) -> None:
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        try:
            proc.kill()
        except ProcessLookupError:
            pass


@dataclass
class ContractReport:
    pool: PoolCheck
    degraded: list[str] = field(default_factory=list)
    read_only: bool = True

    @property
    def refused(self) -> bool:
        return self.pool.refused


def enforce_contract(
    settings: Settings,
    *,
    environ: Mapping[str, str] | None = None,
    read_only: bool = True,
) -> ContractReport:
    check = check_pool_disjointness(settings, environ)
    if check.refused:
        raise ContractViolation(f"pool disjointness refused: {check.detail}")
    degraded: list[str] = []
    if check.status == "not_configured":
        degraded.append(f"pool_disjointness_unverified: {check.detail}")
    return ContractReport(pool=check, degraded=degraded, read_only=read_only)


def verify_payload_hash(path: Path, expected_sha: str, expected_bytes: int | None = None) -> Delivery:
    data = Path(path).read_bytes()
    actual = sha256_hex(data)
    ok = actual == expected_sha and (expected_bytes is None or len(data) == expected_bytes)
    return Delivery(
        ok=ok,
        expected_sha256=expected_sha,
        actual_sha256=actual,
        expected_bytes=expected_bytes if expected_bytes is not None else len(data),
        actual_bytes=len(data),
        detail="" if ok else f"payload at {path} does not match the delivered hash",
        status=AttemptStatus.COMPLETED if ok else AttemptStatus.INPUT_DELIVERY_FAILED,
    )


def iter_argv_paths(argv: Iterable[str]) -> Iterable[Path]:
    for token in argv:
        if token.startswith("/") and Path(token).exists():
            yield Path(token)
