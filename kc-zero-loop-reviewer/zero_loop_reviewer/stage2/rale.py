"""RALE — the three-view structural ensemble (Stage 2C).

Three independent views of the same Python source:

* **AST view** — structural defects that are visible in the syntax tree alone:
  bare ``except``, ``except: pass``, mutable default arguments, unreachable
  statements after a terminator, ``assert`` on a tuple, and a function that
  mixes value returns with an implicit fall-through.
* **CFG view** — control-flow defects: a path that reaches the function exit
  without a value return, an ``except`` handler that leaves an acquired resource
  for the exception path to leak, and a loop whose exit is unreachable.
* **Code view** — a local classifier score (see :mod:`.classifiers`).

The required ensemble is Boolean absorption over the three views::

    flagged = (ast_flag and cfg_flag) or code_flag

Only Python is modelled.  Any other language returns no flags rather than a
guess; ``code_view`` still refuses to invent a score when no classifier is
available.
"""

from __future__ import annotations

import ast
from dataclasses import dataclass, field
from typing import Iterable

from ..errors import ToolError
from ..models import RaleResult
from .classifiers import LocalCodeClassifier

#: A classifier score at or above this value sets ``code_flag``.
CODE_FLAG_THRESHOLD = 0.5

_PYTHON_SUFFIXES = (".py", ".pyw", ".pyi")

_MUTABLE_CALLS = {"list", "dict", "set", "bytearray", "defaultdict", "OrderedDict", "deque"}
_ACQUIRERS = {"open", "acquire", "connect", "mkstemp", "mkdtemp", "socket", "urlopen", "TemporaryFile", "NamedTemporaryFile"}


# --------------------------------------------------------------------------- #
# AST view
# --------------------------------------------------------------------------- #


def ast_view(source: str, path: str) -> tuple[bool, list[str], list[str]]:
    """Return ``(flag, reasons, functions)`` for the structural AST checks."""
    tree = _parse(source, path)
    if tree is None:
        return False, [], []
    functions = list(
        dict.fromkeys(
            node.name
            for node in sorted(
                (n for n in ast.walk(tree) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))),
                key=lambda n: (n.lineno, n.col_offset, n.name),
            )
        )
    )
    reasons: list[str] = []

    for node in ast.walk(tree):
        if isinstance(node, ast.ExceptHandler):
            if node.type is None:
                reasons.append(f"bare_except: {path}:{node.lineno}")
            if node.body and all(isinstance(stmt, ast.Pass) for stmt in node.body):
                reasons.append(f"except_pass: {path}:{node.lineno}")
        if isinstance(node, ast.Assert):
            test = node.test
            if isinstance(test, ast.Tuple) and test.elts:
                reasons.append(f"assert_on_tuple: {path}:{node.lineno}")
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            reasons.extend(_mutable_defaults(node, path))
            reasons.extend(_missing_return_branch(node, path))
        for block in _statement_blocks(node):
            reasons.extend(_unreachable(block, path))

    reasons = sorted(set(reasons))
    return bool(reasons), reasons, functions


def _parse(source: str, path: str) -> ast.Module | None:
    if not path.endswith(_PYTHON_SUFFIXES):
        return None
    try:
        return ast.parse(source, filename=path)
    except SyntaxError:
        return None


def _mutable_defaults(node: ast.FunctionDef | ast.AsyncFunctionDef, path: str) -> list[str]:
    args = node.args
    pairs: list[tuple[ast.arg, ast.expr | None]] = []
    positional = [*args.posonlyargs, *args.args]
    for arg, default in zip(positional[len(positional) - len(args.defaults) :], args.defaults):
        pairs.append((arg, default))
    for arg, default in zip(args.kwonlyargs, args.kw_defaults):
        pairs.append((arg, default))
    out: list[str] = []
    for arg, default in pairs:
        if default is None:
            continue
        if isinstance(default, (ast.List, ast.Dict, ast.Set)):
            out.append(f"mutable_default: {path}:{arg.lineno} {node.name}({arg.arg})")
        elif isinstance(default, ast.Call) and _dotted(default.func).split(".")[-1] in _MUTABLE_CALLS:
            out.append(f"mutable_default: {path}:{arg.lineno} {node.name}({arg.arg})")
    return out


