"""Git plumbing: diffs, merge bases, worktrees, blobs and patch application.

Everything shells out to real ``git``; no library re-implements the object
model.  Shallow or unrelated histories are a hard error (a review against the
wrong base is worse than no review).
"""

from __future__ import annotations

import re
import subprocess
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

from .errors import FailureKind, ReviewError
from .models import DiffBundle, FilePatch, Hunk


class GitError(ReviewError):
    def __init__(self, message: str, *, detail: str = ""):
        super().__init__(message, detail=detail)
        self.kind = FailureKind.INVALID_INPUT


@dataclass(frozen=True)
class GitResult:
    code: int
    stdout: str
    stderr: str


def _run(cwd: Path, args: Sequence[str], *, timeout: float = 60.0, env: dict[str, str] | None = None) -> GitResult:
    # Bytes, not text: a repository contains images, fonts and archives, and
    # `git show` on one of those must not raise UnicodeDecodeError inside a
    # symbol scan.  Callers that need exact bytes (patch application) use their
    # own path; everything here is a human-readable view.
    proc = subprocess.run(
        ["git", *args],
        cwd=str(cwd),
        capture_output=True,
        timeout=timeout,
        env=env,
    )
    return GitResult(
        proc.returncode,
        proc.stdout.decode("utf-8", "replace"),
        proc.stderr.decode("utf-8", "replace"),
    )


def _ok(cwd: Path, args: Sequence[str], *, timeout: float = 60.0) -> str:
    result = _run(cwd, args, timeout=timeout)
    if result.code != 0:
        raise GitError(
            f"git {' '.join(args)} failed",
            detail=(result.stderr or result.stdout).strip()[:2000],
        )
    return result.stdout.strip()


class GitRepo:
    def __init__(self, root: Path):
        self.root = Path(root).resolve()
        # Candidate probes run concurrently; worktree add/remove mutate the
        # shared .git/worktrees directory.
        self._worktree_lock = threading.RLock()

    # -- identity ---------------------------------------------------------
    def is_repo(self) -> bool:
        return _run(self.root, ["rev-parse", "--git-dir"]).code == 0

    def head_sha(self) -> str:
        return _ok(self.root, ["rev-parse", "HEAD"])

    def resolve(self, ref: str) -> str:
        return _ok(self.root, ["rev-parse", "--verify", f"{ref}^{{commit}}"])

    def merge_base(self, base: str, head: str) -> str:
        result = _run(self.root, ["merge-base", base, head])
        if result.code != 0 or not result.stdout.strip():
            raise GitError(
                f"no merge base between {base} and {head}; refusing a diff-only review of unrelated histories",
                detail=result.stderr[:1000],
            )
        return result.stdout.strip()

    def is_ancestor(self, ancestor: str, descendant: str) -> bool:
        result = _run(self.root, ["merge-base", "--is-ancestor", ancestor, descendant])
        if result.code == 0:
            return True
        if result.code == 1:
            return False
        raise GitError("could not evaluate ancestry", detail=result.stderr[:1000])

    def show(self, rev: str, path: str) -> str:
        result = _run(self.root, ["show", f"{rev}:{path}"])
        if result.code != 0:
            raise GitError(f"{path} does not exist at {rev}", detail=result.stderr[:500])
        return result.stdout

    def list_files(self, rev: str = "HEAD") -> list[str]:
        out = _ok(self.root, ["ls-tree", "-r", "--name-only", rev])
        return [line for line in out.splitlines() if line.strip()]

    def changed_files(self, base: str, head: str) -> list[str]:
        out = _ok(self.root, ["diff", "--name-only", "--no-renames", base, head])
        return [line for line in out.splitlines() if line.strip()]

    # -- diff -------------------------------------------------------------
    def diff(self, base: str, head: str, *, max_bytes: int = 8_000_000) -> DiffBundle:
        base_sha = self.resolve(base)
        head_sha = self.resolve(head)
        merge_base = self.merge_base(base_sha, head_sha)
        text = _ok(
            self.root,
            ["diff", "--no-color", "--find-renames", "--unified=0", "-a", merge_base, head_sha],
            timeout=120,
        )
        truncated = False
        if len(text.encode("utf-8")) > max_bytes:
            text = text.encode("utf-8")[:max_bytes].decode("utf-8", "ignore")
            truncated = True
        files = parse_unified_diff(text)
        return DiffBundle(
            base_sha=merge_base,
            head_sha=head_sha,
            files=files,
            patch_text=text,
            truncated=truncated,
        )

    def diff_text(self, base: str, head: str, *, max_bytes: int = 8_000_000) -> str:
        return self.diff(base, head, max_bytes=max_bytes).patch_text

    # -- working trees ----------------------------------------------------
    def add_worktree(self, sha: str, dest: Path) -> Path:
        dest = Path(dest)
        dest.parent.mkdir(parents=True, exist_ok=True)
        if dest.exists():
            raise GitError(f"worktree destination already exists: {dest}")
        with self._worktree_lock:
            _ok(self.root, ["worktree", "add", "--detach", str(dest), sha], timeout=180)
        return dest

    def remove_worktree(self, dest: Path) -> None:
        with self._worktree_lock:
            _run(self.root, ["worktree", "remove", "--force", str(dest)], timeout=120)
            _run(self.root, ["worktree", "prune"])

    def worktree_for(self, sha: str, dest: Path) -> Path:
        return self.add_worktree(sha, dest)

    def apply_patch(self, worktree: Path, patch: str, *, check: bool = True) -> GitResult:
        patch_path = Path(worktree) / ".zero_loop_patch.diff"
        patch_path.write_text(patch, encoding="utf-8")
        try:
            if check:
                checked = _run(Path(worktree), ["apply", "--check", "--whitespace=nowarn", patch_path.name])
                if checked.code != 0:
                    return checked
            return _run(Path(worktree), ["apply", "--whitespace=nowarn", patch_path.name])
        finally:
            patch_path.unlink(missing_ok=True)

    def blob_sha(self, rev: str, path: str) -> str:
        return _ok(self.root, ["rev-parse", f"{rev}:{path}"])


