#!/usr/bin/env python3
"""Exercise the real deep-research binary over karucode's JSON-RPC protocol.

Offline (default): scripted MCP backend, iterative success, mixed batch errors,
insufficient evidence, cancellation, scoping, pagination and force_refresh.
Live (--live): the actual free-search-mcp backend and public web.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent.parent
PLUGIN = ROOT / "target" / "release" / "deep-research"
FIXTURE = ROOT / "probe" / "fake_search_mcp.py"
LOG = ROOT / "target" / "probe-backend.jsonl"
failures = []


def check(condition, label, detail=""):
    if condition:
        print(f"  ok    {label}")
    else:
        print(f"  FAIL  {label} {detail}")
        failures.append(label)


class Plugin:
    def __init__(self, settings):
        self.stderr = open(ROOT / "target" / "probe-plugin-stderr.log", "wb")
        env = dict(os.environ)
        env["KARUCODE_PLUGIN_SETTINGS"] = json.dumps(settings)
        self.process = subprocess.Popen(
            [str(PLUGIN)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=self.stderr, env=env, bufsize=0,
        )
        self.next_id = 0
        self.notifications = []

    def request(self, method, params=None, timeout=120):
        self.next_id += 1
        expected = self.next_id
        message = {"jsonrpc": "2.0", "id": expected, "method": method, "params": params or {}}
        self.process.stdin.write((json.dumps(message) + "\n").encode())
        self.process.stdin.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.process.stdout.readline()
            if not line:
                self.stderr.flush()
                raise RuntimeError(f"plugin closed stdout; see {self.stderr.name}")
            frame = json.loads(line)
            if frame.get("method") == "plugin/settings/get":
                reply = {"jsonrpc": "2.0", "id": frame["id"], "result": {"settings": {}}}
                self.process.stdin.write((json.dumps(reply) + "\n").encode())
                self.process.stdin.flush()
                continue
            if frame.get("method") == "notify":
                self.notifications.append(frame.get("params", {}))
            if frame.get("id") == expected:
                return frame
        raise TimeoutError(f"no answer to {method} within {timeout}s")

    def call(self, tool, arguments=None, session="probe-session", workspace="/tmp/deep-probe"):
        return self.request("tools/call", {
            "name": tool, "arguments": arguments or {},
            "context": {"session_id": session, "workspace": workspace},
        })

    @staticmethod
    def result(reply):
        return reply.get("result", {"ok": False, "content": reply.get("error", {}).get("message", "")})

    def parsed(self, reply):
        result = self.result(reply)
        try:
            return json.loads(result["content"]), result
        except (KeyError, json.JSONDecodeError) as error:
            raise AssertionError(f"expected JSON tool result: {result} ({error})") from error

    def wait_terminal(self, job_id, limit=180):
        deadline = time.time() + limit
        latest = None
        while time.time() < deadline:
            reply = self.call("status", {"id": job_id, "wait_secs": 5})
            latest, raw = self.parsed(reply)
            check(raw["ok"], f"status call for {job_id} succeeds")
            if latest["terminal"]:
                return latest
        raise TimeoutError(f"job {job_id} did not finish: {latest}")

    def stop(self):
        try:
            self.request("shutdown", timeout=5)
        except Exception:
            pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=5)
        self.stderr.close()


def start_args(question, **overrides):
    args = {
        "question": question,
        "aspects": ["pricing", "security"],
        "min_sources": 3,
        "min_domains": 2,
        "min_rounds": 3,
        "max_rounds": 6,
        "max_searches": 18,
        "max_sources": 18,
        "results_per_search": 5,
        "max_minutes": 2,
    }
    args.update(overrides)
    return args


def offline():
    LOG.unlink(missing_ok=True)
    FIXTURE.chmod(0o755)
    plugin = Plugin({
        "command": [str(FIXTURE)],
        "env": {"DEEP_PROBE_LOG": str(LOG)},
        "max_output_bytes": 20000,
    })
    try:
        init = plugin.request("initialize", {"protocol": "1"})["result"]
        check(init["name"] == "deep-research" and init["protocol"] == "1", "protocol initialize")
        check("before_prompt" in init["hooks"], "advertises stable guidance hook")
        tools = plugin.request("tools/list")["result"]["tools"]
        check([t["name"] for t in tools] == ["cancel", "list", "report", "start", "status"], "five companion tools")
        check(all(t["class"] == "read_only" for t in tools), "tools declare no project mutations")

        print("\n== iterative completion and insufficient-evidence behavior")
        first = plugin.call("start", start_args("Rust ownership"))
        status, raw = plugin.parsed(first)
        check(raw["ok"], f"start accepted: {raw['content'][:160]}")
        job_id = status["id"]
        final = plugin.wait_terminal(job_id)
        check(final["state"] == "complete", f"iterated to complete: rounds={final['round']} searches={final['searches']} sources={final['sources_read']}", final.get("stop_reason"))
        check(final["round"] >= 3 and final["searches"] >= 3, "more than one search/round ran")
        check(final["fetch_failures"] >= 1, "mixed per-URL batch failures were retained")
        check(final["distinct_domains"] >= 2, "distinct domains measured")
        check(any(job_id in note.get("body", "") and note.get("session_id") == "probe-session" for note in plugin.notifications), "terminal job emits a scoped notify frame")

        duplicate = plugin.call("start", start_args("Off topic question now", max_rounds=5))
        partial_status, _ = plugin.parsed(duplicate)
        partial_id = partial_status["id"]
        partial = plugin.wait_terminal(partial_id)
        check(partial["state"] in ("partial", "failed"), "off-topic pages cannot satisfy evidence")
        check(partial["sources_read"] == 0 and partial["rejected_pages"] > 0, "off-topic fetched text was explicitly rejected")
        check(len(partial["coverage"]["uncovered_aspects"]) > 0, "partial names uncovered aspects")

        other_session = plugin.result(plugin.call("status", {"id": job_id}, session="other-session"))
        check(not other_session["ok"] and "unknown research job" in other_session["content"], "cross-session status is refused")
        other_list, _ = plugin.parsed(plugin.call("list", session="other-session"))
        check(other_list["jobs"] == [], "cross-session list leaks no jobs")

        print("\n== paginated report and exact JSON reconstruction")
        markdown = plugin.result(plugin.call("report", {"id": job_id, "max_bytes": 1000}))
        check(markdown["ok"] and len(markdown["content"].encode()) <= 1000, "Markdown page respects max_bytes")
        check(markdown["meta"]["has_more"], "large dossier reports next page")
        chunks = []
        offset = 0
        for _ in range(100):
            reply = plugin.result(plugin.call("report", {"id": job_id, "format": "json", "offset": offset, "max_bytes": 1000}))
            assert reply["ok"], reply
            chunks.append(reply["content"])
            if not reply["meta"]["has_more"]:
                break
            new_offset = reply["meta"]["next_offset"]
            assert new_offset > offset, "cursor must advance"
            offset = new_offset
        document = json.loads("".join(chunks))
        check(document["state"] == "complete" and len(document["sources"]) >= 3, "JSON report pages reconstruct exactly")
        check("candidates" not in document, "unread candidates absent from evidence JSON")
        check(all("THIS SNIPPET" not in chunk for chunk in chunks), "search snippets never leak into evidence")
        check(all("content_hash" in source and "cache_age_seconds" in source for source in document["sources"]), "source provenance includes content/cache metadata")

        print("\n== cancellation, duplicate suppression and panel")
        plugin_with_delay = None
        # A third job on the same fixture: delay applies only when configured for a
        # separate process below, but immediate cancel still exercises the boundary.
        cancelled_start, _ = plugin.parsed(plugin.call("start", start_args("Slow Rust ownership", max_rounds=5)))
        cancelled_id = cancelled_start["id"]
        dupe = plugin.result(plugin.call("start", start_args("Slow Rust ownership", max_rounds=5)))
        check(not dupe["ok"] and "active job" in dupe["content"], "duplicate active question is refused")
        cancelling = plugin.result(plugin.call("cancel", {"id": cancelled_id}))
        check(cancelling["ok"] and json.loads(cancelling["content"])["state"] in ("cancelling", "cancelled", "complete"), "cancel is prompt and explicit")
        cancelled = plugin.wait_terminal(cancelled_id, limit=20)
        check(cancelled["state"] in ("cancelled", "complete"), "cancel reaches a terminal state without rewriting complete")
        cancelled_again = plugin.result(plugin.call("cancel", {"id": cancelled_id}))
        check(cancelled_again["ok"], "cancel after terminal is idempotent")
        panel = plugin.request("panels/read", {"panel": "status", "session_id": "probe-session", "workspace": "/tmp/deep-probe"})["result"]["data"]
        check(len(panel["jobs"]) >= 3 and panel["integration"]["backend"] == "free-search-mcp", "panel reports scoped jobs and backend integration")

        print("\n== force_refresh uses single fetch, not cached fetch_batch")
        fresh_args = start_args(
            "Fresh Rust ownership", aspects=[], min_sources=1, min_domains=1,
            min_rounds=1, max_rounds=1, max_searches=1, max_sources=2,
            force_refresh=True,
        )
        fresh_start, _ = plugin.parsed(plugin.call("start", fresh_args))
        fresh = plugin.wait_terminal(fresh_start["id"])
        check(fresh["state"] == "complete", "freshness-sensitive run completes with generic aspects", fresh.get("stop_reason"))
    finally:
        plugin.stop()

    events = [json.loads(line) for line in LOG.read_text().splitlines()]
    calls = [event for event in events if event.get("event") == "call"]
    check(any(call["tool"] == "fetch_batch" for call in calls), "ordinary research reuses batched source reads")
    fresh_calls = [call for call in calls if call["tool"] == "fetch" and call["arguments"].get("force_refresh") is True]
    check(len(fresh_calls) >= 1, "force_refresh reached real per-URL fetch calls")
    pids = {event["pid"] for event in events}
    time.sleep(0.2)
    check(all(not Path(f"/proc/{pid}").exists() for pid in pids), f"scripted backend(s) stopped with plugin: {pids}")


def live():
    plugin = Plugin({"max_output_bytes": 20000})
    try:
        args = {
            "question": "Model Context Protocol (MCP)",
            "aspects": ["tools and capabilities", "security and consent"],
            "min_sources": 4,
            "min_domains": 2,
            "min_rounds": 3,
            "max_rounds": 8,
            "max_searches": 24,
            "max_sources": 16,
            "max_minutes": 5,
            "force_refresh": True,
        }
        started, raw = plugin.parsed(plugin.call("start", args))
        check(raw["ok"], "live research started", raw["content"][:200])
        final = plugin.wait_terminal(started["id"], limit=360)
        print(json.dumps(final, indent=2))
        check(final["terminal"] and final["round"] >= 3, "live job performed iterative rounds")
        check(final["searches"] >= 3 and final["sources_read"] > 0, "live job searched and actually read sources")
        check(final["state"] == "complete", "live coverage predicate completed", final.get("stop_reason"))
        report = plugin.result(plugin.call("report", {"id": started["id"], "max_bytes": 12000}))
        check(report["ok"] and "Evidence by aspect" in report["content"], "live evidence report is readable")
        Path(ROOT / "target" / "live-status.json").write_text(json.dumps(final, indent=2))
        Path(ROOT / "target" / "live-report-page.md").write_text(report["content"])
    finally:
        plugin.stop()


def main():
    if "--live" in sys.argv:
        live()
    else:
        offline()
    print()
    if failures:
        print(f"FAILED: {len(failures)} checks: {', '.join(failures)}")
        return 1
    print("ALL CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
