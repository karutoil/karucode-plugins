"""Stage 4A — CIVER: a minimal repair proposal, verified or not stored at all.

The model only ever *proposes*.  The host decides, on a scratch copy of the
head worktree, in this order:

1. the patch applies (``git apply --check``, then apply);
2. ``python3 -m compileall -q`` succeeds on the patched copy;
3. the configured linter, **only** when its executable exists (a missing
   binary is recorded as skipped, never as a pass);
4. the defect signal disappears on the patched copy *and* was present on the
   unpatched head (``defect_probe``, or an analyzer rerun callback);
5. a whitespace-only perturbation of the same head does **not** remove the
   signal.  If it does, the signal is not attributable to the patch, so the
   fix is not stored.

Every failure returns ``CiverResult(passed=False)`` with a reason for the
receipt: there is no "probably fine" path, and a failed verification never
produces a ``Fix``.
"""

from __future__ import annotations

import re
import shutil
import tempfile
import uuid
from pathlib import Path
from typing import Callable, Protocol, Sequence

from pydantic import ValidationError

from ..config import Settings
from ..errors import LLMError
from ..gitops import GitError, GitRepo
from ..llm.prompts import civer_prompt
from ..models import (
    AttemptStatus,
    Candidate,
    CiverResult,
    ContextSlice,
    ExecResult,
    Fix,
    PatchProposal,
)
from ..stage1.contract import run_bounded

#: ``probe(tree) -> True`` when the defect signal is still present.
Scan = Callable[[Path], bool]

_COMPILE_ARGV = ("python3", "-m", "compileall", "-q", ".")
_BENIGN = frozenset({AttemptStatus.COMPLETED, AttemptStatus.EMPTY_RESPONSE})
_IGNORED = (
    ".git",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "target",          # cargo's build cache: tens of GB in a real workspace
    "node_modules",    # dependencies are not source and can be hundreds of MB
    "dist",
    "build",
    "site",            # generated docs; CIVER needs the patched source, not HTML
    ".site-docs",
    ".venv",
    "venv",
    ".next",
    ".turbo",
    ".cache",
    ".kc-data",
    ".karucode",
    "coverage",
)
_HEADER = re.compile(r"^(?:---|\+\+\+)\s+(?:[ab]/)?(\S+)\s*$")


class CommandRunner(Protocol):
    """The only way this stage starts a host command."""

    def run(self, argv: list[str], *, cwd: Path, timeout_s: float) -> ExecResult: ...


class LocalCommandRunner:
    """Default runner: one bounded, typed subprocess per command."""

    def run(self, argv: list[str], *, cwd: Path, timeout_s: float) -> ExecResult:
        try:
            run = run_bounded(list(argv), cwd=Path(cwd), timeout_s=float(timeout_s))
        except FileNotFoundError:
            return ExecResult(
                argv=list(argv),
                exit_code=None,
                stderr=f"{argv[0]!r} is not installed",
                status=AttemptStatus.TOOL_UNAVAILABLE,
            )
        except OSError as exc:
            return ExecResult(
                argv=list(argv),
                exit_code=None,
                stderr=f"{type(exc).__name__}: {exc}",
                status=AttemptStatus.PROCESS_CRASH,
            )
        return ExecResult(
            argv=list(argv),
            exit_code=run.exit_code,
            stdout=run.stdout,
            stderr=run.stderr,
            duration_ms=run.duration_ms,
            status=run.status,
        )


# --------------------------------------------------------------------------- #
# 4A.1 — proposal
# --------------------------------------------------------------------------- #


def propose_fix(candidate: Candidate, context: ContextSlice, llm) -> PatchProposal | None:
    """Ask for one minimal unified diff; ``None`` when nothing usable came back.

    A transport or schema failure is *not* turned into a finding either way:
    the candidate keeps its status and simply carries no suggestion, which the
    formatter treats as "not publishable" rather than "approved".
    """
    prompt = civer_prompt(candidate, context)
    try:
        answer = llm.complete("civer", prompt, PatchProposal)
    except (LLMError, ValidationError):
        return None
    value = getattr(answer, "value", answer)
    try:
        proposal = value if isinstance(value, PatchProposal) else PatchProposal.model_validate(value)
    except ValidationError:
        return None
    if not proposal.patch.strip():
        return None
    return proposal


