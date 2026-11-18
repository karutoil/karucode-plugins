"""Stage 1D — 1-hop context slicing, the token budget, and convention loading.

Three jobs, all pure of the pull request description:

* :class:`TokenCounter` — the chars/4 estimate used everywhere a budget is
  checked (floor 1, so an empty unit still costs a token);
* :func:`extract_symbols` — definitions a diff touches, plus their 1-hop
  callers and callees.  Python uses ``ast``; other languages use the
  ``ast-grep`` CLI when it is installed and a bounded regex walk otherwise;
* :func:`load_conventions` / :func:`build_context` — ``AGENTS.md``,
  ``.cursorrules``, ``.pr_agent.toml`` and ``.zero_loop/designlore.json`` read
  from the **head commit** (``GitRepo.show``), assembled into ``ContextUnit``s,
  sorted by priority and cut whole-unit-at-a-time until the token budget holds.

``build_context`` has no parameter that could carry a PR description and reads
nothing that could (no commit messages, no provider data, no working tree), so
author text cannot leak into a reviewer prompt through this module.

Deviations worth knowing: for non-Python files the definition list comes from a
bounded regex over the changed lines (exact enough to name the symbol), while
the *call-site* search uses ``ast-grep`` when present.  1-hop discovery is
bounded by ``max_files`` and an internal character budget so a huge repository
cannot blow the stage deadline.
"""

from __future__ import annotations

import ast
import builtins
import json
import re
import shutil
import tomllib
from pathlib import Path
from typing import Any, Iterable, Sequence

from ..config import Settings
from ..gitops import GitError, GitRepo
from ..models import (
    ContextSlice,
    ContextUnit,
    ConventionDirective,
    DesignPrecedent,
    DiffBundle,
    FilePatch,
    SymbolView,
    TriagePlan,
    sha256_hex,
)
from .contract import run_bounded

CHARS_PER_TOKEN = 4
#: Files read while resolving 1-hop symbols; never summarise the whole tree.
SCAN_CHAR_BUDGET = 4_000_000
#: Hard cap on emitted symbols.
MAX_SYMBOLS = 80
#: Bound for one ast-grep invocation.
AST_GREP_TIMEOUT_S = 20.0
#: One unit's text never exceeds this many characters (before the diff cap).
MAX_UNIT_CHARS = 8_000

_VENDORED_PARTS = (
    "vendor/",
    "third_party/",
    "node_modules/",
    "site-packages/",
    ".venv/",
    "venv/",
    "dist/",
    "build/",
    ".git/",
)

_CALL_KEYWORDS = {
    "if", "for", "while", "return", "print", "len", "range", "int", "str", "list", "dict", "set",
    "tuple", "super", "isinstance", "sorted", "min", "max", "sum", "any", "all", "bool", "float",
    "bytes", "type", "getattr", "setattr", "hasattr", "enumerate", "zip", "map", "filter", "open",
}


# --------------------------------------------------------------------------- #
# Token estimation
# --------------------------------------------------------------------------- #


class TokenCounter:
    """The one token estimate this plugin uses: ``chars / 4``, floor 1."""

    def __init__(self, chars_per_token: int = CHARS_PER_TOKEN):
        if chars_per_token <= 0:
            raise ValueError("chars_per_token must be positive")
        self.chars_per_token = chars_per_token

    def count(self, text: str) -> int:
        if not text:
            return 1
        return max(1, len(text) // self.chars_per_token)


def _count_or_zero(counter: TokenCounter, text: str) -> int:
    return 0 if not text else counter.count(text)


# --------------------------------------------------------------------------- #
# Critical tokens
# --------------------------------------------------------------------------- #

_CRITICAL_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("eval", re.compile(r"\beval\s*\(")),
    ("exec", re.compile(r"\bexec\s*\(")),
    ("os.system", re.compile(r"os\.system\s*\(")),
    ("subprocess", re.compile(r"\bsubprocess\b")),
    ("pickle", re.compile(r"\bpickle\.loads?\b")),
    ("yaml.load", re.compile(r"yaml\.load\s*\(")),
    ("shell=True", re.compile(r"shell\s*=\s*True")),
    ("verify=False", re.compile(r"verify\s*=\s*False")),
    ("sql.execute", re.compile(r"\.execute\s*\(")),
    ("open", re.compile(r"\bopen\s*\(")),
    ("chmod", re.compile(r"\bchmod\b|\b0o?777\b")),
    ("assert", re.compile(r"^\s*assert\b")),
    ("bare except", re.compile(r"^\s*except\s*:")),
    ("password", re.compile(r"\bpassw(?:or)?d\b", re.IGNORECASE)),
    ("secret", re.compile(r"\bsecret\b", re.IGNORECASE)),
    ("token", re.compile(r"\btoken\b", re.IGNORECASE)),
    ("api key", re.compile(r"\bapi[_-]?key\b", re.IGNORECASE)),
    ("auth", re.compile(r"\bauth(?:entication|orization)?\b", re.IGNORECASE)),
    ("mutable default", re.compile(r"def\s+\w+\s*\([^)]*=\s*(?:\[\]|\{\})")),
    ("global", re.compile(r"^\s*global\b")),
    ("lock", re.compile(r"\bLock\s*\(|\bmutex\b|\basyncio\.gather\b")),
    ("thread", re.compile(r"\bthreading\b|\bThread\s*\(")),
    ("datetime.now", re.compile(r"\bdatetime\.(?:now|utcnow)\s*\(")),
    ("random", re.compile(r"\brandom\.(?:random|choice|randint|shuffle)\b")),
    ("float compare", re.compile(r"[=!]=\s*0\.0\b")),
    ("todo", re.compile(r"\b(?:TODO|FIXME|XXX|HACK)\b")),
    ("noqa", re.compile(r"\bnoqa\b|\btype:\s*ignore\b")),
    ("broad except", re.compile(r"^\s*except\s+(?:Exception|BaseException)\b")),
)


