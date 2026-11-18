"""CPG frame projection and the taint/lifetime track (Stage 2B, host side).

The project has no code-property-graph engine, so a frame is a *projection* of
what the source itself shows: untrusted roots, aliases, write sets, reachable
source -> sink pairs and allocation/free/use tuples.  Nothing is inferred from a
name's spelling alone, and a frame never claims a flow the source does not
contain.

* Python — real ``ast``: assignments from known source calls taint their
  targets; a tainted value reaching a known sink produces a ``TaintTuple``;
  ``x = y`` produces an alias pair; ``open`` without a ``with``/``close``
  produces an allocation without a free.
* C/C++ — regex-level: ``malloc``/``calloc``/``realloc`` roots, ``free`` and
  use-after-free ordering, ``memcpy`` aliases, and the two LSOG obligation forms
  a review can see in an ACSL annotation (``\\separated`` and
  ``assigns ... \\from``).

``rules/policy.json`` maps an analyzer rule id to the track that rule feeds.
"""

from __future__ import annotations

import ast
import json
import re
from pathlib import Path
from typing import Any, Iterable, Sequence

from ..errors import ToolError
from ..models import (
    AnalyzerFinding,
    CitedSite,
    CpgFrame,
    LsegObligation,
    LifetimeTuple,
    TaintTuple,
)

POLICY_TRACKS = ("taint", "lifetime")

#: Calls whose return value is attacker-influenced (or environment-influenced)
#: from the reviewer's point of view.
_SOURCE_CALLS = frozenset(
    {
        "input",
        "raw_input",
        "os.environ",
        "os.environ.get",
        "os.getenv",
        "sys.argv",
        "sys.stdin",
        "sys.stdin.read",
        "sys.stdin.readline",
        "request.args.get",
        "request.form.get",
        "request.values.get",
        "request.get_json",
        "request.json",
        "request.data",
        "socket.recv",
        "recv",
        "recvfrom",
    }
)

#: Calls that execute, deserialize or interpolate a value somewhere dangerous.
_SINK_CALLS = frozenset(
    {
        "eval",
        "exec",
        "execfile",
        "os.system",
        "os.popen",
        "os.execv",
        "os.spawnl",
        "subprocess.run",
        "subprocess.call",
        "subprocess.Popen",
        "subprocess.check_output",
        "subprocess.check_call",
        "subprocess.getoutput",
        "yaml.load",
        "yaml.unsafe_load",
        "pickle.loads",
        "pickle.load",
        "marshal.loads",
        "db.execute",
        "conn.execute",
        "cursor.execute",
        "cursor.executemany",
        "execute",
        "executemany",
    }
)

_ASSIGN_LHS = re.compile(r"^\s*([A-Za-z_]\w*(?:->\w+|\.\w+)?)\s*=(?!=)")
_ALLOC = re.compile(r"\b([A-Za-z_]\w*)\s*=\s*(?:\([^;)]*\)\s*)?(malloc|calloc|realloc|strdup|_strdup)\s*\(")
_FREE = re.compile(r"\bfree\s*\(\s*([A-Za-z_]\w*)\s*\)")
_MEMCPY = re.compile(r"\bmemcpy\s*\(\s*([A-Za-z_]\w*)\s*,\s*([A-Za-z_]\w*)")
_ARROW = re.compile(r"\b([A-Za-z_]\w*)\s*->")
_SEPARATED = re.compile(r"\\separated\s*\(([^)]*)\)")
_ASSIGNS_FROM = re.compile(r"assigns\s+([^;\\]+?)\s*\\from\s+([^;\\]+)")
_C_FUNC = re.compile(r"^[ \t]*[A-Za-z_][\w \t\*]*?\b([A-Za-z_]\w*)\s*\([^;{]*\)\s*\{", re.MULTILINE)


# --------------------------------------------------------------------------- #
# Policy
# --------------------------------------------------------------------------- #


