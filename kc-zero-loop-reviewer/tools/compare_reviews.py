#!/usr/bin/env python3
"""Compare a CodeRabbit review export with a zero-loop verdict.

This is an adjudication aid, not an automatic winner.  CodeRabbit publishes a
lot of plausible-but-unverified comments; zero-loop publishes only verified
findings.  Counting comments favours CodeRabbit.  The useful metrics are:

* matching issues both reviewers found,
* issues only one found,
* the subset a human labels valid,
* coverage and false positives.

Input files:

* CodeRabbit: the JSON array from
  ``gh api repos/OWNER/REPO/pulls/N/comments`` (or newline-delimited objects).
* zero-loop: the ``--json`` verdict from ``zero-loop-reviewer review``.
* labels (optional): ``{"cr-1": {"valid": true, "note": "..."}, ...}``.

The report deliberately prints unmatched items with enough context for a
human to label them.  Do not turn this into a leaderboard before labelling.
"""

from __future__ import annotations

import argparse
import difflib
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable


@dataclass
class Issue:
    id: str
    source: str
    path: str
    line: int | None
    severity: str
    category: str
    title: str
    body: str

    @property
    def key(self) -> tuple[str, str]:
        return (self.path, _norm(self.title))


@dataclass
class Comparison:
    coderabbit: list[Issue] = field(default_factory=list)
    ours: list[Issue] = field(default_factory=list)
    matches: list[tuple[Issue, Issue, float]] = field(default_factory=list)
    labels: dict[str, dict[str, Any]] = field(default_factory=dict)

    def unmatched_coderabbit(self) -> list[Issue]:
        matched = {id(cr) for cr, _, _ in self.matches}
        return [issue for issue in self.coderabbit if id(issue) not in matched]

    def unmatched_ours(self) -> list[Issue]:
        matched = {id(ours) for _, ours, _ in self.matches}
        return [issue for issue in self.ours if id(issue) not in matched]

    def valid(self, issues: Iterable[Issue]) -> int:
        count = 0
        for issue in issues:
            label = self.labels.get(issue.id, {})
            if label.get("valid") is True:
                count += 1
        return count

    def summary(self) -> dict[str, int]:
        return {
            "coderabbit": len(self.coderabbit),
            "ours": len(self.ours),
            "matched": len(self.matches),
            "coderabbit_only": len(self.unmatched_coderabbit()),
            "ours_only": len(self.unmatched_ours()),
            "coderabbit_valid": self.valid(self.coderabbit),
            "ours_valid": self.valid(self.ours),
        }


def _norm(text: str) -> str:
    text = re.sub(r"<!--.*?-->", " ", text, flags=re.S)
    text = re.sub(r"<[^>]+>", " ", text)
    text = text.lower()
    text = re.sub(r"[^a-z0-9]+", " ", text)
    return " ".join(text.split())


def _tokens(text: str) -> set[str]:
    return set(_norm(text).split())


def _line(record: dict[str, Any]) -> int | None:
    for key in ("line", "original_line", "start_line"):
        value = record.get(key)
        if isinstance(value, int) and value > 0:
            return value
    return None


def _coderabbit_title(body: str) -> str:
    lines = [line.strip() for line in body.splitlines() if line.strip()]
    for line in lines:
        if "coderabbit-ai-generated" in line or "<picture>" in line:
            continue
        if not line.startswith("**"):
            continue
        text = line.strip("* ").strip()
        if not text or any(word in text for word in ("Critical", "Major", "Minor", "Nitpick", "Trivial")):
            continue
        if text in {"Quick win", "Heavy lift"}:
            continue
        if re.fullmatch(r"[A-Za-z &/+-]+", text) and len(text.split()) <= 3:
            continue
        return text[:300]
    # Fall back to the first sentence.
    first = re.sub(r"<[^>]+>", " ", body)
    first = re.sub(r"<!--.*?-->", " ", first, flags=re.S)
    first = re.sub(r"\*\*", "", first)
    first = " ".join(first.split())
    return first[:300]


def _severity(body: str) -> str:
    match = re.search(r"\b(Critical|Major|Minor|Nitpick|Trivial)\b", body)
    return match.group(1).lower() if match else "unknown"


def _category(body: str) -> str:
    match = re.search(r"·\s*\*\*([^*]+)\*\*\s*·", body)
    return match.group(1).strip() if match else ""


def load_records(path: Path) -> list[dict[str, Any]]:
    text = path.read_text(encoding="utf-8")
    stripped = text.strip()
    if not stripped:
        return []
    if stripped.startswith("["):
        value = json.loads(stripped)
        return value if isinstance(value, list) else [value]
    records = []
    for line in text.splitlines():
        line = line.strip()
        if line:
            records.append(json.loads(line))
    return records


def coderabbit_issues(path: Path, head: str = "") -> list[Issue]:
    issues: list[Issue] = []
    seen: set[str] = set()
    for index, record in enumerate(load_records(path), 1):
        login = str((record.get("user") or {}).get("login") or "")
        if login and login not in ("coderabbitai", "coderabbitai[bot]"):
            continue
        commit = str(record.get("commit_id") or "")
        if head and commit and not commit.startswith(head):
            continue
        body = str(record.get("body") or "")
        if "coderabbit-ai-generated" not in body:
            continue
        title = _coderabbit_title(body)
        if not title:
            continue
        issue = Issue(
            id=f"cr-{index}",
            source="coderabbit",
            path=str(record.get("path") or ""),
            line=_line(record),
            severity=_severity(body),
            category=_category(body),
            title=title,
            body=body,
        )
        identity = f"{issue.path}:{issue.line}:{_norm(issue.title)}"
        if identity in seen:
            continue
        seen.add(identity)
        issues.append(issue)
    return issues