def critical_tokens(diff: DiffBundle) -> list[str]:
    """Names on added lines that deserve explicit reviewer attention."""
    found: set[str] = set()
    for patch in diff.files:
        if patch.is_binary:
            continue
        for _, text in _added_lines(patch):
            for label, pattern in _CRITICAL_PATTERNS:
                if label in found:
                    continue
                if pattern.search(text):
                    found.add(label)
    return sorted(found)[:80]


# --------------------------------------------------------------------------- #
# Small git helpers
# --------------------------------------------------------------------------- #


def _show(repo: GitRepo, rev: str, path: str, cache: dict[str, str] | None = None) -> str:
    if not rev or not path:
        return ""
    if cache is not None and path in cache:
        return cache[path]
    try:
        text = repo.show(rev, path)
    except GitError:
        text = ""
    if cache is not None:
        cache[path] = text
    return text


def _is_vendored(path: str) -> bool:
    return any(part in path for part in _VENDORED_PARTS)


def _head_files(repo: GitRepo, head: str, max_files: int) -> list[str]:
    try:
        files = repo.list_files(head or "HEAD")
    except GitError:
        return []
    out: list[str] = []
    for path in files:
        if _is_vendored(path):
            continue
        out.append(path)
        if len(out) >= max(1, max_files):
            break
    return out


def _added_lines(patch: FilePatch) -> list[tuple[int, str]]:
    out: list[tuple[int, str]] = []
    for hunk in patch.hunks:
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


def _unique(values: Iterable[str]) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    for value in values:
        text = str(value or "").strip()
        if text and text not in seen:
            seen.add(text)
            out.append(text)
    return out


# --------------------------------------------------------------------------- #
# 1-hop symbol extraction
# --------------------------------------------------------------------------- #

_DEF_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"^\s*(?:export\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)"),
    re.compile(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:\(|function)"),
    re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z_]\w*)"),
    re.compile(r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)"),
    re.compile(
        r"^\s*(?:export\s+)?(?:abstract\s+|final\s+|public\s+|private\s+|protected\s+|static\s+)*"
        r"(?:class|struct|interface|enum|trait|record|object)\s+([A-Za-z_]\w*)"
    ),
    re.compile(
        r"^\s*(?:public|private|protected|internal|static|final|override|virtual|synchronized|abstract)"
        r"\s+[\w<>\[\],.?*\s]+\s+([A-Za-z_]\w*)\s*\([^;]*\)\s*\{?\s*$"
    ),
    re.compile(r"^\s*(?:def|defp|fun|sub|proc)\s+([A-Za-z_]\w*)"),
)

_AST_GREP_LANG = {
    ".js": "javascript",
    ".jsx": "javascript",
    ".mjs": "javascript",
    ".cjs": "javascript",
    ".ts": "typescript",
    ".tsx": "tsx",
    ".go": "go",
    ".rs": "rust",
    ".java": "java",
    ".c": "c",
    ".h": "c",
    ".cc": "cpp",
    ".cpp": "cpp",
    ".hpp": "cpp",
    ".rb": "ruby",
}


def ast_grep_binary() -> str:
    """Path to the ast-grep CLI, or ``""`` when it is not installed."""
    for name in ("ast-grep", "sg"):
        found = shutil.which(name)
        if found:
            return found
    return ""


