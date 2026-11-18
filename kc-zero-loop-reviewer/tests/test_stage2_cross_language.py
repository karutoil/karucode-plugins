"""Cross-language Stage 2 regressions: rules for Rust, TypeScript and Actions.

The CI smoke test on a workflow-only PR exposed the real problem: the shipped
rules were Python-only, so a Rust/TS repository had no patch-caused analyzer
trace to verify.  These tests exercise the shipped rule directory itself.
"""

from __future__ import annotations

import shutil

import pytest

from tests.support.gitfix import make_repo
from zero_loop_reviewer.config import load_settings
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.stage2 import sast

HAVE_SEMGREP = shutil.which("semgrep") is not None


@pytest.mark.integration
@pytest.mark.skipif(not HAVE_SEMGREP, reason="semgrep is not installed")
def test_the_shipped_rules_find_rust_typescript_and_actions_defects(tmp_path) -> None:
    fixture = make_repo(tmp_path, "cross-language")
    fixture.commit(
        "base",
        **{
            "src/lib.rs": "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            "web/app.ts": "export const add = (a: number, b: number) => a + b;\n",
            ".github/workflows/x.yml": "name: x\non: push\njobs: {}\n",
        },
    )
    head = fixture.commit(
        "head",
        **{
            "src/lib.rs": "pub fn read(v: Option<i32>) -> i32 { v.unwrap() }\n",
            "web/app.ts": "export const run = (x: string) => eval(x);\n",
            ".github/workflows/x.yml": "name: x\non: push\npermissions: write-all\njobs: {}\n",
        },
    )
    repo = GitRepo(fixture.root)
    diff = repo.diff("HEAD~1", head)
    outcome = sast.run_semgrep(fixture.root, diff, load_settings({}, {}))
    assert outcome.status.value == "completed", outcome.detail
    rules = {finding.rule_id for finding in outcome.findings}
    assert {"rust-unwrap", "js-eval", "actions-write-all"} <= rules
    assert all(finding.patch_caused for finding in outcome.findings)
