"""Stage 1C — PR message/code inconsistency (PR-MCI) with author-text isolation.

The pull request description is the only attacker-controlled prose the reviewer
ever reads.  It reaches exactly one prompt (``pmi_prompt``, stage ``"pmi"``,
``carries_author_text=True``) and only to *extract claims*.  Judging those
claims is host code: :func:`ast_facts` reads the changed files at the head
commit, and :func:`_ground_claim` decides whether the code can support what the
description says.

Grounding rule (see ``INTERFACES.md``): a claim is ``supported`` only when

* every symbol it names exists in ``facts``, and
* at least one of them is a changed/added definition or an added call, and
* the claim does not assert behaviour the diff cannot produce.

A claim that asserts a removal of something still defined at head (or an
addition of something that already existed) is a contradiction: the description
and the code disagree, which is itself the finding.

Nothing here copies the description into a result beyond one short quote.
"""

from __future__ import annotations

import ast
import re
from typing import Any, Iterable, Sequence

from ..config import Settings
from ..errors import FailureKind, LLMError
from ..gitops import GitError, GitRepo
from ..llm.prompts import pmi_prompt
from ..models import DiffBundle, FilePatch, MciClaim, MciResult, MciScan

# --------------------------------------------------------------------------- #
# Author-text isolation
# --------------------------------------------------------------------------- #

#: Hard cap on the description text that is allowed to reach a prompt.
MAX_AUTHOR_CHARS = 20_000

_ZERO_WIDTH = re.compile(r"[\u200b-\u200f\u2028-\u202e\u2060-\u2064\ufeff]")
_HTML_COMMENT = re.compile(r"<!--.*?-->", re.DOTALL)
_EVIDENCE_TAG = re.compile(r"</?\s*EVIDENCE\b[^>]*>", re.IGNORECASE)
_FENCE = re.compile(r"^\s*```")


def strip_author_text(text: str) -> str:
    """Return a bounded, delimiter-safe copy of author-written text.

    Removes the sequences that let a description forge the evidence block the
    prompts use (``<EVIDENCE>``/``</EVIDENCE>``), HTML comments (invisible to a
    reader, visible to a model), zero-width and bidi control characters, and
    code-fence fences that would close a quoted block.  Whitespace is
    normalised and the result is truncated with an explicit marker.
    """
    if not text:
        return ""
    body = str(text).replace("\r\n", "\n").replace("\r", "\n")
    body = body.replace("\x00", "")
    body = _HTML_COMMENT.sub("", body)
    body = _ZERO_WIDTH.sub("", body)
    body = _EVIDENCE_TAG.sub("[evidence-delimiter removed]", body)
    lines = [line for line in body.split("\n") if not _FENCE.match(line)]
    body = "\n".join(lines)
    body = re.sub(r"[ \t]+$", "", body, flags=re.MULTILINE)
    body = re.sub(r"\n{3,}", "\n\n", body).strip()
    if len(body) > MAX_AUTHOR_CHARS:
        marker = "\n[description truncated by the reviewer]"
        body = body[: MAX_AUTHOR_CHARS - len(marker)] + marker
    return body


# --------------------------------------------------------------------------- #
# Host-side facts: the AST of the changed files at the head commit
# --------------------------------------------------------------------------- #

#: Files larger than this are not parsed; the fact set records the skip.
MAX_FILE_CHARS = 600_000
#: Per-file, per-list bound so one huge file cannot flood a prompt.
MAX_ITEMS = 200
#: Bound on the added-line excerpts carried in ``facts``.
MAX_ADDED_LINES = 80

_DEF_PATTERNS: tuple[re.Pattern[str], ...] = (
    re.compile(r"^\s*(?:export\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)"),
    re.compile(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:\(|function)"),
    re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z_]\w*)"),
    re.compile(r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)"),
    re.compile(r"^\s*(?:export\s+)?(?:abstract\s+|final\s+|public\s+|private\s+|protected\s+|static\s+)*"
               r"(?:class|struct|interface|enum|trait|record|object)\s+([A-Za-z_]\w*)"),
    re.compile(r"^\s*(?:public|private|protected|internal|static|final|override|virtual|\s)+"
               r"[\w<>\[\],.?*\s]+\s+([A-Za-z_]\w*)\s*\([^;]*\)\s*\{?\s*$"),
    re.compile(r"^\s*(?:def|defp|fun|sub|proc)\s+([A-Za-z_]\w*)"),
)
_CALL = re.compile(r"\b([A-Za-z_$][\w$]*)\s*\(")
_IDENT = re.compile(r"[A-Za-z_$][\w$]*")
_RETURN = re.compile(r"^\s*return\b(.*)$")
_CALL_KEYWORDS = {
    "if", "for", "while", "return", "print", "len", "range", "int", "str", "list", "dict", "set",
    "tuple", "super", "isinstance", "sorted", "min", "max", "sum", "any", "all", "bool", "float",
    "bytes", "open", "type", "getattr", "setattr", "hasattr", "enumerate", "zip", "map", "filter",
}