# --------------------------------------------------------------------------- #
# 4A.2 — verification
# --------------------------------------------------------------------------- #


def verify_fix(
    candidate: Candidate,
    proposal: PatchProposal,
    repo: GitRepo,
    head_sha: str,
    work_root: Path,
    runner: CommandRunner,
    settings: Settings,
    *,
    defect_probe: Scan | None = None,
    analyzer_rerun: Scan | None = None,
    linter: Sequence[str] | None = None,
    tool_root: Path | None = None,
) -> CiverResult:
    """Run every check on scratch copies; store a ``Fix`` only if all pass.

    ``defect_probe`` (injected by the pipeline) returns ``True`` while the
    defect signal is present.  When it is absent, ``analyzer_rerun`` is used
    with the same meaning (the rule still fires).  With neither callback the
    removal cannot be attributed to the patch, so the result is
    ``passed=False``.
    """
    work_root = Path(work_root)
    try:
        resolved = repo.resolve(head_sha)
    except GitError as exc:
        return CiverResult(passed=False, detail=f"unknown_head: {exc}")

    probe: Scan | None = defect_probe or analyzer_rerun
    if probe is None:
        return CiverResult(
            passed=False,
            detail=(
                "defect_removal_unverified: no defect_probe and no analyzer rerun callback was "
                "supplied, so a patch cannot be shown to remove the defect"
            ),
        )
    unsafe = _unsafe_paths(proposal.patch)
    if unsafe:
        return CiverResult(
            passed=False,
            detail="patch_rejected: the patch names paths outside the worktree: " + ", ".join(unsafe),
        )

    checks: list[str] = []
    head_tree: Path | None = None
    scratch: list[Path] = []
    try:
        try:
            work_root.mkdir(parents=True, exist_ok=True)
            head_tree = repo.add_worktree(resolved, work_root / f"civer-head-{uuid.uuid4().hex[:10]}")
        except (GitError, OSError) as exc:
            return CiverResult(passed=False, detail=f"head_worktree_unavailable: {exc}")

        patched = _scratch_copy(head_tree, work_root)
        if patched is None:
            return CiverResult(passed=False, detail="scratch_unavailable: the head worktree could not be copied")
        scratch.append(patched)

        rejected = _apply_patch(proposal.patch, patched, runner, settings, work_root)
        if rejected:
            return _fail(rejected, checks)
        checks.append("patch_applies: git apply --check")

        syntax_commands, unavailable = _syntax_commands(
            _patch_paths(proposal.patch), patched, tool_root
        )
        if unavailable:
            return _fail(f"syntax_checker_unavailable: {unavailable}", checks)
        if not syntax_commands:
            checks.append("syntax_check_skipped: no recognized source file in the patch")
        for label, argv in syntax_commands:
            checked = _command(runner, argv, patched, settings)
            if not _succeeded(checked):
                return _fail(
                    f"{label}_failed: {' '.join(argv)} exited {checked.exit_code}: {_tail(checked)}",
                    checks,
                )
            checks.append(f"{label}_ok: " + " ".join(argv))

        lint_ok = False
        command = _linter_argv(linter, settings)
        if not command:
            checks.append("linter_skipped: no linter is configured")
        else:
            lint = _command(runner, command, patched, settings)
            if lint.status is AttemptStatus.TOOL_UNAVAILABLE:
                checks.append(f"linter_skipped: {command[0]} is not installed")
            elif _succeeded(lint):
                checks.append("lint_ok: " + " ".join(command))
                lint_ok = True
            else:
                return _fail(
                    f"lint_failed: {' '.join(command)} exited {lint.exit_code}: {_tail(lint)}",
                    checks,
                )

        present_before, why = _signal(probe, head_tree)
        if present_before is None:
            return _fail(f"defect_probe_crashed: {why}", checks)
        if not present_before:
            return _fail(
                "defect_not_reproduced: the defect signal is already absent on the unpatched head, "
                "so its disappearance would not be evidence",
                checks,
            )
        present_after, why = _signal(probe, patched)
        if present_after is None:
            return _fail(f"defect_probe_crashed: {why}", checks)
        if present_after:
            return _fail(
                "defect_removed: the defect signal is still present after applying the patch",
                checks,
            )
        checks.append("defect_removed: the signal is gone on the patched head and present on the unpatched head")

        noop_patch = _whitespace_patch(head_tree, _target_paths(proposal))
        if noop_patch is None:
            return _fail(
                "semantic_noop_unavailable: no Python file in the patch could take a whitespace-only "
                "perturbation, so the signal cannot be attributed to the patch",
                checks,
            )
        noop_tree = _scratch_copy(head_tree, work_root)
        if noop_tree is None:
            return _fail("scratch_unavailable: the head worktree could not be copied for the no-op probe", checks)
        scratch.append(noop_tree)
        rejected = _apply_patch(noop_patch, noop_tree, runner, settings, work_root)
        if rejected:
            return _fail(f"semantic_noop_unverifiable: {rejected}", checks)
        noop_compiled = _command(runner, list(_COMPILE_ARGV), noop_tree, settings)
        if not _succeeded(noop_compiled):
            return _fail(
                "semantic_noop_unverifiable: the whitespace-only perturbation did not compile: "
                f"{_tail(noop_compiled)}",
                checks,
            )
        noop_signal, why = _signal(probe, noop_tree)
        if noop_signal is None:
            return _fail(f"defect_probe_crashed: {why}", checks)
        if not noop_signal:
            return CiverResult(
                passed=False,
                semantic_noop_signal=True,
                detail=(
                    "semantic_noop_outperforms: a whitespace-only perturbation also removes the defect "
                    "signal, so the probe cannot attribute the repair to this patch"
                ),
                attempts=1,
            )
        checks.append("semantic_noop: the whitespace-only perturbation left the signal in place")

        fix = Fix(
            patch=proposal.patch,
            files_changed=_dedupe(list(proposal.files_changed) + _patch_paths(proposal.patch)),
            applies_to_sha=resolved,
            verified=True,
            compile_ok=True,
            lint_ok=lint_ok,
            defect_removed=True,
            noop_outperformed=True,
            checks=checks,
            private=False,
        )
        return CiverResult(
            passed=True,
            fix=fix,
            semantic_noop_signal=False,
            detail=f"civer verified a minimal repair for {candidate.id}: {len(checks)} checks passed",
            attempts=1,
        )
    finally:
        for path in scratch:
            shutil.rmtree(path, ignore_errors=True)
        if head_tree is not None:
            try:
                repo.remove_worktree(head_tree)
            except Exception:  # cleanup must never mask the verification outcome
                pass