def _missing_return_branch(node: ast.FunctionDef | ast.AsyncFunctionDef, path: str) -> list[str]:
    body = list(node.body)
    if not body:
        return []
    has_value_return = any(
        isinstance(stmt, ast.Return) and stmt.value is not None for stmt in _walk_own(node)
    )
    last = body[-1]
    if has_value_return and not _terminates(last):
        return [f"missing_return_branch: {path}:{last.end_lineno or last.lineno} {node.name}"]
    return []


def _terminates(stmt: ast.stmt) -> bool:
    """Whether every path through ``stmt`` leaves the function."""
    if isinstance(stmt, (ast.Return, ast.Raise)):
        return True
    if isinstance(stmt, (ast.With, ast.AsyncWith)):
        return _block_terminates(stmt.body)
    if isinstance(stmt, ast.If):
        return bool(stmt.orelse) and _block_terminates(stmt.body) and _block_terminates(stmt.orelse)
    if isinstance(stmt, ast.Try):
        blocks = [stmt.body, *[handler.body for handler in stmt.handlers]]
        if stmt.orelse:
            blocks.append(stmt.orelse)
        if stmt.finalbody and _block_terminates(stmt.finalbody):
            return True
        return all(_block_terminates(block) for block in blocks)
    if isinstance(stmt, ast.While) and _is_constant_true(stmt.test) and not _has_break(stmt):
        return True
    return False


def _block_terminates(block: list[ast.stmt]) -> bool:
    return bool(block) and _terminates(block[-1])


def _has_break(node: ast.AST) -> bool:
    stack: list[ast.AST] = list(ast.iter_child_nodes(node))
    while stack:
        current = stack.pop()
        if isinstance(current, ast.Break):
            return True
        if isinstance(current, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef, ast.For, ast.While)):
            continue
        stack.extend(ast.iter_child_nodes(current))
    return False


def _walk_own(node: ast.AST) -> Iterable[ast.AST]:
    """Walk ``node`` without descending into nested function/class bodies."""
    stack: list[ast.AST] = list(ast.iter_child_nodes(node))
    while stack:
        current = stack.pop()
        yield current
        if isinstance(current, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef)):
            continue
        stack.extend(ast.iter_child_nodes(current))


def _statement_blocks(node: ast.AST) -> list[list[ast.stmt]]:
    blocks: list[list[ast.stmt]] = []
    for _field, value in ast.iter_fields(node):
        if isinstance(value, list) and value and all(isinstance(item, ast.stmt) for item in value):
            blocks.append(value)  # type: ignore[arg-type]
    return blocks


def _unreachable(block: list[ast.stmt], path: str) -> list[str]:
    out: list[str] = []
    for index, stmt in enumerate(block):
        if isinstance(stmt, (ast.Return, ast.Raise, ast.Continue, ast.Break)):
            for later in block[index + 1 :]:
                out.append(f"unreachable_code: {path}:{later.lineno}")
            break
    return out


# --------------------------------------------------------------------------- #
# CFG view
# --------------------------------------------------------------------------- #


@dataclass
class _CfgNode:
    kind: str  # enter | stmt | cond | return | raise | break | continue | exit
    line: int = 0
    succ: list[int] = field(default_factory=list)


@dataclass
class _Loop:
    line: int
    cond_node: int
    exit_node: int
    constant_true: bool


@dataclass
class _FunctionCfg:
    name: str
    line: int
    nodes: dict[int, _CfgNode]
    entry: int
    exit_node: int
    loops: list[_Loop]
    bare_returns: list[int]