def parse_policy(path: Path) -> dict:
    """Read ``rules/policy.json`` and validate it.  Always raises ``ToolError``."""
    target = Path(path)
    try:
        raw = target.read_text(encoding="utf-8")
    except OSError as exc:
        raise ToolError(f"cannot read the CPG policy at {target}", detail=str(exc)) from None
    try:
        doc = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ToolError(f"CPG policy {target} is not valid JSON", detail=str(exc)) from None
    if not isinstance(doc, dict):
        raise ToolError(f"CPG policy {target} must be a JSON object")
    items = doc.get("rules")
    if not isinstance(items, list):
        raise ToolError(f"CPG policy {target} has no 'rules' array")
    rules: list[dict[str, Any]] = []
    seen: set[str] = set()
    for index, item in enumerate(items):
        if not isinstance(item, dict):
            raise ToolError(f"CPG policy {target}: rules[{index}] is not an object")
        rule_id = item.get("id")
        if not isinstance(rule_id, str) or not rule_id.strip():
            raise ToolError(f"CPG policy {target}: rules[{index}] has no string id")
        rule_id = rule_id.strip()
        if rule_id in seen:
            raise ToolError(f"CPG policy {target}: duplicate rule id {rule_id!r}")
        seen.add(rule_id)
        track = item.get("track")
        if track not in POLICY_TRACKS:
            raise ToolError(f"CPG policy {target}: rule {rule_id!r} has track {track!r}, expected one of {POLICY_TRACKS}")
        severity = item.get("severity", 1)
        if isinstance(severity, bool) or not isinstance(severity, int) or not 0 <= severity <= 3:
            raise ToolError(f"CPG policy {target}: rule {rule_id!r} has a severity outside 0..3")
        cwe = item.get("cwe", "")
        if cwe is None:
            cwe = ""
        if not isinstance(cwe, str):
            raise ToolError(f"CPG policy {target}: rule {rule_id!r} has a non-string cwe")
        rules.append({"id": rule_id, "track": track, "cwe": cwe, "severity": int(severity)})
    rules.sort(key=lambda rule: rule["id"])
    return {"rules": rules}


def taint_and_lifetime(
    analyzer: list[AnalyzerFinding], policy: dict
) -> tuple[list[TaintTuple], list[LifetimeTuple]]:
    """Map analyzer findings onto CPG tuples using the policy's track assignment."""
    rules = policy.get("rules", []) if isinstance(policy, dict) else []
    rules = [rule for rule in rules if isinstance(rule, dict)]
    taint: list[TaintTuple] = []
    lifetime: list[LifetimeTuple] = []
    for finding in sorted(analyzer, key=lambda f: (f.path, f.line, f.rule_id)):
        rule = _match_rule(finding, rules)
        if rule is None:
            continue
        site = _site(finding.path, finding.line, finding.end_line, finding.rule_id, finding.message)
        if rule.get("track") == "taint":
            taint.append(TaintTuple(source=site, sink=site, rule_id=finding.rule_id))
        else:
            alloc = free = use = None
            if _is_alloc_rule(finding.rule_id):
                alloc = site
            elif _is_free_rule(finding.rule_id):
                free = site
            else:
                use = site
            lifetime.append(LifetimeTuple(alloc=alloc, free=free, use=use, rule_id=finding.rule_id))
    return taint, lifetime


def _match_rule(finding: AnalyzerFinding, rules: Sequence[dict[str, Any]]) -> dict[str, Any] | None:
    wanted = _short_rule_id(finding.rule_id)
    for rule in rules:
        if _short_rule_id(str(rule.get("id", ""))) == wanted:
            return rule
    finding_cwe = _cwe_number(finding.cwe)
    if finding_cwe:
        for rule in rules:
            if _cwe_number(str(rule.get("cwe", ""))) == finding_cwe:
                return rule
    return None


def _short_rule_id(rule_id: str) -> str:
    marker = "zero-loop."
    if marker in rule_id:
        return rule_id.rsplit(marker, 1)[-1]
    return rule_id


def _cwe_number(text: str) -> str:
    match = re.search(r"CWE-(\d+)", text or "")
    return match.group(0) if match else ""


def _is_alloc_rule(rule_id: str) -> bool:
    low = rule_id.lower()
    return any(token in low for token in ("open", "alloc", "malloc", "calloc", "realloc", "acquire", "tmpfile", "mkstemp"))


