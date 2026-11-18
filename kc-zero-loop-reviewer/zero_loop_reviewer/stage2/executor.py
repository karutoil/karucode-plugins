"""Stage 2 executor — three tracks, one hard wall clock.

    2A  Semgrep delta        -> AnalyzerFinding (patch_caused + suppressions)
    2B  CPG frames + secSem  -> CpgFrame, SecSemScan, Candidate
    2C  RALE                 -> RaleResult

The tracks run on a thread pool.  ``settings.stage2_timeout_s`` bounds the whole
stage; a track that fails (or overruns) contributes a ``degraded`` reason and
never aborts its siblings.  Stage 2C results are file-level structural signals
without a verified line, so they are reported in ``rale`` for the later stages
instead of being emitted as candidates; the only Stage 2 candidate source is the
2B reconciliation (which carries cited, reconciled lines).
"""

from __future__ import annotations

import time
from concurrent.futures import Future, ThreadPoolExecutor, TimeoutError as FutureTimeout, wait
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping, Sequence

from ..config import Settings
from ..errors import ToolError
from ..gitops import GitRepo
from ..models import (
    AnalyzerFinding,
    Candidate,
    ContextSlice,
    CpgFrame,
    DiffBundle,
    RaleResult,
    SecSemScan,
)
from .classifiers import default_classifier
from .cpg import frame_projection, parse_policy, taint_and_lifetime
from .rale import rale
from .sast import apply_suppressions, patch_caused, run_semgrep
from .secsem import run_security

#: Policy shipped with the plugin, relative to the package root.
DEFAULT_POLICY = Path(__file__).resolve().parents[2] / "rules" / "policy.json"

#: Bounds on how much of the diff is re-read for the CPG/RALE views.
MAX_SOURCE_FILES = 500
MAX_SOURCE_BYTES = 400_000


@dataclass
class Stage2Outcome:
    candidates: list[Candidate] = field(default_factory=list)
    findings: list[AnalyzerFinding] = field(default_factory=list)
    frames: list[CpgFrame] = field(default_factory=list)
    rale: list[RaleResult] = field(default_factory=list)
    scans: list[SecSemScan] = field(default_factory=list)
    degraded: list[str] = field(default_factory=list)
    elapsed_ms: int = 0


@dataclass
class _SastTrack:
    findings: list[AnalyzerFinding] = field(default_factory=list)
    degraded: list[str] = field(default_factory=list)


@dataclass
class _SecurityTrack:
    scan: SecSemScan | None = None
    frames: list[CpgFrame] = field(default_factory=list)
    candidates: list[Candidate] = field(default_factory=list)
    degraded: list[str] = field(default_factory=list)


@dataclass
class _RaleTrack:
    results: list[RaleResult] = field(default_factory=list)
    degraded: list[str] = field(default_factory=list)


@dataclass
class _Source:
    path: str
    text: str
    language: str


# --------------------------------------------------------------------------- #
# Entry point
# --------------------------------------------------------------------------- #