class _Builder:
    def __init__(self) -> None:
        self.nodes: dict[int, _CfgNode] = {}
        self._next = 0
        self._loops: list[_Loop] = []
        self._break_targets: list[int] = []
        self._continue_targets: list[int] = []
        self.bare_returns: list[int] = []

    # -- primitives -------------------------------------------------------
    def new(self, kind: str, line: int = 0) -> int:
        node_id = self._next
        self._next += 1
        self.nodes[node_id] = _CfgNode(kind=kind, line=line)
        return node_id

    def edge(self, src: int, dst: int) -> None:
        if dst not in self.nodes[src].succ:
            self.nodes[src].succ.append(dst)

    # -- blocks -----------------------------------------------------------
    def block(self, stmts: list[ast.stmt], entry: list[int]) -> list[int]:
        open_exits = entry
        for stmt in stmts:
            open_exits = self.stmt(stmt, open_exits)
        return open_exits

    def stmt(self, stmt: ast.stmt, entry: list[int]) -> list[int]:
        if isinstance(stmt, ast.If):
            cond = self.new("cond", stmt.lineno)
            for src in entry:
                self.edge(src, cond)
            then_exits = self.block(stmt.body, [cond])
            else_exits = self.block(stmt.orelse, [cond]) if stmt.orelse else [cond]
            return then_exits + else_exits
        if isinstance(stmt, (ast.For, ast.AsyncFor, ast.While)):
            constant_true = isinstance(stmt, ast.While) and _is_constant_true(stmt.test)
            cond = self.new("cond", stmt.lineno)
            exit_node = self.new("exit", stmt.end_lineno or stmt.lineno)
            for src in entry:
                self.edge(src, cond)
            if not constant_true:
                self.edge(cond, exit_node)
            self._loops.append(_Loop(stmt.lineno, cond, exit_node, constant_true))
            self._break_targets.append(exit_node)
            self._continue_targets.append(cond)
            body_exits = self.block(stmt.body, [cond])
            for src in body_exits:
                self.edge(src, cond)
            self._break_targets.pop()
            self._continue_targets.pop()
            if stmt.orelse:
                return self.block(stmt.orelse, [exit_node])
            return [exit_node]
        if isinstance(stmt, ast.Try):
            trunk = self.new("stmt", stmt.lineno)
            for src in entry:
                self.edge(src, trunk)
            body_exits = self.block(stmt.body, [trunk])
            handler_exits: list[int] = []
            for handler in stmt.handlers:
                handler_head = self.new("stmt", handler.lineno)
                self.edge(trunk, handler_head)
                handler_exits.extend(self.block(handler.body, [handler_head]))
            merge = body_exits
            if stmt.orelse:
                merge = self.block(stmt.orelse, body_exits)
            open_exits = merge + handler_exits
            if stmt.finalbody:
                return self.block(stmt.finalbody, open_exits)
            return open_exits
        if isinstance(stmt, ast.With | ast.AsyncWith):
            head = self.new("stmt", stmt.lineno)
            for src in entry:
                self.edge(src, head)
            return self.block(stmt.body, [head])
        if isinstance(stmt, ast.Return):
            node_id = self.new("return", stmt.lineno)
            for src in entry:
                self.edge(src, node_id)
            if stmt.value is None:
                self.bare_returns.append(node_id)
            return []
        if isinstance(stmt, ast.Raise):
            node_id = self.new("raise", stmt.lineno)
            for src in entry:
                self.edge(src, node_id)
            return []
        if isinstance(stmt, ast.Break):
            node_id = self.new("break", stmt.lineno)
            for src in entry:
                self.edge(src, node_id)
            if self._break_targets:
                self.edge(node_id, self._break_targets[-1])
            return []
        if isinstance(stmt, ast.Continue):
            node_id = self.new("continue", stmt.lineno)
            for src in entry:
                self.edge(src, node_id)
            if self._continue_targets:
                self.edge(node_id, self._continue_targets[-1])
            return []
        # Nested definitions are separate CFGs; their lifetimes are not ours.
        node_id = self.new("stmt", stmt.lineno)
        for src in entry:
            self.edge(src, node_id)
        return [node_id]


def _is_constant_true(test: ast.expr) -> bool:
    return isinstance(test, ast.Constant) and bool(test.value)


def _build_function(node: ast.FunctionDef | ast.AsyncFunctionDef) -> _FunctionCfg:
    builder = _Builder()
    entry = builder.new("enter", node.lineno)
    exits = builder.block(list(node.body), [entry])
    exit_node = builder.new("exit", node.end_lineno or node.lineno)
    for src in exits:
        builder.edge(src, exit_node)
    return _FunctionCfg(
        name=node.name,
        line=node.lineno,
        nodes=builder.nodes,
        entry=entry,
        exit_node=exit_node,
        loops=builder._loops,
        bare_returns=builder.bare_returns,
    )


def _reachable(cfg: _FunctionCfg) -> set[int]:
    seen: set[int] = set()
    stack = [cfg.entry]
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        stack.extend(cfg.nodes[current].succ)
    return seen