def _is_free_rule(rule_id: str) -> bool:
    low = rule_id.lower()
    return any(token in low for token in ("free", "close", "release", "drop"))


def _site(path: str, line: int, end_line: int | None, symbol: str, snippet: str) -> CitedSite:
    return CitedSite(
        path=path,
        line=line,
        end_line=end_line if end_line is not None and end_line >= line else None,
        symbol=symbol[:512],
        snippet=(snippet or "")[:400],
    )


# --------------------------------------------------------------------------- #
# Frame projection
# --------------------------------------------------------------------------- #

_PY_SUFFIXES = (".py", ".pyw", ".pyi")
_C_SUFFIXES = (".c", ".h")
_CPP_SUFFIXES = (".cc", ".cpp", ".cxx", ".hpp", ".hh", ".hxx")


def frame_projection(path: str, source: str, language: str) -> CpgFrame:
    """Project everything the source itself shows into a :class:`CpgFrame`."""
    resolved = _resolve_language(path, language)
    if resolved == "python":
        return _python_frame(path, source)
    if resolved in ("c", "cpp"):
        return _c_frame(path, source, resolved)
    return CpgFrame(path=path, language=resolved)


def _resolve_language(path: str, language: str) -> str:
    lang = (language or "").strip().lower()
    if lang in ("python", "py", "python3"):
        return "python"
    if lang in ("c", "cpp", "c++", "cxx"):
        return "cpp" if lang in ("cpp", "c++", "cxx") else "c"
    lowered = path.lower()
    if lowered.endswith(_PY_SUFFIXES):
        return "python"
    if lowered.endswith(_CPP_SUFFIXES):
        return "cpp"
    if lowered.endswith(_C_SUFFIXES):
        return "c"
    return lang


# -- Python ---------------------------------------------------------------- #


def _python_frame(path: str, source: str) -> CpgFrame:
    try:
        tree = ast.parse(source, filename=path)
    except SyntaxError:
        return CpgFrame(path=path, language="python")
    frame = CpgFrame(path=path, language="python", function=_primary_function(tree))
    roots: set[str] = set()
    aliases: set[tuple[str, str]] = set()
    writes: set[str] = set()
    taint: list[TaintTuple] = []
    lifetime: list[LifetimeTuple] = []

    scopes: list[ast.AST] = [tree]
    scopes.extend(
        node for node in ast.walk(tree) if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
    )
    source_lines = source.splitlines()
    for scope in scopes:
        scope_start = getattr(scope, "lineno", 1)
        scope_end = getattr(scope, "end_lineno", None) or len(source_lines)
        tainted: dict[str, tuple[int, str]] = {}
        for node in _scope_nodes(scope):
            if isinstance(node, (ast.Assign, ast.AnnAssign)):
                targets = node.targets if isinstance(node, ast.Assign) else [node.target]
                value = node.value
                for target in targets:
                    name = _target_name(target)
                    if not name:
                        continue
                    writes.add(name)
                    source_ref = _source_ref(value)
                    if source_ref is not None:
                        label, line = source_ref
                        roots.add(label)
                        tainted[name] = (line, label)
                    elif isinstance(value, ast.Name | ast.Attribute):
                        alias = _dotted(value)
                        if alias and alias != name:
                            aliases.add((alias, name))
                        if alias in tainted:
                            tainted[name] = tainted[alias]
                    opened = _open_call(value)
                    if opened is not None and isinstance(target, ast.Name):
                        close_line = _close_line(source_lines, target.id, scope_start, scope_end)
                        lifetime.append(
                            LifetimeTuple(
                                alloc=_site(path, opened, None, "open", ""),
                                free=_site(path, close_line, None, "close", "") if close_line else None,
                                rule_id="cpg.open-without-context",
                            )
                        )
            elif isinstance(node, ast.With | ast.AsyncWith):
                for item in node.items:
                    opened = _open_call(item.context_expr)
                    if opened is None:
                        continue
                    end = node.end_lineno or node.lineno
                    lifetime.append(
                        LifetimeTuple(
                            alloc=_site(path, opened, None, "open", ""),
                            free=_site(path, end, None, "with", ""),
                            rule_id="cpg.context-manager",
                        )
                    )
            elif isinstance(node, ast.Call):
                sink = _sink_name(node)
                if sink is None:
                    continue
                for arg in [*node.args, *[keyword.value for keyword in node.keywords]]:
                    ref = _source_ref(arg)
                    if ref is not None:
                        label, line = ref
                        roots.add(label)
                        taint.append(
                            TaintTuple(
                                source=_site(path, line, None, label, ""),
                                sink=_site(path, node.lineno, None, sink, ""),
                                rule_id=f"cpg.{sink.split('.')[-1]}",
                            )
                        )
                        continue
                    for name in _names_in(arg):
                        if name in tainted:
                            line, label = tainted[name]
                            taint.append(
                                TaintTuple(
                                    source=_site(path, line, None, label, ""),
                                    sink=_site(path, node.lineno, None, sink, ""),
                                    rule_id=f"cpg.{sink.split('.')[-1]}",
                                )
                            )
                            break

    frame.memory_roots = sorted(roots)
    frame.alias_pairs = sorted(aliases)
    frame.write_sets = sorted(writes)
    frame.taint = _dedupe_taint(taint)
    frame.lifetime = _dedupe_lifetime(lifetime)
    if roots:
        frame.obligations = [
            LsegObligation(
                kind="taint",
                subjects=sorted(roots),
                detail="untrusted sources observed in this file; a guard must separate each from its sink",
            )
        ]
    return frame