# --------------------------------------------------------------------------- #
# Unified diff parser
# --------------------------------------------------------------------------- #

_HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$")
_DEV_NULL = "/dev/null"


def parse_unified_diff(text: str) -> list[FilePatch]:
    files: list[FilePatch] = []
    current: FilePatch | None = None
    hunk: Hunk | None = None
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.startswith("diff --git "):
            current = FilePatch(path="")
            hunk = None
            files.append(current)
            i += 1
            continue
        if current is None:
            i += 1
            continue
        if line.startswith("new file mode"):
            current.is_new = True
            i += 1
            continue
        if line.startswith("deleted file mode"):
            current.is_deleted = True
            i += 1
            continue
        if line.startswith("rename from "):
            current.old_path = line[len("rename from ") :].strip()
            current.is_rename = True
            i += 1
            continue
        if line.startswith("rename to "):
            current.path = line[len("rename to ") :].strip()
            current.is_rename = True
            i += 1
            continue
        if line.startswith("Binary files ") or line.startswith("GIT binary patch"):
            current.is_binary = True
            i += 1
            continue
        if line.startswith("--- "):
            path = line[4:].strip()
            if path != _DEV_NULL:
                current.old_path = _strip_prefix(path)
                if not current.path:
                    current.path = current.old_path
            i += 1
            continue
        if line.startswith("+++ "):
            path = line[4:].strip()
            if path != _DEV_NULL:
                current.path = _strip_prefix(path)
            i += 1
            continue
        match = _HUNK.match(line)
        if match and current is not None:
            old_start = int(match.group(1))
            old_lines = int(match.group(2) or "1")
            new_start = int(match.group(3))
            new_lines = int(match.group(4) or "1")
            hunk = Hunk(
                old_start=old_start,
                old_lines=old_lines,
                new_start=new_start,
                new_lines=new_lines,
                header=line,
                lines=[],
            )
            current.hunks.append(hunk)
            i += 1
            continue
        if hunk is not None and (line.startswith(("+", "-", " ", "\\")) or line == ""):
            hunk.lines.append(line if line else " ")
            if line.startswith("+") and not line.startswith("+++"):
                current.additions += 1
            elif line.startswith("-") and not line.startswith("---"):
                current.deletions += 1
            i += 1
            continue
        i += 1
    out = [f for f in files if f.path]
    for f in out:
        if not f.path and f.old_path:
            f.path = f.old_path
    return out


