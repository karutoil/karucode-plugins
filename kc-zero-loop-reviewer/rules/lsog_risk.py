#!/usr/bin/env python3
"""Deterministic LSOG risk counter used by ObliVul.

The counter answers one question: *how many dangerous sinks in the enclosing
function can be reached without passing a guard?*  It is deliberately narrow:

* a sink is a line in the spec (the candidate's cited site plus every taint or
  lifetime sink the CPG frame found);
* a guard is an ``if``/``assert`` (or an inline ``if`` wrapping the sink) whose
  test names a value used at the sink and whose body raises or returns, or a
  sanitizer call on that value before the sink;
* the risk is the number of unguarded sinks, not a probability.

ObliVul runs this on a pristine copy, on a copy with a guard inserted, and on a
copy with a guard removed.  A binary reachability flag would make the two
directions mutually exclusive; a count can move down for the positive
intervention and up for the negative one, which is exactly what bidirectional
validation needs.

Usage: ``lsog_risk.py --spec /abs/spec.json``, prints ``{"risk": n, ...}``.
"""

from __future__ import annotations

import argparse
import ast
import json
import sys
from pathlib import Path
from typing import Any

SANITIZERS = {
    "escape",
    "quote",
    "join",
    "safe_load",
    "realpath",
    "normpath",
    "int",
    "float",
    "bool",
    "parameterize",
    "sanitize",
    "validate",
    "clean",
}


def _names(node: ast.AST) -> set[str]:
    return {n.id for n in ast.walk(node) if isinstance(n, ast.Name)}


def _terminates(body: list[ast.stmt]) -> bool:
    for stmt in body:
        if isinstance(stmt, (ast.Raise, ast.Return)):
            return True
        if isinstance(stmt, ast.If) and _terminates(stmt.body):
            return True
    return False


def _function_for(tree: ast.Module, line: int) -> ast.AST | None:
    best: ast.AST | None = None
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            end = getattr(node, "end_lineno", node.lineno)
            if node.lineno <= line <= end:
                if best is None or node.lineno > getattr(best, "lineno", 0):
                    best = node
    return best or tree


def _sink_names(tree: ast.Module, line: int) -> set[str]:
    for node in ast.walk(tree):
        if isinstance(node, ast.stmt) and node.lineno <= line <= getattr(node, "end_lineno", node.lineno):
            return _names(node)
    return set()


def _guarded(tree: ast.Module, function: ast.AST, sink: int) -> bool:
    sink_names = _sink_names(tree, sink)
    for node in ast.walk(function):
        if isinstance(node, (ast.If, ast.Assert)):
            test_names = _names(node.test)
            body_end = getattr(node, "end_lineno", node.lineno)
            if node.lineno <= sink <= body_end and test_names & sink_names:
                # Inline guard wrapping the sink itself.
                return True
            if body_end < sink and test_names & sink_names and _terminates(node.body):
                return True
        if isinstance(node, (ast.Assign, ast.AnnAssign)):
            value = getattr(node, "value", None)
            if value is None or getattr(node, "end_lineno", node.lineno) >= sink:
                continue
            targets = _names(ast.Module(body=[node], type_ignores=[]))
            if not (targets & sink_names):
                continue
            if isinstance(value, ast.Call) and _call_name(value) in SANITIZERS:
                return True
    return False


def _call_name(call: ast.Call) -> str:
    func = call.func
    if isinstance(func, ast.Name):
        return func.id
    if isinstance(func, ast.Attribute):
        return func.attr
    return ""


def _analyze(path: Path, sinks: list[int]) -> dict[str, Any]:
    source = path.read_text(encoding="utf-8")
    try:
        tree = ast.parse(source)
    except SyntaxError as exc:
        return {"risk": 0, "error": f"the sandbox copy does not parse: {exc.msg}"}
    counts: list[dict[str, Any]] = []
    for line in sorted(set(int(value) for value in sinks if int(value) > 0)):
        function = _function_for(tree, line)
        guarded = _guarded(tree, function, line)
        counts.append({"line": line, "guarded": guarded})
    risk = sum(0 if item["guarded"] else 1 for item in counts)
    return {"risk": risk, "sinks": counts}


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--spec", required=True)
    args = parser.parse_args(argv[1:])
    spec = json.loads(Path(args.spec).read_text(encoding="utf-8"))
    path = Path(str(spec.get("path", "")))
    if not path.is_file():
        print(json.dumps({"risk": 0, "error": f"file not found: {path}"}))
        return 0
    result = _analyze(path, list(spec.get("sinks") or []))
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