def _primary_function(tree: ast.Module) -> str:
    functions = [node for node in ast.walk(tree) if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))]
    if not functions:
        return ""
    best = max(functions, key=lambda node: (len(node.body), -node.lineno))
    return best.name


def _scope_nodes(scope: ast.AST) -> list[ast.AST]:
    """Every node of one scope in source order; nested scopes are excluded."""
    nodes: list[ast.AST] = []
    stack: list[ast.AST] = [scope]
    while stack:
        current = stack.pop()
        for child in ast.iter_child_nodes(current):
            if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Lambda)):
                continue
            nodes.append(child)
            stack.append(child)
    nodes.sort(key=lambda node: (getattr(node, "lineno", 0), getattr(node, "col_offset", 0)))
    return nodes


def _target_name(target: ast.AST) -> str:
    if isinstance(target, ast.Name):
        return target.id
    return _dotted(target)


def _names_in(node: ast.AST) -> Iterable[str]:
    for child in ast.walk(node):
        if isinstance(child, ast.Name) and isinstance(child.ctx, ast.Load):
            yield child.id


def _source_ref(node: ast.AST | None) -> tuple[str, int] | None:
    """The first known untrusted-source reference inside ``node``, if any."""
    if node is None:
        return None
    if isinstance(node, ast.Call):
        name = _dotted(node.func)
        if name in _SOURCE_CALLS:
            return name, node.lineno
    if isinstance(node, ast.Subscript):
        name = _dotted(node.value)
        if name in _SOURCE_CALLS:
            return name, node.lineno
    if isinstance(node, ast.Attribute):
        name = _dotted(node)
        if name in _SOURCE_CALLS:
            return name, node.lineno
    for child in ast.iter_child_nodes(node):
        found = _source_ref(child)
        if found is not None:
            return found
    return None


def _sink_name(call: ast.Call) -> str | None:
    name = _dotted(call.func)
    if name in _SINK_CALLS:
        return name
    tail = name.rsplit(".", 1)[-1]
    if tail in {"eval", "exec", "system", "popen", "execute", "executemany"}:
        return tail
    return None


def _open_call(node: ast.AST | None) -> int | None:
    if isinstance(node, ast.Call) and _dotted(node.func).rsplit(".", 1)[-1] == "open":
        return node.lineno
    return None


def _close_line(lines: Sequence[str], name: str, start: int, end: int) -> int | None:
    """The line of ``name.close()`` inside the same scope, if any."""
    pattern = re.compile(rf"\b{re.escape(name)}\s*\.\s*close\s*\(")
    for index in range(max(1, start), min(len(lines), end) + 1):
        if pattern.search(lines[index - 1]):
            return index
    return None


