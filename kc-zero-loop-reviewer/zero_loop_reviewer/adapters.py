"""Language adapters for probe synthesis and sandbox execution.

The built-in adapters cover Python, Rust, Go and JavaScript/TypeScript.  A
repository in any other language can provide an adapter through
``probe.adapters`` (JSON) without changing the reviewer:

```toml
probe.adapters = '''
{
  "ruby": {
    "extensions": [".rb"],
    "test_suffix": "_test.rb",
    "test_path": "test/zl_probe_{name}.rb",
    "framework": "ruby",
    "image": "docker.io/library/ruby:3.3-alpine",
    "command": ["ruby", "{test_path}"],
    "test_marker": "def test_",
    "assertion_marker": "assert"
  }
}
'''
```

The adapter supplies the facts the reviewer cannot infer: where a generated
test goes, which image has the toolchain, and the exact command that runs one
test file.  Without a matching adapter the reviewer stays static and sends the
candidate to human review; it never guesses a language.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Iterable


@dataclass(frozen=True)
class ProbeAdapter:
    name: str
    language: str
    extensions: tuple[str, ...]
    test_suffix: str
    test_path: str
    framework: str
    image: str = ""
    command: tuple[str, ...] = ()
    test_marker: str = ""
    assertion_marker: str = ""
    syntax_command: tuple[str, ...] = ()
    builtin: bool = False

    def accepts(self, path: str) -> bool:
        return Path(path).suffix.lower() in self.extensions

    def path_for(self, stem: str) -> str:
        return self.test_path.replace("{name}", stem)


def _adapter(
    name: str,
    language: str,
    extensions: tuple[str, ...],
    test_suffix: str,
    test_path: str,
    framework: str,
    *,
    image: str = "",
    command: tuple[str, ...] = (),
    test_marker: str = "",
    assertion_marker: str = "",
    syntax_command: tuple[str, ...] = (),
) -> ProbeAdapter:
    return ProbeAdapter(
        name=name,
        language=language,
        extensions=extensions,
        test_suffix=test_suffix,
        test_path=test_path,
        framework=framework,
        image=image,
        command=command,
        test_marker=test_marker,
        assertion_marker=assertion_marker,
        syntax_command=syntax_command,
        builtin=True,
    )


BUILTIN_ADAPTERS: tuple[ProbeAdapter, ...] = (
    _adapter(
        "python",
        "python",
        (".py", ".pyi"),
        ".py",
        ".probe/test_{name}.py",
        "pytest",
        test_marker="def test_",
        assertion_marker="assert",
        syntax_command=("python3", "-m", "compileall", "-q"),
    ),
    _adapter(
        "rust",
        "rust",
        (".rs",),
        ".rs",
        "tests/zl_probe_{name}.rs",
        "cargo",
        image="docker.io/library/rust:1.89-slim",
        command=("cargo", "test", "--offline", "--test", "{test_stem}", "--", "--nocapture"),
        test_marker="#[test]",
        assertion_marker="assert",
        syntax_command=("rustfmt", "--emit", "stdout", "--quiet"),
    ),
    _adapter(
        "go",
        "go",
        (".go",),
        "_test.go",
        "{name}_test.go",
        "go",
        image="docker.io/library/golang:1.23-alpine",
        command=("go", "test", "./{test_dir}", "-run", "Test"),
        test_marker="func Test",
        assertion_marker="t.Fatalf",
        syntax_command=("gofmt", "-d"),
    ),
    _adapter(
        "typescript",
        "typescript",
        (".ts", ".tsx"),
        ".test.ts",
        "zl_probe_{name}.test.ts",
        "node",
        image="docker.io/library/node:22-alpine",
        command=("node", "--experimental-strip-types", "--test", "{test_path}"),
        test_marker="test(",
        assertion_marker="assert",
    ),
    _adapter(
        "javascript",
        "javascript",
        (".js", ".jsx", ".mjs", ".cjs"),
        ".test.js",
        "zl_probe_{name}.test.js",
        "node",
        image="docker.io/library/node:22-alpine",
        command=("node", "--test", "{test_path}"),
        test_marker="test(",
        assertion_marker="assert",
    ),
)

BUILTIN_BY_NAME = {adapter.name: adapter for adapter in BUILTIN_ADAPTERS}
EXTENSIONS = tuple(ext for adapter in BUILTIN_ADAPTERS for ext in adapter.extensions)


def adapters_map(adapters: Iterable[ProbeAdapter]) -> dict[str, ProbeAdapter]:
    out: dict[str, ProbeAdapter] = {}
    for adapter in adapters:
        out[adapter.name] = adapter
        out.setdefault(adapter.framework, adapter)
    return out


def adapter_for(path: str, adapters: Iterable[ProbeAdapter] = BUILTIN_ADAPTERS) -> ProbeAdapter | None:
    """The most specific adapter for a path.

    Extensions are unique by construction; if a custom adapter duplicates one,
    the custom adapter wins because it is inserted after the built-ins.
    """
    suffix = Path(path).suffix.lower()
    found: ProbeAdapter | None = None
    for adapter in adapters:
        if suffix in adapter.extensions:
            found = adapter
    return found


def parse_adapters(raw: str | Iterable[dict[str, Any]] | None) -> tuple[ProbeAdapter, ...]:
    """Parse ``probe.adapters`` over built-ins; malformed input fails loudly."""
    if not raw:
        return BUILTIN_ADAPTERS
    if isinstance(raw, str):
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise ValueError(f"probe.adapters is not valid JSON: {exc}") from exc
    else:
        value = list(raw)
    if isinstance(value, dict):
        value = [{"name": name, **(body or {})} for name, body in value.items()]
    if not isinstance(value, list):
        raise ValueError("probe.adapters must be a JSON object or array")
    out = dict(BUILTIN_BY_NAME)
    for index, item in enumerate(value):
        if not isinstance(item, dict):
            raise ValueError(f"probe.adapters[{index}] is not an object")
        name = str(item.get("name") or "").strip()
        if not name or not re.fullmatch(r"[a-z][a-z0-9_-]*", name):
            raise ValueError(f"probe.adapters[{index}] has an invalid name {name!r}")
        extensions = item.get("extensions") or []
        if not isinstance(extensions, list) or not extensions:
            raise ValueError(f"probe adapter {name!r} needs a non-empty extensions list")
        cleaned = []
        for extension in extensions:
            text = str(extension).strip().lower()
            if not text.startswith(".") or "/" in text or ".." in text:
                raise ValueError(f"probe adapter {name!r} has an invalid extension {extension!r}")
            cleaned.append(text)
        test_path = str(item.get("test_path") or "").strip()
        if not test_path or test_path.startswith("/") or ".." in Path(test_path).parts:
            raise ValueError(f"probe adapter {name!r} needs a safe test_path")
        command = item.get("command") or []
        if not isinstance(command, list) or not all(isinstance(part, str) and part for part in command):
            raise ValueError(f"probe adapter {name!r} needs a command as a non-empty string array")
        image = str(item.get("image") or "").strip()
        framework = str(item.get("framework") or name).strip()
        language = str(item.get("language") or name).strip()
        test_marker = str(item.get("test_marker") or "")
        assertion_marker = str(item.get("assertion_marker") or "")
        if not test_marker or not assertion_marker:
            raise ValueError(
                f"probe adapter {name!r} needs test_marker and assertion_marker so a generated test can be validated"
            )
        if Path(test_path).suffix.lower() not in cleaned:
            raise ValueError(
                f"probe adapter {name!r} test_path suffix must be one of its extensions: {cleaned}"
            )
        out[name] = ProbeAdapter(
            name=name,
            language=language,
            extensions=tuple(cleaned),
            test_suffix=str(item.get("test_suffix") or "").strip(),
            test_path=test_path,
            framework=framework,
            image=image,
            command=tuple(command),
            test_marker=test_marker,
            assertion_marker=assertion_marker,
            syntax_command=tuple(item.get("syntax_command") or ()),
        )
    return tuple(out.values())
