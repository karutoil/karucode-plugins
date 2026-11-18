#!/usr/bin/env python3
"""Measure the pipeline on the annotated corpus.

Two modes:

* ``--provider scripted`` (default) — the candidate, probe and repair are taken
  from the case annotation (oracle generation).  This measures the
  deterministic verification/emission half: real git, real Semgrep, real probe
  execution, real CIVER, real receipts.
* ``--provider live`` — the real prompts and the configured endpoint generate
  the candidates, probes and repairs.  This is the only mode that measures
  model recall, and it refuses to run without an endpoint.

The report prints both the interception rate over buggy cases and the
false-positive count over clean cases.  No number is claimed for a mode that
did not run.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
if str(ROOT / "eval") not in sys.path:
    sys.path.insert(0, str(ROOT / "eval"))

from corpus import CASES  # noqa: E402
from tests.support.gitfix import make_repo  # noqa: E402
from zero_loop_reviewer.config import load_settings  # noqa: E402
from zero_loop_reviewer.llm.client import build_llm  # noqa: E402
from zero_loop_reviewer.models import (  # noqa: E402
    ModelAnswer,
    ReviewVerdict,
    TokenUsage,
)
from zero_loop_reviewer.pipeline import ReviewRequest, Reviewer  # noqa: E402


class OracleLLM:
    """Answers every schema from the case annotation, regardless of call order."""

    def __init__(self, answers: dict[str, Any]):
        self.answers = answers
        self.calls: list[str] = []

    def complete(self, stage, prompt, schema, *, stage_model=None, temperature=None, instance="reviewer"):
        self.calls.append(schema.__name__)
        if schema.__name__ not in self.answers:
            raise AssertionError(f"oracle has no answer for {schema.__name__} (stage {stage})")
        value = schema.model_validate(self.answers[schema.__name__])
        return ModelAnswer(value=value, raw=value.model_dump_json(), model="oracle", usage=TokenUsage(), attempts=1)


def _patch_for(case: dict[str, Any]) -> str:
    chunks: list[str] = []
    for path, fixed in case.get("fix", {}).items():
        old = case["head"].get(path, "")
        chunk = "\n".join(
            difflib.unified_diff(
                old.splitlines(),
                fixed.splitlines(),
                fromfile=f"a/{path}",
                tofile=f"b/{path}",
                lineterm="",
            )
        )
        if chunk:
            chunks.append(chunk + "\n")
    return "\n".join(chunks)


def _answers(case: dict[str, Any]) -> dict[str, Any]:
    kind = case["kind"]
    candidate = case.get("candidate") or {}
    # A "fixed" case carries a real candidate and probe (the PR fixes a bug),
    # but the head behaviour is correct: it must be approved, not blocked.
    has_candidate = kind in ("buggy", "fixed")
    p = 0.93 if kind == "buggy" else 0.03
    answers: dict[str, Any] = {
        "TriagePlan": {
            "focus": [
                {
                    "path": (
                        case["ground_truth"][0]["path"]
                        if case["ground_truth"]
                        else candidate.get("path", next(iter(case["head"])))
                    ),
                    "function": "",
                    "reason": "changed code under review",
                    "priority": 2,
                }
            ],
            "search_symbols": [],
            "risk_hypotheses": [],
            "budget_note": "",
        },
        "SecSemScan": {
            "monologue": "No untrusted value reaches a dangerous sink on a changed line.",
            "cwe": "",
            "verdict": "SAFE",
            "cited_lines": [],
            "taint": [],
            "lifetime": [],
            "confidence": 0.6,
        },
        "MciScan": {"claims": [], "summary": "no behavioural claims"},
        "BehavioralCandidates": {
            "candidates": (
                [
                    {
                        "title": candidate["title"],
                        "category": candidate["category"],
                        "severity": candidate["severity"],
                        "cited_sites": [
                            {"path": candidate["path"], "line": candidate["line"], "symbol": ""}
                        ],
                        "root_cause": candidate["root_cause"],
                        "suspected_trigger": candidate.get("trigger", ""),
                    }
                ]
                if has_candidate
                else []
            )
        },
        "ProbeSpec": {
            "test_path": case.get("probe_path", ".zero/test_probe.py"),
            "content": case.get("probe", "def test_noop():\n    assert True\n"),
            "target_symbols": [],
            "framework": "pytest",
            "rationale": "annotated reproduction",
        },
        "FallbackVerdict": {"verdict": "uncertain", "confidence": 0.3, "rationale": "the probe is the evidence"},
        "PatchProposal": {
            "patch": _patch_for(case) or "--- a/none\n+++ b/none\n@@ -0,0 +1 @@\n+# no repair\n",
            "files_changed": list(case.get("fix", {})),
            "rationale": "annotated minimal repair",
        },
        "RiskElicitation": {
            "defect_present": kind == "buggy",
            "category": candidate.get("category", "correctness"),
            "severity_score": int(candidate.get("severity", 0)),
            "failure_probability": p,
            "cited_sites": (
                [{"path": candidate["path"], "line": candidate["line"]}] if kind == "buggy" else []
            ),
            "root_cause": candidate.get("root_cause", "no defect found in the changed lines"),
        },
        "MonitorVote": {"score": p, "rationale": "independent assessment"},
    }
    if kind != "buggy":
        # Clean and fixed cases must not elicit a defect in the head.
        answers["RiskElicitation"]["defect_present"] = False
        answers["RiskElicitation"]["root_cause"] = "no defect found in the changed lines"
    return answers


def _matches(finding, ground_truth: list[dict[str, Any]]) -> bool:
    if finding.redacted:
        # A redacted security finding deliberately hides its cited lines.  The
        # CWE class is the public evidence it still carries; the private record
        # has the exact site.
        cwe = (finding.cwe or "").upper()
        return any(
            str(truth.get("cwe", "")).upper().split(":")[0] in cwe
            for truth in ground_truth
            if truth.get("cwe")
        )
    for site in finding.cited_sites:
        for truth in ground_truth:
            if site.path == truth["path"] and abs(site.line - int(truth["line"])) <= 2:
                return True
    return False


def run_case(case: dict[str, Any], workspace_root: Path, provider: str) -> dict[str, Any]:
    root = workspace_root / case["id"]
    if root.exists():
        shutil.rmtree(root)
    fixture = make_repo(workspace_root, case["id"])
    base = fixture.commit("base", **{path: text for path, text in case["base"].items()})
    head = fixture.commit("head", **{path: text for path, text in case["head"].items()})
    state = workspace_root / f"{case['id']}-state"
    settings_data = {
        "sandbox.runtime": "local",
        "state.dir": str(state),
        "review.deadline_secs": 180,
        "review.stage2_timeout_secs": 45,
        "sast.semgrep_config": str(ROOT / "rules"),
    }
    if provider == "live":
        settings_data.update(
            {
                "llm.base_url": os.environ.get("ZERO_LOOP_LLM_BASE_URL", ""),
                "llm.api_key_env": "ZERO_LOOP_LLM_API_KEY",
            }
        )
    settings = load_settings(settings_data, os.environ)
    oracle = OracleLLM(_answers(case)) if provider == "scripted" else None
    monitor = OracleLLM({"MonitorVote": _answers(case)["MonitorVote"]}) if provider == "scripted" else None
    started = time.monotonic()
    verdict: ReviewVerdict = Reviewer(
        settings,
        environ=os.environ,
        llm=oracle,
        monitor_llm=monitor,
    ).review(
        ReviewRequest(
            workspace=fixture.root,
            base_ref=base,
            head_ref=head,
            expected_head_sha=head,
            description=case.get("description", ""),
        )
    )
    matched = any(_matches(finding, case["ground_truth"]) for finding in verdict.findings)
    if case["kind"] == "buggy":
        success = verdict.decision.value == "request_changes" and matched
    elif case["kind"] == "fixed":
        # A confirmed fix is evidence for approval, never a finding.
        success = verdict.decision.value == "approve" and not verdict.findings
    else:
        success = verdict.decision.value == "approve" and not verdict.findings
    return {
        "id": case["id"],
        "kind": case["kind"],
        "decision": verdict.decision.value,
        "findings": len(verdict.findings),
        "finding_sites": [[site.path, site.line] for finding in verdict.findings for site in finding.cited_sites],
        "matched_ground_truth": matched,
        "success": success,
        "degraded": verdict.degraded,
        "elapsed_ms": verdict.elapsed_ms,
        "receipts": len(verdict.receipts),
        "wall_ms": int((time.monotonic() - started) * 1000),
        "error": "",
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", choices=["scripted", "live"], default="scripted")
    parser.add_argument("--only", default="")
    parser.add_argument("--json-out", default=str(ROOT / "eval" / "report.json"))
    args = parser.parse_args(argv)

    if args.provider == "live":
        endpoint = os.environ.get("ZERO_LOOP_LLM_BASE_URL", "")
        if not endpoint:
            print("live mode needs ZERO_LOOP_LLM_BASE_URL and ZERO_LOOP_LLM_API_KEY; refusing to fabricate a number")
            return 2

    workspace_root = Path(tempfile.mkdtemp(prefix="zero-loop-eval-", dir=str(Path.home() / ".cache")))
    cases = [case for case in CASES if not args.only or args.only in case["id"]]
    results: list[dict[str, Any]] = []
    try:
        for case in cases:
            try:
                result = run_case(case, workspace_root, args.provider)
            except Exception as exc:  # a crashed case is a miss, not a silent pass
                result = {
                    "id": case["id"],
                    "kind": case["kind"],
                    "decision": "error",
                    "findings": 0,
                    "matched_ground_truth": False,
                    "success": False,
                    "degraded": [],
                    "elapsed_ms": 0,
                    "receipts": 0,
                    "wall_ms": 0,
                    "error": f"{type(exc).__name__}: {exc}",
                }
            results.append(result)
            print(f"{result['id']:<32} {result['kind']:<6} {result['decision']:<16} success={result['success']}")
            if result["error"]:
                print(f"    error: {result['error']}")
    finally:
        shutil.rmtree(workspace_root, ignore_errors=True)

    buggy = [r for r in results if r["kind"] == "buggy"]
    fixed = [r for r in results if r["kind"] == "fixed"]
    clean = [r for r in results if r["kind"] in ("clean", "fixed")]
    intercepted = sum(1 for r in buggy if r["success"])
    false_positives = sum(1 for r in clean if not r["success"])
    summary = {
        "provider": args.provider,
        "cases": len(results),
        "buggy_cases": len(buggy),
        "intercepted": intercepted,
        "interception_rate": (intercepted / len(buggy)) if buggy else None,
        "clean_cases": len(clean),
        "fixed_cases": len(fixed),
        "false_positives": false_positives,
        "false_positive_rate": (false_positives / len(clean)) if clean else None,
        "all_receipts_present": all(r["receipts"] > 0 for r in results),
        "results": results,
    }
    Path(args.json_out).write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print()
    print(
        f"interception {intercepted}/{len(buggy)}"
        f" ({(intercepted / len(buggy) * 100) if buggy else 0:.1f}%)  "
        f"clean+fixed false positives {false_positives}/{len(clean)}"
    )
    print(f"report: {args.json_out}")
    return 0 if false_positives == 0 and intercepted == len(buggy) else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