def _signature(lines: Sequence[str], node: ast.AST) -> str:
    line = getattr(node, "lineno", 0)
    end = getattr(node, "end_lineno", line) or line
    if not (1 <= line <= len(lines)):
        return ""
    parts: list[str] = []
    depth = 0
    for cursor in range(line - 1, min(int(end), line + 6, len(lines))):
        text = lines[cursor].strip()
        parts.append(text)
        depth += text.count("(") + text.count("[") + text.count("{")
        depth -= text.count(")") + text.count("]") + text.count("}")
        if depth <= 0:
            break
    return " ".join(parts)[:400]


def _kind_of(node: ast.AST) -> str:
    if isinstance(node, ast.ClassDef):
        return "class"
    return "function"


def _node_touched(node: ast.AST, lines: set[int]) -> bool:
    start = int(getattr(node, "lineno", 0) or 0)
    end = int(getattr(node, "end_lineno", start) or start)
    return any(start <= number <= end for number in lines)


def _is_resolvable(name: str) -> bool:
    """A call worth chasing: not a keyword, a local helper or a builtin."""
    return name not in _CALL_KEYWORDS and not hasattr(builtins, name)


def _calls_in(node: ast.AST) -> list[tuple[str, int]]:
    found: list[tuple[str, int]] = []
    for child in ast.walk(node):
        if isinstance(child, ast.Call) and isinstance(child.func, ast.Name):
            if _is_resolvable(child.func.id):
                found.append((child.func.id, int(getattr(child, "lineno", 0) or 0)))
    return found


def _parse_python(repo: GitRepo, head: str, files: Sequence[str], cache: dict[str, str]) -> list[tuple[str, str, ast.Module]]:
    parsed: list[tuple[str, str, ast.Module]] = []
    used = 0
    for path in files:
        if not path.endswith(".py"):
            continue
        text = _show(repo, head, path, cache)
        if not text.strip():
            continue
        try:
            tree = ast.parse(text)
        except (SyntaxError, ValueError):
            continue
        parsed.append((path, text, tree))
        used += len(text)
        if used > SCAN_CHAR_BUDGET:
            break
    return parsed


def _enclosing_python(
    parsed: Sequence[tuple[str, str, ast.Module]], path: str, line: int
) -> tuple[str, int, str] | None:
    for file_path, text, tree in parsed:
        if file_path != path:
            continue
        best: tuple[str, int, str] | None = None
        for node in ast.walk(tree):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                start = int(getattr(node, "lineno", 0) or 0)
                end = int(getattr(node, "end_lineno", start) or start)
                if start <= line <= end:
                    if best is None or start > best[1]:
                        best = (node.name, start, _signature(text.splitlines(), node))
        return best
    return None


def _python_symbols(
    repo: GitRepo, head: str, patches: Sequence[FilePatch], files: Sequence[str], cache: dict[str, str]
) -> list[SymbolView]:
    views: list[SymbolView] = []
    changed_names: set[str] = set()
    callee_requests: dict[str, tuple[str, int]] = {}
    head_paths = set(files)
    top_level = {path.split("/")[0] for path in files if "/" in path}
    top_level |= {path[:-3] for path in files if path.endswith(".py") and "/" not in path}
    for patch in patches:
        text = _show(repo, head, patch.path, cache)
        if not text.strip():
            continue
        try:
            tree = ast.parse(text)
        except (SyntaxError, ValueError):
            continue
        lines = text.splitlines()
        module_defs = {
            node.name
            for node in tree.body
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))
        }
        changed_lines = patch.changed_lines
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                modules = [alias.name for alias in node.names]
            elif isinstance(node, ast.ImportFrom):
                modules = [] if node.level else [node.module or ""]
            else:
                modules = []
            for module in modules:
                if not module or module.split(".")[0] not in top_level:
                    continue  # stdlib or third-party: not this repository's business
                relative = module.replace(".", "/")
                if f"{relative}.py" in head_paths or f"{relative}/__init__.py" in head_paths:
                    continue
                views.append(
                    SymbolView(
                        name=module,
                        kind="import",
                        path=patch.path,
                        line=int(getattr(node, "lineno", 1) or 1),
                        signature=f"import {module} (not present at head)",
                        hop=1,
                        unresolved=True,
                    )
                )
        for node in ast.walk(tree):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                continue
            if not _node_touched(node, changed_lines):
                continue
            views.append(
                SymbolView(
                    name=node.name,
                    kind=_kind_of(node),  # type: ignore[arg-type]
                    path=patch.path,
                    line=int(getattr(node, "lineno", 1) or 1),
                    signature=_signature(lines, node),
                    hop=0,
                    changed=True,
                )
            )
            changed_names.add(node.name)
            for callee, line in _calls_in(node):
                if callee in module_defs or callee in changed_names:
                    continue
                callee_requests.setdefault(callee, (patch.path, line))
    if not changed_names and not callee_requests:
        return views

    parsed = _parse_python(repo, head, files, cache)
    definition_index: dict[str, tuple[str, int, str]] = {}
    for path, text, tree in parsed:
        lines = text.splitlines()
        for node in ast.walk(tree):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                definition_index.setdefault(
                    node.name,
                    (path, int(getattr(node, "lineno", 1) or 1), _signature(lines, node)),
                )

    # callees: definitions one hop away
    for name, (origin, line) in callee_requests.items():
        hit = definition_index.get(name)
        if hit is None:
            views.append(
                SymbolView(
                    name=name,
                    kind="call",
                    path=origin,
                    line=max(1, line),
                    signature=f"{name}() called in {origin}",
                    hop=1,
                    unresolved=True,
                )
            )
            continue
        views.append(
            SymbolView(
                name=name,
                kind="function",
                path=hit[0],
                line=hit[1],
                signature=hit[2],
                hop=1,
                changed=False,
            )
        )

    # callers: functions (anywhere at head) that call a changed name
    wanted = set(changed_names)
    if wanted:
        for path, text, tree in parsed:
            lines = text.splitlines()
            for node in ast.walk(tree):
                if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                    continue
                if node.name in wanted:
                    continue
                hit = next(
                    (
                        (callee, line)
                        for callee, line in _calls_in(node)
                        if callee in wanted and callee != node.name
                    ),
                    None,
                )
                if hit is None:
                    continue
                views.append(
                    SymbolView(
                        name=node.name,
                        kind=_kind_of(node),  # type: ignore[arg-type]
                        path=path,
                        line=int(getattr(node, "lineno", 1) or 1),
                        signature=f"{node.name} calls {hit[0]}(): {_signature(lines, node)}"[:400],
                        hop=1,
                        changed=False,
                    )
                )
    return views