# --------------------------------------------------------------------------- #
# Helpers
# --------------------------------------------------------------------------- #


def _fail(detail: str, checks: Sequence[str]) -> CiverResult:
    return CiverResult(passed=False, detail=f"{detail} (checks: {', '.join(checks) or 'none'})", attempts=1)


def _timeout_s(settings: Settings) -> float:
    return max(5.0, min(float(getattr(settings, "deadline_s", 120.0) or 120.0), 120.0))


def _command(runner: CommandRunner, argv: Sequence[str], cwd: Path, settings: Settings) -> ExecResult:
    try:
        return runner.run(list(argv), cwd=Path(cwd), timeout_s=_timeout_s(settings))
    except Exception as exc:  # a runner that raises is a crashed command, not a pass
        return ExecResult(
            argv=list(argv),
            exit_code=None,
            stderr=f"{type(exc).__name__}: {exc}",
            status=AttemptStatus.PROCESS_CRASH,
        )


def _succeeded(result: ExecResult) -> bool:
    return result.exit_code == 0 and result.status in _BENIGN


def _tail(result: ExecResult, limit: int = 300) -> str:
    text = (result.stderr or "").strip() or (result.stdout or "").strip()
    return text[-limit:] if text else "no output"


def _scratch_copy(source: Path, parent: Path) -> Path | None:
    parent.mkdir(parents=True, exist_ok=True)
    dest = Path(tempfile.mkdtemp(prefix="civer-", dir=str(parent)))
    try:
        shutil.copytree(
            source,
            dest,
            symlinks=True,
            dirs_exist_ok=True,
            ignore=shutil.ignore_patterns(*_IGNORED),
        )
    except OSError:
        shutil.rmtree(dest, ignore_errors=True)
        return None
    return dest