def _strip_prefix(path: str) -> str:
    if path.startswith("a/") or path.startswith("b/"):
        return path[2:]
    return path


# --------------------------------------------------------------------------- #
# Structured facts about a diff (used by PR-MCI and triage; never the raw code)
# --------------------------------------------------------------------------- #


def diff_facts(bundle: DiffBundle) -> dict[str, object]:
    """A compact, deterministic description of what the patch does structurally."""
    files: list[dict[str, object]] = []
    for f in bundle.files:
        added: list[str] = []
        removed: list[str] = []
        touched: set[str] = set()
        for hunk in f.hunks:
            for line in hunk.lines:
                body = line[1:] if line[:1] in "+- " else line
                if line.startswith("+") and not line.startswith("+++"):
                    added.append(body)
                    touched.update(re.findall(r"\b[A-Za-z_][A-Za-z0-9_]*\b", body))
                elif line.startswith("-") and not line.startswith("---"):
                    removed.append(body)
                    touched.update(re.findall(r"\b[A-Za-z_][A-Za-z0-9_]*\b", body))
        added_defs = _definitions(added)
        removed_defs = _definitions(removed)
        files.append(
            {
                "path": f.path,
                "is_new": f.is_new,
                "is_deleted": f.is_deleted,
                "additions": f.additions,
                "deletions": f.deletions,
                "added_definitions": added_defs,
                "removed_definitions": removed_defs,
                "modified_definitions": _modified(added_defs, removed_defs),
                "added_calls": _calls(added),
                "added_returns": _returns(added),
                "changed_identifiers": sorted(touched)[:80],
                "added_lines": [
                    {"line": n, "text": text[:300]}
                    for n, text in _added_lines(f)
                ][:120],
            }
        )
    return {
        "base_sha": bundle.base_sha,
        "head_sha": bundle.head_sha,
        "files": files,
    }


_DEF = re.compile(r"^\s*(?:async\s+)?(?:def|function|func|fn)\s+([A-Za-z_][A-Za-z0-9_]*)")
_CLASS = re.compile(r"^\s*(?:class|struct|interface|type)\s+([A-Za-z_][A-Za-z0-9_]*)")
_CALL = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(")
_RETURN = re.compile(r"^\s*return\b(.*)$")


def _definitions(lines: Iterable[str]) -> list[str]:
    out: list[str] = []
    for line in lines:
        m = _DEF.match(line) or _CLASS.match(line)
        if m:
            out.append(m.group(1))
    return out[:60]


def _modified(added: Sequence[str], removed: Sequence[str]) -> list[str]:
    # A definition present on both sides but whose body changed is "modified";
    # the diff cannot tell us the body, so report the name only when the
    # signature line itself changed.
    add = set(added)
    rem = set(removed)
    return sorted(n for n in add & rem)[:60]


def _calls(lines: Iterable[str]) -> list[str]:
    out: set[str] = set()
    for line in lines:
        out.update(_CALL.findall(line))
    keywords = {"if", "for", "while", "return", "print", "len", "range", "int", "str", "list", "dict", "set", "tuple", "super", "isinstance"}
    return sorted(c for c in out if c not in keywords)[:80]


def _returns(lines: Iterable[str]) -> list[str]:
    out: list[str] = []
    for line in lines:
        m = _RETURN.match(line)
        if m:
            out.append(m.group(1).strip()[:200])
    return out[:40]


def _added_lines(f: FilePatch) -> list[tuple[int, str]]:
    out: list[tuple[int, str]] = []
    for hunk in f.hunks:
        cursor = hunk.new_start
        for line in hunk.lines:
            if line.startswith("+") and not line.startswith("+++"):
                out.append((cursor, line[1:]))
                cursor += 1
            elif line.startswith("-") and not line.startswith("---"):
                continue
            elif line.startswith("\\"):
                continue
            else:
                cursor += 1
    return out