def _relative_to_repo(repo: GitRepo, path: str) -> str:
    """ast-grep may print absolute paths; the reviewer speaks repo-relative."""
    candidate = Path(path)
    if candidate.is_absolute():
        try:
            return candidate.resolve().relative_to(repo.root).as_posix()
        except ValueError:
            return ""
    return candidate.as_posix()


def _ast_grep_calls(repo: GitRepo, name: str, lang: str) -> list[tuple[str, int]] | None:
    """Call sites of ``name`` via ast-grep; ``None`` when the tool is unusable."""
    binary = ast_grep_binary()
    if not binary or not lang:
        return None
    argv = [
        binary,
        "run",
        "--pattern",
        f"{name}($$$ARGS)",
        "--lang",
        lang,
        "--json=stream",
        str(repo.root),
    ]
    run = run_bounded(argv, cwd=repo.root, timeout_s=AST_GREP_TIMEOUT_S)
    if not run.completed:
        return None
    hits: list[tuple[str, int]] = []
    for raw in run.stdout.splitlines():
        raw = raw.strip()
        if not raw.startswith("{"):
            continue
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            continue
        raw_path = payload.get("file")
        start = (payload.get("range") or {}).get("start") or {}
        try:
            line = int(start.get("line") or 0) + 1
        except (TypeError, ValueError):
            line = 0
        path = _relative_to_repo(repo, str(raw_path)) if raw_path else ""
        if path and line:
            hits.append((path, line))
    return hits[:20]


def _regex_calls(
    repo: GitRepo, head: str, name: str, files: Sequence[str], cache: dict[str, str], *, limit: int = 20
) -> list[tuple[str, int]]:
    pattern = re.compile(rf"\b{re.escape(name)}\s*\(")
    hits: list[tuple[str, int]] = []
    for path in files:
        text = _show(repo, head, path, cache)
        if not text:
            continue
        for number, line in enumerate(text.splitlines(), 1):
            if pattern.search(line):
                hits.append((path, number))
                if len(hits) >= limit:
                    return hits
    return hits


_STATEMENT_START = re.compile(r"^\s*(?:return|if|for|while|switch|case|else|await|yield|throw|new)\b")


def _regex_enclosing(
    repo: GitRepo, head: str, path: str, line: int, cache: dict[str, str], *, exclude: str = ""
) -> tuple[str, int] | None:
    """Nearest definition above ``line`` in the file at ``head``.

    Lines that are themselves statements are never definitions, and a match for
    ``exclude`` (the callee) is the call site we started from, not a caller.
    """
    text = _show(repo, head, path, cache)
    if not text:
        return None
    lines = text.splitlines()
    for cursor in range(min(line, len(lines)) - 1, -1, -1):
        candidate = lines[cursor]
        if _STATEMENT_START.match(candidate):
            continue
        for pattern in _DEF_PATTERNS:
            match = pattern.match(candidate)
            if match:
                if exclude and match.group(1) == exclude:
                    continue
                return match.group(1), cursor + 1
    return None


