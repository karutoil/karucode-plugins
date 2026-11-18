"""Stage 4C — DesignLore: project precedents and directives as a suppression filter.

Two sources are read **at the head commit** (never the working tree):

* ``.zero_loop/designlore.json`` — design precedents (``DesignPrecedent``);
* ``AGENTS.md`` / ``.cursorrules`` — convention directives whose kind is
  ``never``, ``must``, ``prefer`` or ``allow``.  A directive names its code
  construct and its path globs in backticks, e.g.::

      - never: use `eval` in `src/**`
      - must: keep `logging` calls in `src/**`

``filter_candidate`` suppresses a suggestion when

* it *adds* a construct a matching ``never`` directive forbids, or *removes* a
  construct a matching ``must`` directive requires, or
* a precedent matches by path glob **and** trigger, its preconditions hold
  (every referenced path or symbol exists under the repo root), and the
  suggestion contradicts the recorded decision.

Reasons are for the receipt, never for the public comment.  The filter is
fail-open on unreadable input: malformed lore never suppresses a finding, it
just stops constraining it.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Iterable, Sequence

from ..config import Settings
from ..errors import ReviewError
from ..gitops import GitRepo
from ..models import Candidate, ConventionDirective, DesignPrecedent, Fix

DESIGN_LORE_FILE = ".zero_loop/designlore.json"
DIRECTIVE_FILES = ("AGENTS.md", ".cursorrules")

MAX_PRECEDENTS = 200
MAX_DIRECTIVES = 200
MAX_SCANNED_FILES = 4000
MAX_SCANNED_BYTES = 2_000_000

_BULLET = re.compile(r"^\s*(?:[-*+]|\d+[.)])\s*")
_KIND_PREFIX = re.compile(r"^(never|must|prefer|allow)\b\s*[:\-\u2014]?\s*", re.IGNORECASE)
_NEGATIVE_WORD = re.compile(r"\b(never|not|avoid|forbid(?:den)?|reject(?:ed)?|instead\s+of|deprecat\w*|do\s+not|don't|must\s+not)\b", re.IGNORECASE)
_MUST_WORD = re.compile(r"\b(always|must|required?|mandatory)\b", re.IGNORECASE)
_PREFER_WORD = re.compile(r"\b(prefer|should|recommended?)\b", re.IGNORECASE)
_ALLOW_WORD = re.compile(r"\b(allow(?:ed)?|may|permitted)\b", re.IGNORECASE)
_BACKTICK = re.compile(r"`([^`\n]+)`")
_IDENT = re.compile(r"\b[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*\b")
_PATH_SUFFIXES = (".py", ".md", ".rst", ".txt", ".json", ".toml", ".yml", ".yaml", ".cfg", ".ini", ".js", ".ts", ".c", ".h", ".cpp")
_SKIP_DIRS = frozenset({".git", "__pycache__", "node_modules", ".mypy_cache", ".pytest_cache", ".ruff_cache"})

#: English words that are never a "construct" in a directive or a decision.
_ENGLISH = frozenset(
    """a an the and or but if then else when where while for with without into from at by to of on in
    is are be been being it its this that these those any all every each other another new old same
    only also not never must prefer allow avoid use uses using used used add adds adding remove
    removes removing keep keeps keeping call calls called return returns read reads write writes
    code file files path paths function functions method methods module modules class classes
    should always required mandatory recommended allowed permitted instead before after over under
    do does did done make makes made get gets set sets value values type types check checks
    implemented implement implements there here their them they you your our we us who what which
    can cannot may might will would shall ignore ignores""".split()
)


# --------------------------------------------------------------------------- #
# Loaders
# --------------------------------------------------------------------------- #


def load_precedents(repo: GitRepo, head: str, settings: Settings) -> list[DesignPrecedent]:
    """Read design precedents from the ``.json`` lore files at ``head``."""
    out: list[DesignPrecedent] = []
    for name in _json_sources(settings):
        text = _show(repo, head, name)
        if text is None:
            continue
        out.extend(_parse_precedents(name, text))
    return out[:MAX_PRECEDENTS]


def load_directives(repo: GitRepo, head: str, settings: Settings) -> list[ConventionDirective]:
    """Read never/must/prefer directives from the lore files at ``head``."""
    out: list[ConventionDirective] = []
    for name in _directive_sources(settings):
        text = _show(repo, head, name)
        if text is None:
            continue
        out.extend(_parse_directives(name, text))
    return out[:MAX_DIRECTIVES]


def _json_sources(settings: Settings) -> list[str]:
    names = [name for name in getattr(settings, "designlore_files", ()) if str(name).lower().endswith(".json")]
    if DESIGN_LORE_FILE not in names:
        names.insert(0, DESIGN_LORE_FILE)
    return _dedupe(names)


def _directive_sources(settings: Settings) -> list[str]:
    names = [
        name
        for name in getattr(settings, "designlore_files", ())
        if not str(name).lower().endswith((".json", ".toml"))
    ]
    names.extend(DIRECTIVE_FILES)
    return _dedupe(names)


def _show(repo: GitRepo, head: str, name: str) -> str | None:
    try:
        return repo.show(head, name)
    except (ReviewError, OSError):
        return None


def _parse_precedents(source: str, text: str) -> list[DesignPrecedent]:
    try:
        data = json.loads(text)
    except (ValueError, TypeError):
        return []
    entries: object = data
    if isinstance(data, dict):
        for key in ("precedents", "designs", "entries", "features"):
            value = data.get(key)
            if isinstance(value, list):
                entries = value
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
        try:
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
        except ValueError:
            continue
    return out


def _parse_directives(source: str, text: str) -> list[ConventionDirective]:
    out: list[ConventionDirective] = []
    in_fence = False
    for number, raw in enumerate(text.splitlines(), start=1):
        if raw.strip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        line = raw.strip()
        if not line or line.startswith(("#", "<!--", "|", ">")):
            continue
        body = _BULLET.sub("", line).strip()
        if len(body) < 5:
            continue
        kind = _directive_kind(body)
        if kind is None:
            continue
        out.append(
            ConventionDirective(
                source=source,
                line=number,
                text=body[:600],
                kind=kind,
                applies_to=_globs(body)[:10],
            )
        )
        if len(out) >= MAX_DIRECTIVES:
            break
    return out


def _directive_kind(text: str) -> str | None:
    match = _KIND_PREFIX.match(text)
    if match:
        return match.group(1).lower()
    if _NEGATIVE_WORD.search(text):
        return "never"
    if _MUST_WORD.search(text):
        return "must"
    if _PREFER_WORD.search(text):
        return "prefer"
    if _ALLOW_WORD.search(text):
        return "allow"
    return None


def _globs(text: str) -> list[str]:
    out: list[str] = []
    for token in _BACKTICK.findall(text):
        cleaned = token.strip()
        if _looks_like_path(cleaned) and cleaned not in out:
            out.append(cleaned)
    return out


# --------------------------------------------------------------------------- #
# Filter
# --------------------------------------------------------------------------- #


def filter_candidate(
    candidate: Candidate,
    precedents: Sequence[DesignPrecedent],
    directives: Sequence[ConventionDirective],
    *,
    repo_root: Path,
) -> tuple[bool, str]:
    """Return ``(keep, reason)``; a suppressed suggestion carries why it was dropped."""
    repo_root = Path(repo_root)
    fix = _fix(candidate)
    patch = fix.patch if fix is not None else ""
    added = [line[1:] for line in patch.splitlines() if line.startswith("+") and not line.startswith("+++")]
    removed = [line[1:] for line in patch.splitlines() if line.startswith("-") and not line.startswith("---")]
    added_text = "\n".join(added)
    removed_text = "\n".join(removed)
    corpus = "\n".join(
        part for part in (candidate.title, candidate.root_cause, patch, candidate.private_detail) if part
    )
    paths = _candidate_paths(candidate, fix)

    for directive in directives:
        if directive.kind not in ("never", "must"):
            continue
        if directive.applies_to and not _matches(directive.applies_to, paths):
            continue
        constructs = _constructs(directive.text)
        if not constructs:
            continue
        if directive.kind == "never" and added_text:
            hit = _hit(constructs, added_text)
            if hit:
                return (
                    False,
                    f"designlore: the suggestion adds `{hit}`, which the `never` directive at "
                    f"{directive.source}:{directive.line} forbids for {_where(directive.applies_to)}",
                )
        if directive.kind == "must" and removed_text:
            hit = _hit(constructs, removed_text)
            if hit:
                return (
                    False,
                    f"designlore: the suggestion removes `{hit}`, which the `must` directive at "
                    f"{directive.source}:{directive.line} requires in {_where(directive.applies_to)}",
                )

    notes: list[str] = []
    for precedent in precedents:
        matched, how = _precedent_match(precedent, paths, corpus)
        if not matched:
            continue
        holds, why = _preconditions_hold(precedent, repo_root)
        if not holds:
            notes.append(
                f"precedent `{precedent.id}` matched by {how} but its preconditions do not hold ({why})"
            )
            continue
        violated, reason = _contradicts(precedent, added_text, removed_text)
        if violated:
            return (
                False,
                f"designlore: precedent `{precedent.id}` ({precedent.source or 'designlore'}) matched by "
                f"{how}; {reason}",
            )

    if notes:
        return True, "designlore kept the candidate: " + "; ".join(notes)[:600]
    return True, "designlore: no matched directive or precedent constrains this suggestion"


# --------------------------------------------------------------------------- #
# Matching helpers
# --------------------------------------------------------------------------- #


def _fix(candidate: Candidate) -> Fix | None:
    if candidate.verified_fix is not None:
        return candidate.verified_fix
    if candidate.verification.civer is not None:
        return candidate.verification.civer.fix
    return None


def _candidate_paths(candidate: Candidate, fix: Fix | None) -> list[str]:
    paths = [site.path for site in candidate.cited_sites]
    if fix is not None:
        paths.extend(fix.files_changed)
        paths.extend(_patch_files(fix.patch))
    return _dedupe([path for path in paths if path])


def _patch_files(patch: str) -> list[str]:
    out: list[str] = []
    for line in patch.splitlines():
        if not line.startswith(("--- ", "+++ ")):
            continue
        token = line[4:].strip().split("\t")[0]
        if token.startswith(("a/", "b/")):
            token = token[2:]
        if token and token != "/dev/null" and token not in out:
            out.append(token)
    return out


def _matches(patterns: Sequence[str], paths: Sequence[str]) -> bool:
    for pattern in patterns:
        for path in paths:
            try:
                if _glob_match(pattern, path):
                    return True
            except (ValueError, re.error):
                continue
    return False


def _glob_match(pattern: str, path: str) -> bool:
    pattern = str(pattern).strip().lstrip("./")
    path = str(path).strip().lstrip("./")
    if not pattern:
        return False
    regex = _glob_regex(pattern)
    return re.fullmatch(regex, path) is not None


def _glob_regex(pattern: str) -> str:
    out = ""
    index = 0
    while index < len(pattern):
        if pattern.startswith("**/", index):
            if out.endswith("/"):
                out = out[:-1]
            out += "(?:.*/)?"
            index += 3
            continue
        char = pattern[index]
        if char == "*":
            if pattern.startswith("**", index):
                out += ".*"
                index += 2
            else:
                out += "[^/]*"
                index += 1
            continue
        if char == "?":
            out += "[^/]"
            index += 1
            continue
        out += re.escape(char)
        index += 1
    return out


def _looks_like_path(token: str) -> bool:
    text = token.strip()
    if not text or " " in text:
        return False
    if "*" in text or "/" in text:
        return True
    return text.lower().endswith(_PATH_SUFFIXES)


def _precedent_match(precedent: DesignPrecedent, paths: Sequence[str], corpus: str) -> tuple[bool, str]:
    by_path = bool(precedent.applies_to) and _matches(precedent.applies_to, paths)
    by_trigger = bool(precedent.trigger) and any(_trigger_hit(trigger, corpus) for trigger in precedent.trigger)
    if precedent.applies_to and not by_path:
        return False, ""
    if precedent.trigger and not by_trigger:
        return False, ""
    if not precedent.applies_to and not precedent.trigger:
        return False, ""
    reasons: list[str] = []
    if by_path:
        reasons.append("path glob")
    if by_trigger:
        reasons.append("trigger")
    return True, " and ".join(reasons)


def _trigger_hit(trigger: str, corpus: str) -> bool:
    phrase = trigger.strip().lower()
    if not phrase:
        return False
    haystack = corpus.lower()
    if phrase in haystack:
        return True
    words = [word for word in _IDENT.findall(phrase) if word not in _ENGLISH and len(word) > 2]
    if not words:
        return False
    tokens = set(_IDENT.findall(haystack))
    return all(word in tokens for word in words)


def _preconditions_hold(precedent: DesignPrecedent, repo_root: Path) -> tuple[bool, str]:
    for precondition in precedent.preconditions:
        token = precondition.strip().strip("`").strip()
        if not token:
            continue
        if _looks_like_path(token):
            if not _path_exists(repo_root, token):
                return False, f"{token} does not exist"
        elif not _symbol_exists(repo_root, token):
            return False, f"symbol {token} was not found"
    return True, ""


def _path_exists(repo_root: Path, pattern: str) -> bool:
    if pattern.startswith(("/", "~")):
        return False
    if any(char in pattern for char in "*?["):
        try:
            for _ in repo_root.glob(pattern):
                return True
        except (ValueError, NotImplementedError, OSError):
            return False
        return False
    return (repo_root / pattern).exists()


def _symbol_exists(repo_root: Path, symbol: str) -> bool:
    matches = _IDENT.findall(symbol)
    if not matches:
        return False
    needle = matches[-1].split(".")[-1]
    if not needle:
        return False
    word = re.compile(rf"\b{re.escape(needle)}\b")
    scanned = 0
    for path in sorted(repo_root.rglob("*")):
        if scanned >= MAX_SCANNED_FILES:
            break
        if not path.is_file() or any(part in _SKIP_DIRS for part in path.relative_to(repo_root).parts):
            continue
        try:
            if path.stat().st_size > MAX_SCANNED_BYTES:
                continue
        except OSError:
            continue
        scanned += 1
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        if word.search(text):
            return True
    return False


def _contradicts(precedent: DesignPrecedent, added_text: str, removed_text: str) -> tuple[bool, str]:
    chosen = set(_code_tokens(precedent.decision))
    rejected: set[str] = set()
    for text in [precedent.rationale, *precedent.options]:
        for clause in re.split(r"[;.,\n]", text or ""):
            if not _NEGATIVE_WORD.search(clause):
                continue
            rejected.update(_code_tokens(clause))
    rejected -= chosen
    if rejected and added_text:
        hit = _hit(sorted(rejected), added_text)
        if hit:
            return True, f"the suggestion re-introduces `{hit}`, which the recorded decision rejects"
    if chosen and removed_text:
        hit = _hit(sorted(chosen), removed_text)
        if hit:
            return True, f"the suggestion removes `{hit}`, which the recorded decision requires"
    return False, ""


def _constructs(text: str) -> list[str]:
    backticked = [token.strip() for token in _BACKTICK.findall(text)]
    code = [token for token in backticked if token and not _looks_like_path(token)]
    if code:
        return _dedupe(code)
    stripped = _BACKTICK.sub(" ", text)
    return _dedupe(
        [
            word
            for word in _IDENT.findall(stripped)
            if word.lower() not in _ENGLISH and len(word) > 2 and word.lower() not in ("never", "must", "prefer", "allow")
        ]
    )


def _code_tokens(text: str) -> list[str]:
    backticked = [token.strip() for token in _BACKTICK.findall(text or "")]
    code = [token for token in backticked if token and not _looks_like_path(token)]
    if code:
        return _dedupe(code)
    stripped = _BACKTICK.sub(" ", text or "")
    return _dedupe(
        [
            word
            for word in _IDENT.findall(stripped)
            if word.lower() not in _ENGLISH and len(word) > 2
        ]
    )


def _hit(needles: Iterable[str], haystack: str) -> str:
    for needle in needles:
        text = str(needle).strip()
        if not text:
            continue
        if re.search(rf"(?<![A-Za-z0-9_]){re.escape(text)}(?![A-Za-z0-9_])", haystack):
            return text
    return ""


def _where(patterns: Sequence[str]) -> str:
    return ", ".join(f"`{pattern}`" for pattern in patterns) if patterns else "the whole repository"


def _as_list(value: object, *, limit: int = 40) -> list[str]:
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


def _dedupe(values: Sequence[str]) -> list[str]:
    out: list[str] = []
    for value in values:
        text = str(value).strip()
        if text and text not in out:
            out.append(text)
    return out
