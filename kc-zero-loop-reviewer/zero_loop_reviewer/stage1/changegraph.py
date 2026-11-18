"""Change-graph decomposition for massive pull requests.

The behavioural scanner batches files by size.  That preserves coverage but
mixes unrelated features and hides the edges between them.  This module builds
a conservative *file graph* from changed-file imports, test/source pairing and
shared paths, then returns connected components as change units.

It is deliberately not an LLM planner.  The graph is deterministic, auditable
and reproducible; a model may review a unit, but it does not decide where the
boundary is.
"""

from __future__ import annotations

import ast
import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable

from ..gitops import GitRepo
from ..models import DiffBundle


@dataclass(frozen=True)
class ChangeUnit:
    id: str
    title: str
    paths: tuple[str, ...]
    depends_on: tuple[str, ...] = ()
    related: tuple[str, ...] = ()


@dataclass
class ChangePlan:
    units: list[ChangeUnit] = field(default_factory=list)
    edges: list[tuple[str, str]] = field(default_factory=list)

    def unit_for(self, path: str) -> ChangeUnit | None:
        for unit in self.units:
            if path in unit.paths:
                return unit
        return None

    def unit_id_for(self, path: str) -> str:
        unit = self.unit_for(path)
        return unit.id if unit is not None else ""

    def summary(self) -> dict[str, object]:
        return {
            "units": len(self.units),
            "edges": len(self.edges),
            "files": sum(len(unit.paths) for unit in self.units),
        }


def build_change_plan(repo: GitRepo, diff: DiffBundle) -> ChangePlan:
    """Build connected components over the changed files' real relationships."""
    changed = [
        patch
        for patch in diff.files
        if not patch.is_deleted and not patch.is_binary and patch.path
    ]
    by_path = {patch.path: patch for patch in changed}
    paths = sorted(by_path)
    if not paths:
        return ChangePlan()

    sources: dict[str, str] = {}
    for path in paths:
        try:
            sources[path] = repo.show(diff.head_sha or "HEAD", path)
        except Exception:
            sources[path] = ""

    graph: dict[str, set[str]] = {path: set() for path in paths}
    for path in paths:
        for target in _imports(path, sources.get(path, ""), paths):
            if target != path and target in by_path:
                graph[path].add(target)
                graph[target].add(path)

    # Directory affinity is the fallback when a PR changes two sides of a
    # feature without a changed import between them.  It is intentionally
    # coarse: crates/kc-core, crates/kc-server, web/src, docs.
    by_feature: dict[str, list[str]] = {}
    for path in paths:
        key = _feature_key(path)
        if key:
            by_feature.setdefault(key, []).append(path)
    for members in by_feature.values():
        if len(members) < 2:
            continue
        anchor = members[0]
        for member in members[1:]:
            graph[anchor].add(member)
            graph[member].add(anchor)

    for test in paths:
        if not _is_test_path(test):
            continue
        stem = _stem(test)
        for source in paths:
            if source == test or _is_test_path(source):
                continue
            if stem and (stem in source or _stem(source) == stem):
                graph[test].add(source)
                graph[source].add(test)

    components = _components(paths, graph)
    split_components: list[list[str]] = []
    for component in components:
        split_components.extend(_split_component(component))
    unit_of: dict[str, str] = {}
    units: list[ChangeUnit] = []
    for index, component in enumerate(split_components, 1):
        title = _title(component)
        unit = ChangeUnit(
            id=f"u{index}-{_slug(title)}",
            title=title,
            paths=tuple(sorted(component)),
        )
        units.append(unit)
        for path in component:
            unit_of[path] = unit.id

    edges: set[tuple[str, str]] = set()
    for source, targets in graph.items():
        for target in targets:
            left, right = unit_of[source], unit_of[target]
            if left != right:
                edges.add(tuple(sorted((left, right))))
    related: dict[str, set[str]] = {unit.id: set() for unit in units}
    for left, right in edges:
        related[left].add(right)
        related[right].add(left)

    # Rebuild immutable units with the cross-unit edges attached.
    enriched = [
        ChangeUnit(
            id=unit.id,
            title=unit.title,
            paths=unit.paths,
            depends_on=tuple(sorted(related[unit.id])),
            related=tuple(sorted(related[unit.id])),
        )
        for unit in units
    ]
    enriched.sort(key=lambda unit: (-len(unit.paths), unit.title))
    return ChangePlan(units=enriched, edges=sorted(edges))


def _components(paths: list[str], graph: dict[str, set[str]]) -> list[list[str]]:
    seen: set[str] = set()
    out: list[list[str]] = []
    for path in paths:
        if path in seen:
            continue
        stack = [path]
        seen.add(path)
        component: list[str] = []
        while stack:
            current = stack.pop()
            component.append(current)
            for neighbour in graph.get(current, ()):
                if neighbour not in seen:
                    seen.add(neighbour)
                    stack.append(neighbour)
        out.append(component)
    out.sort(key=lambda group: (-len(group), sorted(group)[0]))
    return out


def _split_component(component: list[str], limit: int = 12) -> list[list[str]]:
    """Keep a huge connected component reviewable without losing its edges."""
    if len(component) <= limit:
        return [component]
    groups: dict[str, list[str]] = {}
    for path in component:
        groups.setdefault(_feature_key(path) or "root", []).append(path)
    out: list[list[str]] = []
    for members in groups.values():
        if len(members) <= limit:
            out.append(members)
            continue
        # A single feature is still too large: chunk by directory, not size,
        # so related files stay together.
        sub: dict[str, list[str]] = {}
        for path in members:
            parts = Path(path).parts
            if len(parts) <= 1:
                key = "root"
            elif len(parts) == 2:
                key = parts[0]
            else:
                key = "/".join(parts[:3])
            sub.setdefault(key, []).append(path)
        for group in sub.values():
            for start in range(0, len(group), limit):
                out.append(group[start : start + limit])
    out.sort(key=lambda group: (-len(group), sorted(group)[0]))
    return out