def run_stage2(
    repo_root: Path,
    repo: GitRepo,
    diff: DiffBundle,
    context: ContextSlice,
    llm: Any,
    settings: Settings,
    *,
    environ: Mapping[str, str] | None = None,
) -> Stage2Outcome:
    """Run the three Stage 2 tracks concurrently under one deadline."""
    started = time.monotonic()
    timeout_s = max(1.0, float(settings.stage2_timeout_s))
    deadline = started + timeout_s
    root = Path(repo_root)

    pool = ThreadPoolExecutor(max_workers=3, thread_name_prefix="stage2")
    futures: dict[str, Future] = {
        "2a": pool.submit(_sast_track, root, diff, settings, environ),
        "2c": pool.submit(_rale_track, root, repo, diff, settings),
    }
    futures["2b"] = pool.submit(
        _security_track, root, repo, diff, context, futures["2a"], llm, settings, deadline
    )
    wait(futures.values(), timeout=timeout_s)
    pool.shutdown(wait=False, cancel_futures=True)

    results: dict[str, Any] = {}
    degraded: list[str] = []
    for name in ("2a", "2b", "2c"):
        future = futures[name]
        if not future.done():
            degraded.append(f"{name} track did not finish within stage2_timeout_s={timeout_s:.1f}s")
            continue
        try:
            results[name] = future.result()
        except Exception as exc:  # a track must never abort the stage
            degraded.append(f"{name} track failed: {type(exc).__name__}: {exc}")

    sast: _SastTrack = results.get("2a") or _SastTrack()
    security: _SecurityTrack = results.get("2b") or _SecurityTrack()
    rale_track: _RaleTrack = results.get("2c") or _RaleTrack()

    for track in (sast, security, rale_track):
        degraded.extend(track.degraded)

    outcome = Stage2Outcome(
        candidates=sorted(security.candidates, key=_candidate_key),
        findings=sorted(sast.findings, key=lambda f: (f.path, f.line, f.rule_id)),
        frames=sorted(security.frames, key=lambda f: f.path),
        rale=sorted(rale_track.results, key=lambda r: r.path),
        scans=[security.scan] if security.scan is not None else [],
        degraded=_dedupe(degraded),
        elapsed_ms=max(0, int((time.monotonic() - started) * 1000)),
    )
    return outcome


def _candidate_key(candidate: Candidate) -> tuple[int, float, str]:
    return (
        -int(candidate.severity),
        -float(candidate.p if candidate.p is not None else 0.0),
        candidate.id,
    )


def _dedupe(messages: Sequence[str]) -> list[str]:
    seen: set[str] = set()
    out: list[str] = []
    for message in messages:
        if not message or message in seen:
            continue
        seen.add(message)
        out.append(message)
    return out


# --------------------------------------------------------------------------- #
# Track 2A — Semgrep delta
# --------------------------------------------------------------------------- #


def _sast_track(
    root: Path, diff: DiffBundle, settings: Settings, environ: Mapping[str, str] | None
) -> _SastTrack:
    try:
        outcome = run_semgrep(root, diff, settings, environ=environ)
    except Exception as exc:
        return _SastTrack(degraded=[f"2a semgrep could not run: {type(exc).__name__}: {exc}"])
    findings = patch_caused(outcome.findings, diff)
    findings = apply_suppressions(findings, diff, settings.sast)
    degraded: list[str] = []
    if not outcome.status.is_success:
        degraded.append(f"2a semgrep {outcome.status.value}: {(outcome.detail or 'no detail')[:300]}")
    return _SastTrack(findings=findings, degraded=degraded)


# --------------------------------------------------------------------------- #
# Track 2B — CPG frames + security monologue
# --------------------------------------------------------------------------- #


