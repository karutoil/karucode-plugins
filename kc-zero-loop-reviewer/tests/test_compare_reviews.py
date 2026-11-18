"""The CodeRabbit comparison harness parses and matches without network."""

from __future__ import annotations

import json

from tools.compare_reviews import (
    coderabbit_issues,
    compare,
    report,
    verdict_issues,
)


def _write(tmp_path, name, value):
    path = tmp_path / name
    path.write_text(json.dumps(value), encoding="utf-8")
    return path


def test_coderabbit_comments_and_zero_loop_findings_match(tmp_path) -> None:
    coderabbit = _write(
        tmp_path,
        "cr.json",
        [
            {
                "path": "src/api.py",
                "line": 12,
                "body": (
                    "<!-- coderabbit-ai-generated:v1 -->\n\n"
                    "**Major** · **Data Integrity & Integration** · **Quick win**\n\n"
                    "**Drop the stale cache key when the schema changes.**\n\n"
                    "The cached map still contains the old key."
                ),
            },
            {
                "path": "src/other.py",
                "line": 4,
                "body": (
                    "<!-- coderabbit-ai-generated:v1 -->\n\n"
                    "**Minor** · **Maintainability** · **Quick win**\n\n"
                    "**Rename this local for clarity.**\n"
                ),
            },
        ],
    )
    ours = _write(
        tmp_path,
        "ours.json",
        {
            "findings": [
                {
                    "id": "3-abc",
                    "title": "stale cache key after schema change",
                    "root_cause": "The old key remains in the cached map.",
                    "severity": 3,
                    "category": "logic",
                    "cited_sites": [{"path": "src/api.py", "line": 12}],
                }
            ]
        },
    )
    cr = coderabbit_issues(coderabbit)
    assert len(cr) == 2
    assert cr[0].severity == "major"
    assert "stale cache" in cr[0].title.lower()
    comparison = compare(cr, verdict_issues(ours))
    assert len(comparison.matches) == 1
    assert len(comparison.unmatched_coderabbit()) == 1
    text = report(comparison)
    assert "matched: **1**" in text
    assert "CodeRabbit-only: **1**" in text


def test_duplicate_coderabbit_comments_are_collapsed(tmp_path) -> None:
    body = (
        "<!-- coderabbit-ai-generated:v1 -->\n\n"
        "**Major** · **Correctness** · **Quick win**\n\n"
        "**The same title.**\n"
    )
    path = _write(
        tmp_path,
        "dupes.json",
        [
            {"path": "src/a.py", "line": 3, "body": body},
            {"path": "src/a.py", "line": 3, "body": body},
        ],
    )
    assert len(coderabbit_issues(path)) == 1