def _nonpython_symbols(
    repo: GitRepo, head: str, patches: Sequence[FilePatch], files: Sequence[str], cache: dict[str, str]
) -> list[SymbolView]:
    views: list[SymbolView] = []
    for patch in patches:
        for number, text in _added_lines(patch):
            for pattern in _DEF_PATTERNS:
                match = pattern.match(text)
                if match:
                    views.append(
                        SymbolView(
                            name=match.group(1),
                            kind="function",
                            path=patch.path,
                            line=max(1, number),
                            signature=text.strip()[:400],
                            hop=0,
                            changed=True,
                        )
                    )
                    break
    head_set = set(files)
    for view in views[:10]:
        lang = _AST_GREP_LANG.get(Path(view.path).suffix.lower(), "")
        hits = _ast_grep_calls(repo, view.name, lang) if lang else None
        if hits is not None:
            # ast-grep reads the checkout, not a commit: keep only hits that the
            # head commit actually contains, with the call on the reported line.
            verified: list[tuple[str, int]] = []
            for path, line in hits:
                if path not in head_set:
                    continue
                lines = _show(repo, head, path, cache).splitlines()
                if 1 <= line <= len(lines) and re.search(
                    rf"\b{re.escape(view.name)}\s*\(", lines[line - 1]
                ):
                    verified.append((path, line))
            hits = verified if verified or not hits else None
        if hits is None:
            same_ext = [p for p in files if Path(p).suffix.lower() == Path(view.path).suffix.lower()]
            hits = _regex_calls(repo, head, view.name, same_ext, cache)
        for path, line in hits:
            if path == view.path and line == view.line:
                continue
            enclosing = _regex_enclosing(repo, head, path, line, cache, exclude=view.name)
            if enclosing is None or enclosing[0] == view.name:
                continue
            views.append(
                SymbolView(
                    name=enclosing[0],
                    kind="function",
                    path=path,
                    line=max(1, enclosing[1]),
                    signature=f"{enclosing[0]} calls {view.name}() at line {line}",
                    hop=1,
                    changed=False,
                )
            )
    return views


def extract_symbols(
    repo: GitRepo, diff: DiffBundle, *, head: str, max_files: int = 2000
) -> list[SymbolView]:
    """Changed definitions plus their 1-hop callers/callees.

    Python is resolved with :mod:`ast` at ``head``; every other language goes
    through the ``ast-grep`` CLI when it is installed and a bounded regex walk
    otherwise.  A callee that cannot be found at head is emitted with
    ``unresolved=True`` rather than dropped silently.
    """
    head = head or diff.head_sha
    files = _head_files(repo, head, max_files)
    cache: dict[str, str] = {}
    python_patches = [
        patch for patch in diff.files if patch.path.endswith(".py") and not patch.is_binary and not patch.is_deleted
    ]
    other_patches = [
        patch for patch in diff.files if not patch.path.endswith(".py") and not patch.is_binary and not patch.is_deleted
    ]
    views = _python_symbols(repo, head, python_patches, files, cache)
    views += _nonpython_symbols(repo, head, other_patches, files, cache)
    out: list[SymbolView] = []
    seen: set[tuple[str, str, int]] = set()
    for view in sorted(views, key=lambda v: (v.hop, v.path, v.line, v.name)):
        key = (view.name, view.path, view.line)
        if key in seen:
            continue
        seen.add(key)
        out.append(view)
        if len(out) >= MAX_SYMBOLS:
            break
    return out


def _resolve_named(repo: GitRepo, head: str, names: Sequence[str], max_files: int) -> list[SymbolView]:
    """Resolve triage-requested symbols to their definitions at head."""
    wanted = {name for name in names if name}
    if not wanted:
        return []
    files = _head_files(repo, head, max_files)
    cache: dict[str, str] = {}
    found: list[SymbolView] = []
    for path, text, tree in _parse_python(repo, head, files, cache):
        lines = text.splitlines()
        for node in ast.walk(tree):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                continue
            if node.name in wanted:
                found.append(
                    SymbolView(
                        name=node.name,
                        kind=_kind_of(node),  # type: ignore[arg-type]
                        path=path,
                        line=int(getattr(node, "lineno", 1) or 1),
                        signature=_signature(lines, node),
                        hop=1,
                        changed=False,
                    )
                )
                wanted.discard(node.name)
        if not wanted:
            break
    if wanted:
        for path in files:
            text = _show(repo, head, path, cache)
            for number, line in enumerate(text.splitlines(), 1):
                for pattern in _DEF_PATTERNS:
                    match = pattern.match(line)
                    if match and match.group(1) in wanted:
                        found.append(
                            SymbolView(
                                name=match.group(1),
                                kind="function",
                                path=path,
                                line=max(1, number),
                                signature=line.strip()[:400],
                                hop=1,
                                changed=False,
                            )
                        )
                        wanted.discard(match.group(1))
                        break
                if not wanted:
                    break
            if not wanted:
                break
    return found