def _security_track(
    root: Path,
    repo: GitRepo,
    diff: DiffBundle,
    context: ContextSlice,
    sast_future: Future,
    llm: Any,
    settings: Settings,
    deadline: float,
) -> _SecurityTrack:
    track = _SecurityTrack()
    sources, source_notes = _sources(root, repo, diff)
    track.degraded.extend(source_notes)
    for item in sources:
        try:
            track.frames.append(frame_projection(item.path, item.text, item.language))
        except Exception as exc:  # a single file must not lose the track
            track.degraded.append(f"2b frame projection failed for {item.path}: {type(exc).__name__}: {exc}")
    track.frames.sort(key=lambda frame: frame.path)

    analyzer: list[AnalyzerFinding] = []
    remaining = deadline - time.monotonic()
    try:
        sast: _SastTrack = sast_future.result(timeout=max(0.0, remaining))
        analyzer = sast.findings
    except FutureTimeout:
        track.degraded.append("2b analyzer trace unavailable: the 2A track did not finish in time")
    except Exception as exc:
        track.degraded.append(f"2b analyzer trace unavailable: {type(exc).__name__}: {exc}")

    policy: dict = {"rules": []}
    try:
        policy = parse_policy(DEFAULT_POLICY)
    except ToolError as exc:
        track.degraded.append(f"2b cpg policy unavailable: {exc.message}")

    # Attach the analyzer-driven taint/lifetime tuples to the frames.  Without
    # this the frames describe memory structure only, and every consumer that
    # needs a source→sink path (secSem evidence, redaction, ObliVul) sees none.
    try:
        taint, lifetime = taint_and_lifetime(analyzer, policy)
    except Exception as exc:
        taint, lifetime = [], []
        track.degraded.append(f"2b cpg tuples failed: {type(exc).__name__}: {exc}")
    if taint or lifetime:
        merged: list[CpgFrame] = []
        for frame in track.frames:
            frame_taint = [t for t in taint if t.source.path == frame.path or t.sink.path == frame.path]
            frame_lifetime = [
                item
                for item in lifetime
                if any(site is not None and site.path == frame.path for site in (item.alloc, item.free, item.use))
            ]
            merged.append(
                frame.model_copy(update={"taint": frame_taint, "lifetime": frame_lifetime})
                if (frame_taint or frame_lifetime)
                else frame
            )
        track.frames = merged

    scan, _reconciliation, candidates, detail = run_security(
        diff, context, analyzer, track.frames, llm, settings, policy=policy
    )
    track.scan = scan
    track.candidates = candidates
    if scan is None:
        track.degraded.append(f"2b {detail}")
    return track


# --------------------------------------------------------------------------- #
# Track 2C — RALE
# --------------------------------------------------------------------------- #


def _rale_track(root: Path, repo: GitRepo, diff: DiffBundle, settings: Settings) -> _RaleTrack:
    track = _RaleTrack()
    sources, source_notes = _sources(root, repo, diff)
    track.degraded.extend(source_notes)
    classifier = default_classifier(settings)
    for item in sources:
        if not item.path.endswith((".py", ".pyw", ".pyi")):
            continue
        try:
            track.results.append(rale(item.path, item.text, classifier=classifier))
        except Exception as exc:
            track.degraded.append(f"2c RALE failed for {item.path}: {type(exc).__name__}: {exc}")
    track.results.sort(key=lambda result: result.path)
    return track


# --------------------------------------------------------------------------- #
# Reading the changed sources at the head commit
# --------------------------------------------------------------------------- #


def _sources(root: Path, repo: GitRepo, diff: DiffBundle) -> tuple[list[_Source], list[str]]:
    notes: list[str] = []
    out: list[_Source] = []
    for patch in diff.files:
        if patch.is_deleted or patch.is_binary:
            continue
        if len(out) >= MAX_SOURCE_FILES:
            notes.append(f"stage 2 source view truncated at {MAX_SOURCE_FILES} changed files")
            break
        language = _language(patch.path)
        text = _read_source(root, repo, patch.path, diff.head_sha)
        if text is None:
            notes.append(f"could not read {patch.path} at {diff.head_sha[:12] or 'HEAD'}")
            continue
        out.append(_Source(path=patch.path, text=text, language=language))
    return out, notes


def _read_source(root: Path, repo: GitRepo, path: str, head_sha: str) -> str | None:
    on_disk = root / path
    try:
        if on_disk.is_file() and on_disk.stat().st_size <= MAX_SOURCE_BYTES:
            return on_disk.read_text(encoding="utf-8", errors="replace")
    except OSError:
        pass
    try:
        text = repo.show(head_sha or "HEAD", path)
    except Exception:
        return None
    return text[:MAX_SOURCE_BYTES]


def _language(path: str) -> str:
    lowered = path.lower()
    if lowered.endswith((".py", ".pyw", ".pyi")):
        return "python"
    if lowered.endswith((".cc", ".cpp", ".cxx", ".hpp", ".hh", ".hxx")):
        return "cpp"
    if lowered.endswith((".c", ".h")):
        return "c"
    return ""
