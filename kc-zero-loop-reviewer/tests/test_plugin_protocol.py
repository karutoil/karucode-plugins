"""The plugin as karucode sees it: a real JSON-RPC subprocess on stdio."""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time
from pathlib import Path

import pytest

from zero_loop_reviewer.models import Decision, ReviewVerdict
from zero_loop_reviewer.stage1.receipts import ReceiptLog, read_chain, verify_chain

BIN = Path(__file__).resolve().parents[1] / "bin" / "zero-loop-reviewer"


class PluginProcess:
    def __init__(self, env: dict[str, str]):
        self.proc = subprocess.Popen(
            [str(BIN), "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )
        self.lines: "queue.Queue[dict]" = queue.Queue()
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()
        self._id = 0

    def _read(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            line = line.strip()
            if line:
                self.lines.put(json.loads(line))

    def send(self, message: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def request(self, method: str, params: dict | None = None, *, timeout: float = 20.0) -> dict:
        self._id += 1
        request_id = self._id
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params or {}})
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                message = self.lines.get(timeout=max(0.05, deadline - time.monotonic()))
            except queue.Empty:
                break
            if message.get("method") == "plugin/settings/get":
                self.send({"jsonrpc": "2.0", "id": message["id"], "result": {"settings": self.settings}})
                continue
            if message.get("id") == request_id and "method" not in message:
                return message
            raise AssertionError(f"unexpected frame while waiting for {method}: {message}")
        raise AssertionError(f"no answer to {method}")

    def close(self) -> int:
        try:
            self.request("shutdown", timeout=5)
        except AssertionError:
            pass
        try:
            return self.proc.wait(timeout=10)
        finally:
            if self.proc.poll() is None:
                self.proc.kill()


@pytest.fixture()
def plugin(tmp_path: Path):
    settings = {
        "state.dir": str(tmp_path / "state"),
        "sandbox.runtime": "none",
        "llm.base_url": "http://127.0.0.1:9/v1",
        "ci.provider": "none",
    }
    env = {
        **os.environ,
        "PYTHONPATH": str(Path(__file__).resolve().parents[1]),
        "KARUCODE_PLUGIN_SETTINGS": json.dumps(settings),
    }
    process = PluginProcess(env)
    process.settings = settings
    yield process
    process.close()


def test_initialize_advertises_a_valid_manifest(plugin):
    result = plugin.request("initialize", {"protocol": "1"})["result"]
    assert result["name"] == "zero-loop-reviewer"
    assert result["protocol"] == "1"
    assert result["version"] == "1.0.0"
    assert set(result["permissions"]) == {"files.read", "files.write", "shell", "network"}
    assert 1 <= len(result["settings"]) <= 32
    keys = [setting["key"] for setting in result["settings"]]
    assert len(keys) == len(set(keys))
    for setting in result["settings"]:
        assert setting["type"] in {"bool", "text", "number", "select", "secret"}
        assert "default" in setting
    assert any(setting["key"] == "model.primary" for setting in result["settings"])
    assert any(setting["key"] == "fusion.c_fr" for setting in result["settings"])


def test_tools_list_exposes_three_tools_with_classes(plugin):
    tools = plugin.request("tools/list")["result"]["tools"]
    by_name = {tool["name"]: tool for tool in tools}
    assert set(by_name) == {"review_pr", "review_status", "explain_finding"}
    assert by_name["review_pr"]["class"] == "shell"
    assert by_name["review_status"]["class"] == "read_only"
    assert by_name["review_pr"]["parameters"]["required"] == ["base"]


def test_review_status_reads_a_stored_verdict_and_verifies_its_chain(plugin, tmp_path):
    state = Path(plugin.settings["state.dir"])
    log = ReceiptLog(state / "receipts", "review-test")
    log.record(
        stage="1.contract",
        status=__import__("zero_loop_reviewer.models", fromlist=["AttemptStatus"]).AttemptStatus.COMPLETED,
        started_at="2026-01-01T00:00:00.000Z",
        finished_at="2026-01-01T00:00:01.000Z",
    )
    verdict = ReviewVerdict(
        review_id="review-test",
        base_sha="a" * 40,
        head_sha="b" * 40,
        decision=Decision.APPROVE,
        p_combined=0.02,
        tau=0.909,
        receipts=read_chain(state / "receipts/review-test/receipts.jsonl"),
    )
    log.write_json("verdict.json", verdict.model_dump(mode="json"))

    response = plugin.request("tools/call", {"name": "review_status", "arguments": {"review_id": "review-test"}})
    result = response["result"]
    assert result["ok"] is True
    assert "APPROVE" in result["content"]
    assert "Receipt chain: valid" in result["content"]
    assert result["meta"]["receipt_chain_valid"] is True
    ok, why = verify_chain(read_chain(state / "receipts/review-test/receipts.jsonl"))
    assert ok, why


def test_review_status_rejects_a_tampered_chain(plugin):
    state = Path(plugin.settings["state.dir"])
    log = ReceiptLog(state / "receipts", "tampered")
    log.record(
        stage="1.contract",
        status=__import__("zero_loop_reviewer.models", fromlist=["AttemptStatus"]).AttemptStatus.COMPLETED,
        started_at="2026-01-01T00:00:00.000Z",
        finished_at="2026-01-01T00:00:01.000Z",
    )
    verdict = ReviewVerdict(
        review_id="tampered",
        base_sha="a" * 40,
        head_sha="b" * 40,
        decision=Decision.APPROVE,
        p_combined=0.02,
        tau=0.909,
    )
    log.write_json("verdict.json", verdict.model_dump(mode="json"))
    chain = state / "receipts/tampered/receipts.jsonl"
    chain.write_text(chain.read_text().replace("1.contract", "1.contractx"), encoding="utf-8")

    result = plugin.request("tools/call", {"name": "review_status", "arguments": {"review_id": "tampered"}})["result"]
    assert result["ok"] is True
    assert "BROKEN" in result["content"]
    assert result["meta"]["receipt_chain_valid"] is False


def test_panel_reports_no_reviews_yet(plugin):
    panel = plugin.request("panels/read", {"panel": "review_status"})["result"]
    assert panel["title"] == "Zero-loop review"
    assert panel["data"]["reviews"] == 0


def test_unknown_tool_is_a_jsonrpc_error(plugin):
    response = plugin.request("tools/call", {"name": "nope", "arguments": {}})
    assert response["error"]["code"] == -32601


def test_no_argument_invocation_defaults_to_serve(monkeypatch):
    """karucode spawns the plugin with no argv; that must not crash."""
    import zero_loop_reviewer.cli as cli

    called: list[bool] = []

    def fake_serve(plugin):
        called.append(True)
        return 0

    monkeypatch.setattr(cli, "serve", fake_serve)
    assert cli.main([]) == 0
    assert called == [True]


def test_review_pr_runs_the_whole_pipeline_through_the_tool_surface(tmp_path):
    """The exact call karucode makes: tools/call -> five stages -> one verdict."""
    import difflib

    from tests.support.gitfix import make_repo
    from tests.support.openai_server import FakeOpenAI

    base_file = "def add(a, b):\n    return a + b\n"
    head_file = (
        "def add(a, b):\n"
        "    return a + b\n"
        "\n"
        "\n"
        "def percentile(values, p):\n"
        "    ordered = sorted(values)\n"
        "    return ordered[int(len(ordered) * p)]\n"
    )
    fixed_file = (
        "def add(a, b):\n"
        "    return a + b\n"
        "\n"
        "\n"
        "def percentile(values, p):\n"
        "    ordered = sorted(values)\n"
        "    index = min(len(ordered) - 1, max(0, int(len(ordered) * p)))\n"
        "    return ordered[index]\n"
    )
    fixture = make_repo(tmp_path, "review-tool")
    base = fixture.commit("base", **{"calc.py": base_file})
    head = fixture.commit("head", **{"calc.py": head_file})
    patch = "\n".join(
        difflib.unified_diff(
            head_file.splitlines(),
            fixed_file.splitlines(),
            fromfile="a/calc.py",
            tofile="b/calc.py",
            lineterm="",
        )
    ) + "\n"
    answers = {
        "TriagePlan": {"focus": [], "search_symbols": [], "risk_hypotheses": [], "budget_note": ""},
        "SecSemScan": {
            "monologue": "No untrusted input on a changed line.",
            "cwe": "",
            "verdict": "SAFE",
            "cited_lines": [],
            "taint": [],
            "lifetime": [],
            "confidence": 0.5,
        },
        "BehavioralCandidates": {
            "candidates": [
                {
                    "title": "percentile indexes past the end at p=1.0",
                    "category": "correctness",
                    "severity": 2,
                    "cited_sites": [{"path": "calc.py", "line": 7}],
                    "root_cause": "int(len * 1.0) == len",
                    "suspected_trigger": "percentile([1,2,3], 1.0)",
                }
            ]
        },
        "ProbeSpec": {
            "test_path": ".zero/test_percentile.py",
            "content": "from calc import percentile\n\n\ndef test_full():\n    assert percentile([1, 2, 3], 1.0) == 3\n",
            "target_symbols": ["percentile"],
            "framework": "pytest",
            "rationale": "boundary",
        },
        "FallbackVerdict": {"verdict": "accept", "confidence": 0.9, "rationale": "probe is evidence"},
        "PatchProposal": {"patch": patch, "files_changed": ["calc.py"], "rationale": "clamp index"},
        "RiskElicitation": {
            "defect_present": True,
            "category": "correctness",
            "severity_score": 2,
            "failure_probability": 0.95,
            "cited_sites": [{"path": "calc.py", "line": 7}],
            "root_cause": "index len",
        },
        "MonitorVote": {"score": 0.9, "rationale": "boundary math"},
        "MciScan": {"claims": [], "summary": ""},
    }
    with FakeOpenAI(answers) as fake:
        settings = {
            "state.dir": str(tmp_path / "state"),
            "sandbox.runtime": "local",
            "llm.base_url": fake.base_url,
            "llm.api_key_env": "PLUGIN_TEST_KEY",
            "sast.semgrep_config": str(Path(__file__).resolve().parents[1] / "rules"),
            "review.deadline_secs": 150,
            "review.stage2_timeout_secs": 45,
            "ci.provider": "none",
        }
        env = {
            **os.environ,
            "PYTHONPATH": str(Path(__file__).resolve().parents[1]),
            "KARUCODE_PLUGIN_SETTINGS": json.dumps(settings),
            "PLUGIN_TEST_KEY": "sk-test",
        }
        process = PluginProcess(env)
        process.settings = settings
        try:
            response = process.request(
                "tools/call",
                {
                    "name": "review_pr",
                    "arguments": {
                        "workspace": str(fixture.root),
                        "base": base,
                        "head": head,
                        "expected_head_sha": head,
                    },
                },
                timeout=180,
            )
            assert process.close() == 0
        finally:
            if process.proc.poll() is None:
                process.proc.kill()

    result = response["result"]
    assert result["ok"] is True, result
    assert result["meta"]["decision"] == "request_changes"
    assert result["meta"]["findings"]
    assert "```suggestion" in result["content"] or "Verified" in result["content"]
    assert result["meta"]["receipts"] > 0


def test_post_true_without_a_provider_is_refused(plugin, tmp_path):
    fixture_repo = tmp_path / "repo"
    fixture_repo.mkdir()
    result = plugin.request(
        "tools/call",
        {
            "name": "review_pr",
            "arguments": {"workspace": str(fixture_repo), "base": "HEAD", "post": True},
        },
    )["result"]
    assert result["ok"] is False
    assert "ci.provider" in result["content"]
