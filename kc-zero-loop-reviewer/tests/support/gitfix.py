"""Build real, throw-away git repositories for tests."""

from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass
from pathlib import Path

ENV = {
    "GIT_AUTHOR_NAME": "Test",
    "GIT_AUTHOR_EMAIL": "test@example.invalid",
    "GIT_COMMITTER_NAME": "Test",
    "GIT_COMMITTER_EMAIL": "test@example.invalid",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_SYSTEM": os.devnull,
}


def run(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", *args],
        cwd=str(cwd),
        capture_output=True,
        text=True,
        env={**os.environ, **ENV},
    )
    if proc.returncode != 0:
        raise AssertionError(f"git {' '.join(args)} failed: {proc.stderr}")
    return proc.stdout.strip()


@dataclass
class RepoFixture:
    root: Path

    def write(self, path: str, text: str) -> Path:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
        return target

    def write_bytes(self, path: str, data: bytes) -> Path:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        return target

    def commit(self, message: str, **files: str) -> str:
        for path, text in files.items():
            self.write(path.replace("__", "/"), text)
        run(self.root, "add", "-A")
        run(self.root, "commit", "-m", message)
        return run(self.root, "rev-parse", "HEAD")

    def checkout(self, ref: str) -> None:
        run(self.root, "checkout", "-q", ref)

    def head(self) -> str:
        return run(self.root, "rev-parse", "HEAD")

    def branch(self, name: str) -> None:
        run(self.root, "checkout", "-q", "-B", name)


def make_repo(tmp_path: Path, name: str = "repo") -> RepoFixture:
    root = Path(tmp_path) / name
    root.mkdir(parents=True, exist_ok=True)
    run(root, "init", "-q", "-b", "main")
    run(root, "config", "user.name", "Test")
    run(root, "config", "user.email", "test@example.invalid")
    return RepoFixture(root)