def _feature_key(path: str) -> str:
    parts = Path(path).parts
    if not parts:
        return ""
    if len(parts) >= 2 and parts[0] in ("crates", "packages", "apps"):
        return "/".join(parts[:2])
    if len(parts) >= 2 and parts[0] == "web" and parts[1] == "src":
        return "web/src"
    if len(parts) >= 2 and parts[0] in ("docs", "doc"):
        return "docs"
    if len(parts) == 1:
        return ""
    return parts[0]


def _title(paths: list[str]) -> str:
    if len(paths) == 1:
        return paths[0]
    parts = [Path(path).parts for path in paths]
    common: list[str] = []
    for column in zip(*parts):
        if len(set(column)) != 1:
            break
        common.append(column[0])
    if common:
        return "/".join(common)
    return "mixed-change"


def _slug(text: str) -> str:
    slug = re.sub(r"[^a-zA-Z0-9]+", "-", text.strip("/")).strip("-").lower()
    return slug[:48] or "root"


def _is_test_path(path: str) -> bool:
    low = path.lower()
    name = Path(low).name
    return (
        "/tests/" in low
        or "/test/" in low
        or low.startswith("tests/")
        or name.startswith("test_")
        or ".test." in low
        or ".spec." in low
        or name.endswith("_test.py")
        or name.endswith("_test.rs")
    )


def _stem(path: str) -> str:
    name = Path(path).name
    for suffix in (".py", ".pyi", ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"):
        if name.endswith(suffix):
            name = name[: -len(suffix)]
            break
    for prefix in ("test_",):
        if name.startswith(prefix):
            name = name[len(prefix) :]
    for suffix in ("_test", ".test", ".spec"):
        if name.endswith(suffix):
            name = name[: -len(suffix)]
    return name.lower()


def _imports(path: str, source: str, paths: Iterable[str]) -> set[str]:
    if not source:
        return set()
    suffix = Path(path).suffix.lower()
    if suffix in (".py", ".pyi"):
        return _python_imports(path, source, paths)
    if suffix in (".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".vue", ".svelte"):
        return _js_imports(path, source, paths)
    if suffix == ".rs":
        return _rust_imports(path, source, paths)
    return set()


def _resolve(path: str, paths: Iterable[str]) -> str | None:
    wanted = path.replace("\\", "/").lstrip("./")
    for candidate in paths:
        if candidate == wanted or candidate.endswith("/" + wanted):
            return candidate
    return None


def _python_imports(path: str, source: str, paths: Iterable[str]) -> set[str]:
    out: set[str] = set()
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return out
    base = Path(path).parent
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                target = alias.name.replace(".", "/")
                for candidate in (f"{target}.py", f"{target}/__init__.py"):
                    resolved = _resolve(candidate, paths)
                    if resolved:
                        out.add(resolved)
        elif isinstance(node, ast.ImportFrom):
            module = node.module or ""
            if node.level:
                prefix = base
                for _ in range(max(0, node.level - 1)):
                    prefix = prefix.parent
                relative = prefix / module.replace(".", "/") if module else prefix
                for alias in node.names:
                    for candidate in (
                        str(relative / f"{alias.name}.py"),
                        str(relative / alias.name / "__init__.py"),
                        str(relative / "__init__.py"),
                    ):
                        resolved = _resolve(candidate, paths)
                        if resolved:
                            out.add(resolved)
            else:
                target = module.replace(".", "/")
                for candidate in (f"{target}.py", f"{target}/__init__.py"):
                    resolved = _resolve(candidate, paths)
                    if resolved:
                        out.add(resolved)
                for alias in node.names:
                    for candidate in (f"{target}/{alias.name}.py",):
                        resolved = _resolve(candidate, paths)
                        if resolved:
                            out.add(resolved)
    return out


_JS_IMPORT = re.compile(r"""(?:from\s+|require\s*\(\s*|import\s*\(\s*)['"]([^'"]+)['"]""")


def _js_imports(path: str, source: str, paths: Iterable[str]) -> set[str]:
    out: set[str] = set()
    base = Path(path).parent
    for match in _JS_IMPORT.finditer(source):
        target = match.group(1)
        if not target.startswith("."):
            continue
        relative = (base / target).as_posix()
        candidates = [relative]
        for suffix in (".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".vue", ".svelte"):
            candidates.append(relative + suffix)
            candidates.append(f"{relative}/index{suffix}")
        for candidate in candidates:
            resolved = _resolve(candidate, paths)
            if resolved:
                out.add(resolved)
                break
    return out


_RUST_USE = re.compile(r"^\s*(?:pub\s+)?use\s+(?:crate|super|self)::([A-Za-z0-9_:{}, ]+)", re.MULTILINE)
_RUST_MOD = re.compile(r"^\s*(?:pub\s+)?mod\s+([A-Za-z0-9_]+)\s*;", re.MULTILINE)


def _rust_imports(path: str, source: str, paths: Iterable[str]) -> set[str]:
    out: set[str] = set()
    names: set[str] = set()
    for match in _RUST_USE.finditer(source):
        body = match.group(1)
        for part in re.split(r"[,{}\s]+", body):
            part = part.strip()
            if part and part not in ("self", "super", "crate"):
                names.add(part)
    for match in _RUST_MOD.finditer(source):
        names.add(match.group(1))
    for name in names:
        for candidate in paths:
            if not candidate.endswith(".rs"):
                continue
            if candidate.endswith(f"/{name}.rs") or candidate.endswith(f"/{name}/mod.rs"):
                out.add(candidate)
    return out