def _cleanup_skip(node: ast.FunctionDef | ast.AsyncFunctionDef) -> list[str]:
    """``try`` that acquires a resource but has no ``finally``."""
    out: list[str] = []
    for child in _walk_own(node):
        if not isinstance(child, ast.Try) or child.finalbody or not child.handlers:
            continue
        acquired = [call for call in ast.walk(ast.Module(body=child.body, type_ignores=[])) if _acquires(call)]
        if not acquired:
            continue
        handler_terminates = any(
            any(isinstance(stmt, (ast.Return, ast.Raise, ast.Continue, ast.Break)) for stmt in handler.body)
            for handler in child.handlers
        )
        if handler_terminates:
            out.append(f"exception_path_skips_cleanup: {node.name}:{child.lineno}")
    return out


def _acquires(call: ast.AST) -> bool:
    if not isinstance(call, ast.Call):
        return False
    name = _dotted(call.func).split(".")[-1]
    return name in _ACQUIRERS


def cfg_view(source: str, path: str) -> tuple[bool, list[str]]:
    """Return ``(flag, reasons)`` for the control-flow checks."""
    tree = _parse(source, path)
    if tree is None:
        return False, []
    reasons: list[str] = []
    for node in ast.walk(tree):
        if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        cfg = _build_function(node)
        reachable = _reachable(cfg)
        returns_value = any(
            cfg.nodes[node_id].kind == "return" and node_id not in cfg.bare_returns for node_id in reachable
        )
        falls_through = cfg.exit_node in reachable
        if returns_value and falls_through:
            reasons.append(f"missing_return_on_path: {path}:{node.lineno} {node.name}")
        for loop in cfg.loops:
            if loop.constant_true and loop.cond_node in reachable and loop.exit_node not in reachable:
                reasons.append(f"loop_without_exit: {path}:{loop.line}")
        reasons.extend(_cleanup_skip(node))
    reasons = sorted(set(reasons))
    return bool(reasons), reasons


# --------------------------------------------------------------------------- #
# Code view
# --------------------------------------------------------------------------- #


def code_view(
    source: str,
    path: str,
    *,
    classifier: LocalCodeClassifier | None,
) -> tuple[bool, float, str]:
    """Return ``(flag, score, detail)``; ``(False, 0.0, "unavailable")`` without a classifier."""
    if classifier is None:
        return False, 0.0, "unavailable"
    try:
        raw = classifier.score(source, path)
    except ToolError as exc:
        return False, 0.0, f"unavailable: {exc.message}"
    try:
        score = float(raw)
    except (TypeError, ValueError):
        return False, 0.0, f"unavailable: {getattr(classifier, 'name', 'classifier')} returned a non-number"
    if score != score:  # NaN
        return False, 0.0, "unavailable: classifier returned NaN"
    score = max(0.0, min(1.0, score))
    return score >= CODE_FLAG_THRESHOLD, score, f"{getattr(classifier, 'name', 'classifier')}: {score:.3f}"


# --------------------------------------------------------------------------- #
# Ensemble
# --------------------------------------------------------------------------- #


def ensemble(ast_flag: bool, cfg_flag: bool, code_flag: bool) -> bool:
    """The Boolean absorption: ``(AST && CFG) || Code``."""
    return (bool(ast_flag) and bool(cfg_flag)) or bool(code_flag)


def rale(
    path: str,
    source: str,
    *,
    classifier: LocalCodeClassifier | None = None,
) -> RaleResult:
    """Run the three views and combine them.  Non-Python returns no flags."""
    if not path.endswith(_PYTHON_SUFFIXES):
        return RaleResult(path=path, function="", classifier="")
    ast_flag, ast_reasons, functions = ast_view(source, path)
    cfg_flag, cfg_reasons = cfg_view(source, path)
    code_flag, code_score, _detail = code_view(source, path, classifier=classifier)
    return RaleResult(
        path=path,
        function=functions[0] if functions else "",
        line=_first_definition_line(source),
        ast_flag=ast_flag,
        cfg_flag=cfg_flag,
        code_flag=code_flag,
        ast_reasons=ast_reasons,
        cfg_reasons=cfg_reasons,
        code_score=code_score,
        flagged=ensemble(ast_flag, cfg_flag, code_flag),
        classifier="" if classifier is None else str(getattr(classifier, "name", "classifier")),
    )


def _first_definition_line(source: str) -> int:
    """The line a reviewer should cite for a file-level RALE flag."""
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return 1
    lines = [
        node.lineno
        for node in ast.walk(tree)
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))
    ]
    return min(lines) if lines else 1


def _dotted(node: ast.AST) -> str:
    """Best-effort dotted name for a Name/Attribute/Subscript chain."""
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