def _apply_patch(
    patch: str, tree: Path, runner: CommandRunner, settings: Settings, work_root: Path
) -> str:
    """``git apply --check`` then ``git apply``; "" on success, else the reason."""
    patch_file = Path(work_root) / f"civer-patch-{uuid.uuid4().hex[:10]}.diff"
    body = patch if patch.endswith("\n") else patch + "\n"
    patch_file.write_text(body, encoding="utf-8")
    try:
        check = _command(
            runner,
            ["git", "apply", "--check", "--whitespace=nowarn", str(patch_file)],
            tree,
            settings,
        )
        zero_context = False
        if not _succeeded(check):
            # Model-authored diffs often omit the final context line.  Git
            # rejects that hunk even though it applies exactly at the recorded
            # line; ask Git for the zero-context interpretation instead of
            # discarding an otherwise valid repair.  The compile/analyzer/no-op
            # checks still decide whether the change is acceptable.
            check = _command(
                runner,
                ["git", "apply", "--check", "--unidiff-zero", "--whitespace=nowarn", str(patch_file)],
                tree,
                settings,
            )
            zero_context = True
            if not _succeeded(check):
                return f"patch_rejected: git apply --check exited {check.exit_code}: {_tail(check)}"
        apply_args = ["git", "apply", "--whitespace=nowarn"]
        if zero_context:
            apply_args.append("--unidiff-zero")
        apply_args.append(str(patch_file))
        applied = _command(runner, apply_args, tree, settings)
        if not _succeeded(applied):
            return f"patch_rejected: git apply exited {applied.exit_code}: {_tail(applied)}"
        return ""
    finally:
        patch_file.unlink(missing_ok=True)


def _signal(probe: Scan, tree: Path) -> tuple[bool | None, str]:
    try:
        return bool(probe(Path(tree))), ""
    except Exception as exc:  # a probe that cannot run proves nothing
        return None, f"{type(exc).__name__}: {exc}"


def _linter_argv(linter: Sequence[str] | None, settings: Settings) -> list[str]:
    if linter is not None:
        command = [str(part) for part in linter if str(part)]
    else:
        configured = getattr(settings, "linter", None)
        if isinstance(configured, str):
            command = configured.split()
        elif configured:
            command = [str(part) for part in configured]
        else:
            command = []
    if not command:
        return []
    # A linter is run over the tree root; no write-enabling flag is ever added.
    return [*command, "."]