def _show(repo: GitRepo, rev: str, path: str) -> str:
    """File content at ``rev``; ``""`` when the path does not exist there."""
    if not rev or not path:
        return ""
    try:
        text = repo.show(rev, path)
    except GitError:
        return ""
    return text[:MAX_FILE_CHARS] if len(text) > MAX_FILE_CHARS else text


def _signature(lines: Sequence[str], line: int, end: int) -> str:
    """The definition header, with continuation lines while a bracket is open."""
    if not (1 <= line <= len(lines)):
        return ""
    parts: list[str] = []
    depth = 0
    for cursor in range(line - 1, min(end, line + 6, len(lines))):
        text = lines[cursor].strip()
        parts.append(text)
        depth += text.count("(") + text.count("[") + text.count("{")
        depth -= text.count(")") + text.count("]") + text.count("}")
        if depth <= 0:
            break
    return " ".join(parts)[:400]


def _py_facts(text: str) -> dict[str, Any]:
    """Definitions, calls, imports and identifiers of one Python module."""
    facts: dict[str, Any] = {
        "definitions": [],
        "calls": [],
        "imports": [],
        "identifiers": [],
        "constants": [],
        "parse_error": False,
    }
    if not text.strip():
        return facts
    try:
        tree = ast.parse(text)
    except (SyntaxError, ValueError):
        facts["parse_error"] = True
        return facts
    lines = text.splitlines()
    definitions: list[dict[str, Any]] = []
    calls: set[str] = set()
    imports: set[str] = set()
    identifiers: set[str] = set()
    constants: set[str] = set()
    class_methods: set[int] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef):
            for child in node.body:
                if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    class_methods.add(id(child))
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            kind = (
                "class"
                if isinstance(node, ast.ClassDef)
                else ("method" if id(node) in class_methods else "function")
            )
            definitions.append(
                {
                    "name": node.name,
                    "kind": kind,
                    "line": node.lineno,
                    "end_line": getattr(node, "end_lineno", node.lineno) or node.lineno,
                    "signature": _signature(lines, node.lineno, getattr(node, "end_lineno", node.lineno) or node.lineno),
                }
            )
        elif isinstance(node, ast.Call):
            target = node.func
            if isinstance(target, ast.Name):
                calls.add(target.id)
            elif isinstance(target, ast.Attribute):
                calls.add(target.attr)
        elif isinstance(node, ast.Import):
            for alias in node.names:
                imports.add(alias.asname or alias.name.split(".")[0])
        elif isinstance(node, ast.ImportFrom):
            for alias in node.names:
                imports.add(alias.asname or alias.name)
        elif isinstance(node, ast.Name):
            identifiers.add(node.id)
    for node in tree.body if isinstance(tree, ast.Module) else []:
        if isinstance(node, (ast.Assign, ast.AnnAssign)):
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            for target in targets:
                if isinstance(target, ast.Name):
                    constants.add(target.id)
    facts["definitions"] = definitions[:]
    facts["calls"] = sorted(calls - _CALL_KEYWORDS)[:MAX_ITEMS]
    facts["imports"] = sorted(imports)[:MAX_ITEMS]
    facts["identifiers"] = sorted(identifiers)[:MAX_ITEMS * 4]
    facts["constants"] = sorted(constants)[:MAX_ITEMS]
    return facts