def _dotted(node: ast.AST) -> str:
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        base = _dotted(node.value)
        return f"{base}.{node.attr}" if base else node.attr
    if isinstance(node, ast.Subscript):
        return _dotted(node.value)
    if isinstance(node, ast.Call):
        return _dotted(node.func)
    return ""


def _dedupe_taint(tuples: list[TaintTuple]) -> list[TaintTuple]:
    seen: set[tuple[str, int, int, str]] = set()
    out: list[TaintTuple] = []
    for item in sorted(tuples, key=lambda t: (t.sink.line, t.source.line, t.rule_id)):
        key = (item.sink.path, item.sink.line, item.source.line, item.rule_id)
        if key in seen:
            continue
        seen.add(key)
        out.append(item)
    return out


def _dedupe_lifetime(tuples: list[LifetimeTuple]) -> list[LifetimeTuple]:
    seen: set[tuple[int, int, int]] = set()
    out: list[LifetimeTuple] = []
    for item in sorted(
        tuples,
        key=lambda t: (
            t.alloc.line if t.alloc else 0,
            t.free.line if t.free else 0,
            t.use.line if t.use else 0,
            t.rule_id,
        ),
    ):
        key = (
            item.alloc.line if item.alloc else 0,
            item.free.line if item.free else 0,
            item.use.line if item.use else 0,
        )
        if key in seen:
            continue
        seen.add(key)
        out.append(item)
    return out


# -- C / C++ --------------------------------------------------------------- #


def _c_frame(path: str, source: str, language: str) -> CpgFrame:
    lines = source.splitlines()
    frame = CpgFrame(path=path, language=language)
    match = _C_FUNC.search(source)
    if match:
        frame.function = match.group(1)

    roots: set[str] = set()
    writes: set[str] = set()
    aliases: set[tuple[str, str]] = set()
    obligations: list[LsegObligation] = []
    allocs: dict[str, int] = {}
    frees: dict[str, int] = {}
    uses: dict[str, int] = {}

    for index, line in enumerate(lines, start=1):
        for name in _ALLOC.findall(line):
            allocs.setdefault(name[0], index)
            roots.add(name[0])
        for name in _FREE.findall(line):
            frees.setdefault(name, index)
        for base in _ARROW.findall(line):
            roots.add(base)
            owner = frees.get(base)
            if owner is not None and index > owner:
                uses.setdefault(base, index)
        lhs = _ASSIGN_LHS.match(line)
        if lhs:
            writes.add(lhs.group(1))
        for dst, src in _MEMCPY.findall(line):
            aliases.add((src, dst))
        for group in _SEPARATED.findall(line):
            subjects = [part.strip() for part in group.split(",") if part.strip()]
            obligations.append(
                LsegObligation(kind="separated", subjects=subjects, detail=f"{path}:{index} \\separated")
            )
        for assigned, origin in _ASSIGNS_FROM.findall(line):
            obligations.append(
                LsegObligation(
                    kind="assigns_from",
                    subjects=[part.strip() for part in assigned.split(",") if part.strip()]
                    + [part.strip() for part in origin.split(",") if part.strip()],
                    detail=f"{path}:{index} assigns ... \\from",
                )
            )

    lifetime: list[LifetimeTuple] = []
    for name, alloc_line in sorted(allocs.items()):
        free_line = frees.get(name)
        use_line = uses.get(name)
        if free_line is not None and (use_line is None or use_line < free_line):
            use_line = None
        lifetime.append(
            LifetimeTuple(
                alloc=_site(path, alloc_line, None, name, ""),
                free=_site(path, free_line, None, name, "") if free_line is not None else None,
                use=_site(path, use_line, None, name, "") if use_line is not None else None,
                rule_id="cpg.heap-lifetime",
            )
        )
    for name, free_line in sorted(frees.items()):
        if name in allocs:
            continue
        lifetime.append(
            LifetimeTuple(
                alloc=None,
                free=_site(path, free_line, None, name, ""),
                use=None,
                rule_id="cpg.heap-lifetime",
            )
        )

    frame.memory_roots = sorted(roots)
    frame.alias_pairs = sorted(aliases)
    frame.write_sets = sorted(writes)
    frame.lifetime = lifetime
    frame.obligations = obligations
    return frame