# --------------------------------------------------------------------------- #
# Conventions and design precedents (head commit only)
# --------------------------------------------------------------------------- #

_BULLET = re.compile(r"^\s*(?:[-*+]|\d+[.)])\s+(.+?)\s*$")
_NEVER = re.compile(r"\b(?:never|must\s+not|do\s+not|don't|avoid|forbidden|prohibited)\b", re.IGNORECASE)
_MUST = re.compile(r"\b(?:must|always|required|shall|mandatory)\b", re.IGNORECASE)
_PREFER = re.compile(r"\b(?:prefer|should|recommended?)\b", re.IGNORECASE)
_ALLOW = re.compile(r"\b(?:may|allowed|ok(?:ay)?\s+to|permitted)\b", re.IGNORECASE)
_BACKTICK = re.compile(r"`([^`]+)`")

MAX_DIRECTIVES = 200
MAX_PRECEDENTS = 200


def _directive_kind(text: str) -> str:
    if _NEVER.search(text):
        return "never"
    if _MUST.search(text):
        return "must"
    if _PREFER.search(text):
        return "prefer"
    if _ALLOW.search(text):
        return "allow"
    return "must"


def _markdown_directives(source: str, text: str) -> list[ConventionDirective]:
    out: list[ConventionDirective] = []
    for number, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        match = _BULLET.match(line)
        body = match.group(1) if match else ""
        if not body:
            stripped = line.strip()
            body = stripped if stripped and (_NEVER.search(stripped) or _MUST.search(stripped)) else ""
        if not body or len(body) < 8:
            continue
        out.append(
            ConventionDirective(
                source=source,
                line=number,
                text=body[:600],
                kind=_directive_kind(body),  # type: ignore[arg-type]
                applies_to=_unique(_BACKTICK.findall(body))[:10],
            )
        )
        if len(out) >= MAX_DIRECTIVES:
            break
    return out


def _as_list(value: Any, *, limit: int = 40) -> list[str]:
    if value is None:
        return []
    if isinstance(value, str):
        return [line.strip()[:600] for line in value.splitlines() if line.strip()][:limit]
    if isinstance(value, (list, tuple, set)):
        out: list[str] = []
        for item in list(value)[:limit]:
            text = str(item).strip()
            if text:
                out.append(text[:600])
        return out
    return [str(value)[:600]]


def _pr_agent_directives(source: str, text: str) -> list[ConventionDirective]:
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return []
    blocks: list[tuple[str, Any]] = []
    for section, body in data.items():
        if isinstance(body, dict):
            blocks.append((str(section), body.get("extra_instructions")))
        else:
            blocks.append((str(section), body))
    out: list[ConventionDirective] = []
    for section, value in blocks:
        for item in _as_list(value):
            for offset, line in enumerate(item.splitlines() or [item]):
                body = line.strip().lstrip("-*+").strip()
                if not body or len(body) < 4:
                    continue
                out.append(
                    ConventionDirective(
                        source=source,
                        line=offset + 1,
                        text=body[:600],
                        kind=_directive_kind(body),  # type: ignore[arg-type]
                        applies_to=_unique(_BACKTICK.findall(body))[:10],
                    )
                )
                if len(out) >= MAX_DIRECTIVES:
                    return out
    return out


def _designlore_precedents(source: str, text: str) -> list[DesignPrecedent]:
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return []
    entries: Any = data
    if isinstance(data, dict):
        for key in ("precedents", "designs", "entries", "features"):
            if isinstance(data.get(key), list):
                entries = data[key]
                break
        else:
            entries = [data]
    if not isinstance(entries, list):
        return []
    out: list[DesignPrecedent] = []
    for index, item in enumerate(entries[:MAX_PRECEDENTS]):
        if not isinstance(item, dict):
            continue
        identifier = str(item.get("id") or item.get("name") or f"{source}:{index + 1}")[:200]
        out.append(
            DesignPrecedent(
                id=identifier,
                problem=str(item.get("problem") or item.get("context") or "")[:2000],
                trigger=_as_list(item.get("trigger") or item.get("triggers")),
                options=_as_list(item.get("options")),
                decision=str(item.get("decision") or item.get("chosen") or "")[:2000],
                rationale=str(item.get("rationale") or item.get("reason") or "")[:2000],
                applies_to=_as_list(item.get("applies_to") or item.get("appliesTo")),
                preconditions=_as_list(item.get("preconditions")),
                source=source,
            )
        )
    return out