def _regex_facts(text: str) -> dict[str, Any]:
    """Definition/call/identifier facts for a non-Python file (bounded regex)."""
    facts: dict[str, Any] = {
        "definitions": [],
        "calls": [],
        "imports": [],
        "identifiers": [],
        "constants": [],
        "parse_error": False,
    }
    if not text.strip():
        return facts
    definitions: list[dict[str, Any]] = []
    calls: set[str] = set()
    identifiers: set[str] = set()
    for number, line in enumerate(text.splitlines()[:MAX_ITEMS * 40], 1):
        identifiers.update(_IDENT.findall(line))
        for pattern in _DEF_PATTERNS:
            match = pattern.match(line)
            if match:
                definitions.append(
                    {
                        "name": match.group(1),
                        "kind": "function",
                        "line": number,
                        "end_line": number,
                        "signature": line.strip()[:400],
                    }
                )
                break
        for match in _CALL.finditer(line):
            calls.add(match.group(1))
    facts["definitions"] = definitions[:MAX_ITEMS]
    facts["calls"] = sorted(calls - _CALL_KEYWORDS)[:MAX_ITEMS]
    facts["identifiers"] = sorted(identifiers)[:MAX_ITEMS * 4]
    return facts


def _file_facts(path: str, head_text: str, base_text: str) -> dict[str, Any]:
    reader = _py_facts if path.endswith(".py") else _regex_facts
    return {"head": reader(head_text), "base": reader(base_text)}


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


def _removed_lines(patch: FilePatch) -> list[tuple[int, str]]:
    out: list[tuple[int, str]] = []
    for hunk in patch.hunks:
        cursor = hunk.old_start
        for line in hunk.lines:
            if line.startswith("-") and not line.startswith("---"):
                out.append((cursor, line[1:]))
                cursor += 1
            elif line.startswith("+") and not line.startswith("+++"):
                continue
            elif line.startswith("\\"):
                continue
            else:
                cursor += 1
    return out


def _definitions_touching(definitions: Iterable[dict[str, Any]], lines: set[int]) -> list[str]:
    """Names of definitions whose header or body covers one of ``lines``."""
    names: list[str] = []
    for item in definitions:
        start = int(item.get("line") or 0)
        end = int(item.get("end_line") or start)
        if any(start <= number <= end for number in lines):
            names.append(str(item.get("name") or ""))
    return [name for name in names if name]


def ast_facts(repo: GitRepo, diff: DiffBundle) -> dict[str, Any]:
    """AST facts for every changed file, at the head and base commits.

    The result is plain JSON-able data (never the raw code) and is the only
    authority used to ground a claim.  Keys:

    ``definitions``/``base_definitions`` — names defined in the changed files;
    ``added_definitions`` — definitions whose header or body is new/changed;
    ``removed_definitions`` — definitions that existed at base and are gone;
    ``changed_definitions`` — definitions a changed line falls inside;
    ``added_calls`` — calls appearing on an added line;
    ``changed_symbols`` — the union of the two above, used for grounding;
    ``known_symbols`` — every symbol the changed files mention (definitions,
    calls, imports, identifiers), used to test that a claim names real code.
    """
    files: list[dict[str, Any]] = []
    definitions: set[str] = set()
    base_definitions: set[str] = set()
    added_definitions: set[str] = set()
    removed_definitions: set[str] = set()
    changed_definitions: set[str] = set()
    calls: set[str] = set()
    added_calls: set[str] = set()
    imports: set[str] = set()
    classes: set[str] = set()
    known: set[str] = set()
    added_returns: list[str] = []
    new_files: list[str] = []
    deleted_files: list[str] = []

    for patch in diff.files:
        if patch.is_binary:
            continue
        head_text = "" if patch.is_deleted else _show(repo, diff.head_sha, patch.path)
        base_text = "" if patch.is_new else _show(repo, diff.base_sha, patch.old_path or patch.path)
        facts = _file_facts(patch.path, head_text, base_text)
        head = facts["head"]
        base = facts["base"]
        added = _added_lines(patch)
        removed = _removed_lines(patch)
        added_numbers = {number for number, _ in added}
        removed_numbers = {number for number, _ in removed}

        head_names = {str(d["name"]) for d in head["definitions"] if d.get("name")}
        base_names = {str(d["name"]) for d in base["definitions"] if d.get("name")}
        file_added = set(_definitions_touching(head["definitions"], added_numbers))
        file_added |= head_names - base_names
        file_removed = set(_definitions_touching(base["definitions"], removed_numbers))
        file_removed |= base_names - head_names
        file_changed = set(_definitions_touching(head["definitions"], added_numbers))
        file_added_calls = {
            match.group(1)
            for _, text in added
            for match in _CALL.finditer(text)
            if match.group(1) not in _CALL_KEYWORDS
        }

        definitions |= head_names
        base_definitions |= base_names
        added_definitions |= file_added
        removed_definitions |= file_removed
        changed_definitions |= file_changed
        calls |= set(head["calls"])
        added_calls |= file_added_calls
        imports |= set(head["imports"])
        classes |= {str(d["name"]) for d in head["definitions"] if d.get("kind") == "class"}
        known |= head_names | set(head["calls"]) | set(head["imports"]) | set(head["identifiers"])
        known |= set(head["constants"])
        for _, text in added:
            match = _RETURN.match(text)
            if match:
                added_returns.append(match.group(1).strip()[:200])
        if patch.is_new:
            new_files.append(patch.path)
        if patch.is_deleted:
            deleted_files.append(patch.path)

        files.append(
            {
                "path": patch.path,
                "old_path": patch.old_path or "",
                "is_new": patch.is_new,
                "is_deleted": patch.is_deleted,
                "parse_error": bool(head.get("parse_error")),
                "definitions": [d for d in head["definitions"]][:MAX_ITEMS],
                "base_definitions": [d for d in base["definitions"]][:MAX_ITEMS],
                "added_definitions": sorted(file_added),
                "removed_definitions": sorted(file_removed),
                "changed_definitions": sorted(file_changed),
                "added_calls": sorted(file_added_calls),
                "calls": list(head["calls"]),
                "imports": list(head["imports"]),
                "added_lines": [
                    {"line": number, "text": text[:300]} for number, text in added[:MAX_ADDED_LINES]
                ],
            }
        )

    changed_symbols = added_definitions | added_calls | changed_definitions
    return {
        "head_sha": diff.head_sha,
        "base_sha": diff.base_sha,
        "files": files,
        "changed_files": [f["path"] for f in files],
        "new_files": new_files,
        "deleted_files": deleted_files,
        "definitions": sorted(definitions),
        "base_definitions": sorted(base_definitions),
        "added_definitions": sorted(added_definitions),
        "removed_definitions": sorted(removed_definitions),
        "changed_definitions": sorted(changed_definitions),
        "modified_definitions": sorted(added_definitions & base_definitions),
        "calls": sorted(calls),
        "added_calls": sorted(added_calls),
        "imports": sorted(imports),
        "classes": sorted(classes),
        "added_returns": added_returns[:MAX_ADDED_LINES],
        "changed_symbols": sorted(changed_symbols),
        "known_symbols": sorted(known),
    }