def verdict_issues(path: Path) -> list[Issue]:
    data = json.loads(path.read_text(encoding="utf-8"))
    issues: list[Issue] = []
    for index, finding in enumerate(data.get("findings", []), 1):
        sites = finding.get("cited_sites") or []
        site = sites[0] if sites else {}
        title = str(finding.get("title") or "")
        root = str(finding.get("root_cause") or "")
        issues.append(
            Issue(
                id=str(finding.get("id") or f"ours-{index}"),
                source="ours",
                path=str(site.get("path") or ""),
                line=site.get("line") if isinstance(site.get("line"), int) else None,
                severity=str(finding.get("severity") or ""),
                category=str(finding.get("category") or ""),
                title=title,
                body=root,
            )
        )
    return issues


def _similarity(left: Issue, right: Issue) -> float:
    if left.path and right.path and left.path != right.path:
        return 0.0
    title = difflib.SequenceMatcher(None, _norm(left.title), _norm(right.title)).ratio()
    body = difflib.SequenceMatcher(None, _norm(left.body[:1200]), _norm(right.body[:1200])).ratio()
    token = len(_tokens(left.title + " " + left.body) & _tokens(right.title + " " + right.body))
    token_ratio = token / max(1, len(_tokens(left.title) | _tokens(right.title)))
    close = 0.15 if left.line is not None and right.line is not None and abs(left.line - right.line) <= 5 else 0.0
    return max(title * 0.6 + body * 0.2 + token_ratio * 0.2 + close, title, token_ratio)


def compare(coderabbit: list[Issue], ours: list[Issue], labels: dict[str, dict[str, Any]] | None = None) -> Comparison:
    comparison = Comparison(coderabbit=list(coderabbit), ours=list(ours), labels=labels or {})
    used_ours: set[int] = set()
    for cr in comparison.coderabbit:
        best_score = 0.0
        best_candidate: Issue | None = None
        best_index: int | None = None
        for index, candidate in enumerate(comparison.ours):
            if index in used_ours:
                continue
            score = _similarity(cr, candidate)
            if score > best_score:
                best_score, best_candidate, best_index = score, candidate, index
        if best_candidate is not None and best_index is not None and best_score >= 0.42:
            comparison.matches.append((cr, best_candidate, best_score))
            used_ours.add(best_index)
    return comparison


def _table(issues: list[Issue], labels: dict[str, dict[str, Any]]) -> list[str]:
    lines = [
        "| id | severity | file:line | title | label |",
        "| --- | --- | --- | --- | --- |",
    ]
    for issue in issues:
        label = labels.get(issue.id, {})
        verdict = "valid" if label.get("valid") is True else ("invalid" if label.get("valid") is False else "unlabelled")
        note = str(label.get("note") or "").replace("|", "\\|")
        lines.append(
            f"| {issue.id} | {issue.severity} | `{issue.path}:{issue.line or '?'}` | "
            f"{issue.title[:160].replace('|', '\\|')} | {verdict}{(' — ' + note) if note else ''} |"
        )
    return lines


def report(comparison: Comparison) -> str:
    summary = comparison.summary()
    lines = [
        "# CodeRabbit vs zero-loop",
        "",
        f"- CodeRabbit issues: **{summary['coderabbit']}** "
        f"({summary['coderabbit_valid']} labelled valid)",
        f"- zero-loop findings: **{summary['ours']}** "
        f"({summary['ours_valid']} labelled valid)",
        f"- matched: **{summary['matched']}**",
        f"- CodeRabbit-only: **{summary['coderabbit_only']}**",
        f"- zero-loop-only: **{summary['ours_only']}**",
        "",
        "> Counts are not a quality score. Label the unmatched items before drawing a conclusion.",
        "",
        "## Matched",
        "",
    ]
    if comparison.matches:
        for cr, ours, score in comparison.matches:
            lines.append(
                f"- `{cr.path}:{cr.line or '?'}` — **{cr.title}** "
                f"↔ **{ours.title}** (match {score:.2f})"
            )
    else:
        lines.append("_No matched issues._")
    lines.extend(["", "## CodeRabbit-only", ""])
    cr_only = comparison.unmatched_coderabbit()
    lines.extend(_table(cr_only, comparison.labels) if cr_only else ["_None._"])
    lines.extend(["", "## zero-loop-only", ""])
    ours_only = comparison.unmatched_ours()
    lines.extend(_table(ours_only, comparison.labels) if ours_only else ["_None._"])
    lines.extend(
        [
            "",
            "## How to label",
            "",
            "Set `valid` to `true` or `false` on each id in the labels JSON, then rerun. "
            "A reviewer comment that is technically true but irrelevant to the change is invalid. "
            "A zero-loop finding that is true but not mechanically verified is still a miss until proven.",
        ]
    )
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Compare CodeRabbit comments with a zero-loop verdict")
    parser.add_argument("--coderabbit", required=True, help="CodeRabbit comments JSON export")
    parser.add_argument("--verdict", required=True, help="zero-loop --json verdict")
    parser.add_argument("--head", default="", help="only comments whose commit_id starts with this SHA")
    parser.add_argument("--labels", help="optional issue labels JSON")
    parser.add_argument("--out", help="write the markdown report here")
    args = parser.parse_args(argv)
    labels = json.loads(Path(args.labels).read_text(encoding="utf-8")) if args.labels else {}
    comparison = compare(
        coderabbit_issues(Path(args.coderabbit), head=args.head),
        verdict_issues(Path(args.verdict)),
        labels,
    )
    text = report(comparison)
    if args.out:
        Path(args.out).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