def load_conventions(
    repo: GitRepo, head: str, settings: Settings
) -> tuple[list[ConventionDirective], list[DesignPrecedent], list[str]]:
    """Read conventions and design precedents from the head commit.

    Returns ``(directives, precedents, loaded)`` where ``loaded`` names the
    files that existed at ``head`` and were parsed.  A file that is present only
    in the working tree is invisible here on purpose: the reviewer judges the
    commit, not the developer's checked-out state.
    """
    directives: list[ConventionDirective] = []
    precedents: list[DesignPrecedent] = []
    loaded: list[str] = []
    for name in settings.designlore_files:
        try:
            text = repo.show(head, name)
        except GitError:
            continue
        loaded.append(name)
        suffix = Path(name).suffix.lower()
        if suffix == ".json":
            precedents.extend(_designlore_precedents(name, text))
        elif suffix == ".toml":
            directives.extend(_pr_agent_directives(name, text))
        else:
            directives.extend(_markdown_directives(name, text))
    return (
        directives[:MAX_DIRECTIVES],
        precedents[:MAX_PRECEDENTS],
        _unique(loaded),
    )


# --------------------------------------------------------------------------- #
# Context assembly
# --------------------------------------------------------------------------- #

#: Unit priority, highest first.  The diff always outranks everything else.
PRIORITY_FOCUS_DIFF = 100
PRIORITY_DIFF = 92
PRIORITY_DELETED_DIFF = 88
PRIORITY_CRITICAL = 86
PRIORITY_CONVENTION = 78
PRIORITY_PLANNED_SYMBOL = 72
PRIORITY_PRECEDENT = 70
PRIORITY_CHANGED_SYMBOL = 66
PRIORITY_HOP_SYMBOL = 58
PRIORITY_UNRESOLVED = 44
PRIORITY_FILE_HEAD = 30


def _cap(text: str, limit: int) -> tuple[str, bool]:
    if limit <= 0 or len(text) <= limit:
        return text, False
    return text[: max(0, limit - 32)] + "\n...[truncated by the reviewer]", True


def _render(unit: ContextUnit) -> str:
    return f"[{unit.kind}:{unit.source}]\n{unit.text}"


def _join(units: Sequence[ContextUnit]) -> str:
    return "\n\n".join(_render(unit) for unit in units)