# --------------------------------------------------------------------------- #
# Claim grounding
# --------------------------------------------------------------------------- #

_REMOVAL = re.compile(
    r"\b(?:remov\w*|delet\w*|drop\w*|eliminat\w*|no\s+longer|gets?\s+rid\s+of|stops?\s+calling)\b",
    re.IGNORECASE,
)
_ADDITION = re.compile(r"\b(?:add\w*|introduc\w*|creat\w*|implement\w*|new|brand-new)\b", re.IGNORECASE)


def _unique(values: Iterable[str]) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    for value in values:
        text = str(value or "").strip()
        if text and text not in seen:
            seen.add(text)
            out.append(text)
    return out


def _claim_symbols(text: str, facts: dict[str, Any]) -> list[str]:
    """Symbols a claim names, restricted to names the code actually contains."""
    known = set(facts.get("known_symbols") or ())
    candidates = [token for token in _IDENT.findall(text or "") if token in known]
    changed = set(facts.get("changed_symbols") or ())
    preferred = [token for token in candidates if token in changed]
    return _unique(preferred + candidates)[:12]


def _ground_claim(
    claim_text: str, symbols: Sequence[str], facts: dict[str, Any]
) -> tuple[bool, str, bool]:
    """Return ``(supported, evidence, contradiction)`` for one claim."""
    known = set(facts.get("known_symbols") or ())
    definitions = set(facts.get("definitions") or ())
    base_definitions = set(facts.get("base_definitions") or ())
    added_definitions = set(facts.get("added_definitions") or ())
    removed_definitions = set(facts.get("removed_definitions") or ())
    changed = set(facts.get("changed_symbols") or ())
    symbols = _unique(symbols)
    present = [s for s in symbols if s in known]
    missing = [s for s in symbols if s not in known]
    notes: list[str] = []
    contradiction = False

    if not symbols:
        notes.append("the claim names no symbol the changed code contains")
    if _REMOVAL.search(claim_text or ""):
        still_defined = [
            s for s in present if s in definitions and s not in removed_definitions
        ]
        if still_defined:
            contradiction = True
            notes.append(
                "contradicted: the description says it is removed but "
                + ", ".join(still_defined)
                + " is still defined at head"
            )
    elif _ADDITION.search(claim_text or ""):
        pre_existing = [s for s in present if s in base_definitions and s not in added_definitions]
        if pre_existing:
            contradiction = True
            notes.append(
                "contradicted: the description says it is added but "
                + ", ".join(pre_existing)
                + " already existed before the change"
            )
    if missing:
        notes.append("not found in the changed files: " + ", ".join(missing))
    touched = [s for s in present if s in changed]
    if touched:
        notes.insert(0, "grounded: " + ", ".join(touched) + " is changed or added by this diff")
    elif present:
        notes.append("none of the named symbols is changed or added by this diff")
    supported = bool(symbols) and not missing and bool(touched) and not contradiction
    return supported, "; ".join(notes)[:600], contradiction