def _syntax_commands(
    files: Sequence[str], tree: Path, tool_root: Path | None
) -> tuple[list[tuple[str, list[str]]], str]:
    """Language-aware parse checks for the *patched* tree.

    A Python-only ``compileall`` silently succeeded for every Rust or TypeScript
    repair.  These commands are parse/compile checks, not the project's full
    type-checker: they catch a malformed one-click suggestion before it is
    published.  A missing checker fails closed.
    """
    existing = [
        path
        for path in files
        if not path.startswith("/") and ".." not in Path(path).parts and (tree / path).is_file()
    ]
    commands: list[tuple[str, list[str]]] = []
    python = [path for path in existing if path.endswith((".py", ".pyi"))]
    rust = [path for path in existing if path.endswith(".rs")]
    javascript = [path for path in existing if path.endswith((".js", ".mjs", ".cjs"))]
    ts = [path for path in existing if path.endswith((".ts", ".tsx", ".jsx"))]
    if python:
        commands.append(("compile", ["python3", "-m", "compileall", "-q", *python]))
    for path in rust:
        commands.append(("syntax", ["rustfmt", "--emit", "stdout", "--quiet", path]))
    for path in javascript:
        commands.append(("syntax", ["node", "--check", path]))
    if ts:
        esbuild = _find_esbuild(tool_root)
        if esbuild is None:
            return [], (
                "a TypeScript/TSX repair needs esbuild for a parse check and no "
                "node_modules/.bin/esbuild was found; install the web dependencies "
                "or leave the finding unverified"
            )
        for path in ts:
            loader = "tsx" if path.endswith(".tsx") else ("jsx" if path.endswith(".jsx") else "ts")
            commands.append(
                (
                    "syntax",
                    [
                        esbuild,
                        path,
                        f"--loader:.{loader}={loader}",
                        "--outfile=/dev/null",
                        "--log-level=error",
                    ],
                )
            )
    return commands, ""


def _find_esbuild(tool_root: Path | None) -> str | None:
    candidates: list[Path] = []
    if tool_root is not None:
        root = Path(tool_root)
        candidates.extend(
            [
                root / "web" / "node_modules" / ".bin" / "esbuild",
                root / "node_modules" / ".bin" / "esbuild",
            ]
        )
    for candidate in candidates:
        if candidate.is_file():
            return str(candidate)
    return shutil.which("esbuild")


def _patch_paths(patch: str) -> list[str]:
    out: list[str] = []
    for line in patch.splitlines():
        match = _HEADER.match(line)
        if not match:
            continue
        path = match.group(1)
        if path == "/dev/null" or path in out:
            continue
        out.append(path)
    return out


def _unsafe_paths(patch: str) -> list[str]:
    bad: list[str] = []
    for path in _patch_paths(patch):
        parts = Path(path).parts
        if path.startswith("/") or ".." in parts or (parts and parts[0].endswith(":")):
            if path not in bad:
                bad.append(path)
    return bad


def _dedupe(values: Sequence[str]) -> list[str]:
    out: list[str] = []
    for value in values:
        text = str(value).strip()
        if text and text not in out:
            out.append(text)
    return out


def _target_paths(proposal: PatchProposal) -> list[str]:
    ordered = _dedupe(list(proposal.files_changed) + _patch_paths(proposal.patch))
    # Python first: the semantic no-op must survive compileall.
    indexed = sorted(enumerate(ordered), key=lambda item: (not item[1].endswith(".py"), item[0]))
    return [rel for _index, rel in indexed]


def _whitespace_patch(tree: Path, candidates: Sequence[str]) -> str | None:
    """A patch that adds one trailing space: valid diff, zero semantics."""
    for rel in candidates:
        if not _safe_relative(rel):
            continue
        path = Path(tree) / rel
        if not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if not text.endswith("\n"):
            continue
        lines = text.split("\n")[:-1]
        for index, line in enumerate(lines):
            if not line.strip() or line.endswith(" "):
                continue
            if line.rstrip().endswith("\\"):
                continue
            start = max(0, index - 3)
            stop = min(len(lines), index + 4)
            before = lines[start:index]
            after = lines[index + 1 : stop]
            body = [" " + context for context in before]
            body.append("-" + line)
            body.append("+" + line + " ")
            body.extend(" " + context for context in after)
            header = f"@@ -{start + 1},{len(before) + 1 + len(after)} +{start + 1},{len(before) + 1 + len(after)} @@"
            return "\n".join([f"--- a/{rel}", f"+++ b/{rel}", header, *body]) + "\n"
    return None


def _safe_relative(rel: str) -> bool:
    parts = Path(rel).parts
    return bool(parts) and not rel.startswith("/") and ".." not in parts