def build_context(
    repo: GitRepo,
    diff: DiffBundle,
    settings: Settings,
    *,
    plan: TriagePlan | None = None,
) -> ContextSlice:
    """Assemble the compressed evidence block within the model token budget.

    Units are sorted by priority and dropped whole until
    ``tokens <= settings.max_model_tokens``; ``truncated`` is set when any unit
    was dropped, any text was cut, or the diff itself arrived truncated.

    There is deliberately no parameter for a PR description: this module never
    sees one and never reads commit messages, so author prose cannot reach the
    evidence block.
    """
    counter = TokenCounter()
    budget = max(1, int(settings.max_model_tokens))
    units: list[ContextUnit] = []
    truncated = bool(diff.truncated)
    material_truncated = bool(diff.truncated)
    dropped = 0
    dropped_diff = 0
    focus = {item.path for item in (plan.focus if plan is not None else ())}

    # -- the changed code: one unit per file, the highest priority ---------
    diff_cap = min(MAX_UNIT_CHARS * 15, max(1024, budget * 4))
    for patch in diff.files:
        body = patch.patch_text()
        if not body.strip():
            continue
        text, cut = _cap(body, diff_cap)
        truncated = truncated or cut
        material_truncated = material_truncated or cut
        priority = (
            PRIORITY_FOCUS_DIFF if patch.path in focus else PRIORITY_DIFF
        )
        if patch.is_deleted:
            priority = min(priority, PRIORITY_DELETED_DIFF)
        units.append(
            ContextUnit(
                kind="diff",
                source=patch.path,
                text=text,
                tokens=counter.count(text),
                priority=priority,
                reason="changed file",
            )
        )

    # -- identifiers a reviewer must not skim over -------------------------
    tokens = critical_tokens(diff)
    if tokens:
        text, cut = _cap("CRITICAL TOKENS ON CHANGED LINES: " + ", ".join(tokens), 2000)
        truncated = truncated or cut
        units.append(
            ContextUnit(
                kind="critical_token",
                source="diff",
                text=text,
                tokens=counter.count(text),
                priority=PRIORITY_CRITICAL,
                reason="security/behaviour-sensitive identifiers on added lines",
            )
        )

    # -- conventions and precedents, read from the head commit ------------
    directives, precedents, loaded = load_conventions(repo, diff.head_sha, settings)
    for directive in directives:
        text, cut = _cap(f"{directive.kind.upper()}: {directive.text}", 800)
        truncated = truncated or cut
        units.append(
            ContextUnit(
                kind="convention",
                source=f"{directive.source}:{directive.line}",
                text=text,
                tokens=counter.count(text),
                priority=PRIORITY_CONVENTION,
                reason="project convention at head",
            )
        )
    for precedent in precedents:
        body = f"{precedent.id}: {precedent.problem} -> {precedent.decision}"
        if precedent.rationale:
            body += f" ({precedent.rationale})"
        text, cut = _cap(body, 1200)
        truncated = truncated or cut
        units.append(
            ContextUnit(
                kind="precedent",
                source=precedent.source,
                text=text,
                tokens=counter.count(text),
                priority=PRIORITY_PRECEDENT,
                reason="design precedent at head",
            )
        )

    # -- 1-hop symbols -----------------------------------------------------
    symbols = extract_symbols(repo, diff, head=diff.head_sha)
    planned: list[SymbolView] = []
    if plan is not None and plan.search_symbols:
        have = {view.name for view in symbols}
        planned = [
            view
            for view in _resolve_named(repo, diff.head_sha, plan.search_symbols, 2000)
            if view.name not in have
        ]
    for view in planned:
        text, cut = _cap(f"{view.path}:{view.line}: {view.signature}".strip(), 1200)
        truncated = truncated or cut
        units.append(
            ContextUnit(
                kind="type" if view.kind == "class" else "signature",
                source=view.path,
                text=text,
                tokens=counter.count(text),
                priority=PRIORITY_PLANNED_SYMBOL,
                reason=f"requested by triage: {view.name}",
            )
        )
    for view in symbols:
        body = f"{view.path}:{view.line}: {view.signature}".strip()
        if view.unresolved:
            if view.kind == "import":
                body = (
                    f"{view.path}:{view.line}: project import {view.name} does not exist at head"
                )
                reason = "unresolved project import"
            else:
                body = (
                    f"{view.path}:{view.line}: {view.name}() is called but not defined in this repository"
                )
                reason = "unresolved 1-hop callee"
            priority = PRIORITY_UNRESOLVED
        elif view.changed:
            priority = PRIORITY_CHANGED_SYMBOL
            reason = f"{view.kind} {view.name} (hop {view.hop})"
        else:
            priority = PRIORITY_HOP_SYMBOL
            reason = f"{view.kind} {view.name} (hop {view.hop})"
        text, cut = _cap(body, 1200)
        truncated = truncated or cut
        units.append(
            ContextUnit(
                kind="type" if view.kind == "class" else "signature",
                source=view.path,
                text=text,
                tokens=counter.count(text),
                priority=priority,
                reason=reason,
            )
        )

    # -- project entry points (README/AGENTS/pyproject) -------------------
    for name in settings.important_files:
        text = _show(repo, diff.head_sha, name)
        if not text.strip():
            continue
        head_excerpt, cut = _cap("\n".join(text.splitlines()[:40]), 1500)
        truncated = truncated or cut
        units.append(
            ContextUnit(
                kind="file_head",
                source=name,
                text=head_excerpt,
                tokens=counter.count(head_excerpt),
                priority=PRIORITY_FILE_HEAD,
                reason="project entry point at head",
            )
        )

    # -- selection: priority order, whole units, hard budget --------------
    ordered = sorted(range(len(units)), key=lambda index: (-units[index].priority, index))
    kept: list[ContextUnit] = []
    used = 0
    for index in ordered:
        unit = units[index]
        cost = counter.count(_render(unit)) + 1  # +1 for the join separator
        if used + cost <= budget:
            kept.append(unit)
            used += cost
        else:
            dropped += 1
            if unit.kind == "diff":
                dropped_diff += 1

    text = _join(kept)
    total = _count_or_zero(counter, text)
    while total > budget and len(kept) > 1:
        kept.pop()
        dropped += 1
        text = _join(kept)
        total = _count_or_zero(counter, text)

    if total > budget and kept:
        # A single unit is larger than the whole budget: cut the raw text so the
        # ceiling is never violated, and say so.
        limit = max(64, budget * CHARS_PER_TOKEN)
        text = text[:limit]
        total = _count_or_zero(counter, text)
        truncated = True
        material_truncated = True

    truncated = truncated or dropped > 0
    material_truncated = material_truncated or dropped_diff > 0
    unresolved = sorted({view.name for view in [*symbols, *planned] if view.unresolved})
    return ContextSlice(
        text=text,
        tokens=total,
        budget=budget,
        units=kept,
        dropped_units=dropped,
        unresolved_symbols=unresolved,
        conventions_loaded=loaded,
        precedents_loaded=sorted({precedent.source for precedent in precedents}),
        input_sha256=sha256_hex(text),
        input_bytes=len(text.encode("utf-8")),
        truncated=truncated,
        material_truncated=material_truncated,
    )