def _prompt_facts(facts: dict[str, Any]) -> dict[str, Any]:
    """A compact view of ``facts`` for the claim-extraction prompt."""
    files = []
    for item in list(facts.get("files") or ())[:40]:
        files.append(
            {
                "path": item.get("path", ""),
                "is_new": item.get("is_new", False),
                "is_deleted": item.get("is_deleted", False),
                "added_definitions": item.get("added_definitions", []),
                "removed_definitions": item.get("removed_definitions", []),
                "changed_definitions": item.get("changed_definitions", []),
                "added_calls": item.get("added_calls", []),
                "added_lines": list(item.get("added_lines") or ())[:40],
            }
        )
    return {
        "head_sha": facts.get("head_sha", ""),
        "base_sha": facts.get("base_sha", ""),
        "changed_files": facts.get("changed_files", []),
        "new_files": facts.get("new_files", []),
        "deleted_files": facts.get("deleted_files", []),
        "added_definitions": facts.get("added_definitions", []),
        "removed_definitions": facts.get("removed_definitions", []),
        "added_calls": facts.get("added_calls", []),
        "files": files,
    }


def _short(sha: str) -> str:
    return (sha or "")[:12] or "<unknown>"


def check_consistency(
    description: str,
    facts: dict[str, Any],
    llm: Any,
    *,
    settings: Settings,
) -> MciResult:
    """Extract the description's claims, then ground each one in host code.

    The model is used for *claims only*: every ``supported``/``contradiction``
    bit is computed here from ``facts``.  An LLM failure propagates as a typed
    ``LLMError`` so the pipeline can record it and degrade to human review; it
    is never reported as "the description is consistent".
    """
    stripped = strip_author_text(description)
    if not stripped.strip():
        return MciResult(
            claims=[],
            contradiction=False,
            summary="no pull request description text to check",
            stripped_description=True,
        )

    prompt = pmi_prompt(stripped, _prompt_facts(facts))
    answer = llm.complete(
        "pmi",
        prompt,
        MciScan,
        stage_model=settings.llm.triage,
        temperature=0.0,
    )
    scan = getattr(answer, "value", answer)
    if isinstance(scan, dict):
        try:
            scan = MciScan.model_validate(scan)
        except Exception as exc:  # noqa: BLE001 - surfaced as a typed failure
            raise LLMError(
                f"claim extractor answered outside the MciScan schema: {exc}",
                kind=FailureKind.MALFORMED_OUTPUT,
            ) from None
    if not isinstance(scan, MciScan):
        raise LLMError(
            "claim extractor did not return an MciScan",
            kind=FailureKind.MALFORMED_OUTPUT,
        )

    claims: list[MciClaim] = []
    contradicted: list[MciClaim] = []
    for raw in scan.claims:
        symbols = _unique(raw.symbols) or _claim_symbols(raw.claim, facts)
        supported, evidence, contradiction = _ground_claim(raw.claim, symbols, facts)
        claim = MciClaim(
            claim=raw.claim,
            symbols=symbols[:20],
            supported=supported,
            evidence=evidence,
        )
        claims.append(claim)
        if contradiction:
            contradicted.append(claim)

    supported_count = sum(1 for claim in claims if claim.supported)
    summary = (
        f"{len(claims)} claim(s) checked against head {_short(str(facts.get('head_sha') or ''))}: "
        f"{supported_count} supported, {len(contradicted)} contradicted"
    )
    if contradicted:
        quote = contradicted[0].claim.replace("\n", " ").strip()
        summary += f'; first contradicted claim: "{quote[:160]}"'
    return MciResult(
        claims=claims,
        contradiction=bool(contradicted),
        summary=summary[:600],
        # The description was stripped before it left this module and never
        # appears in the result except as the short quote above.
        stripped_description=True,
    )
