#!/usr/bin/env python3
"""Drive the built plugin over its own protocol, exactly as karucode does.

This is the check a unit test cannot make: the real binary, the real backend, a
real search, and the real translation of an MCP result into a karucode tool
result. Everything else in this directory is a probe of the *upstream* server;
this file is the only thing that exercises what will actually be installed.

    python3 probe/drive_plugin.py          # offline checks only
    python3 probe/drive_plugin.py --live   # also hit the network

Exits non-zero on the first failed expectation, so it is usable as a gate.
"""
import json
import os
import subprocess
import sys
import time

PLUGIN = os.path.join(os.path.dirname(__file__), "..", "target", "release", "free-search")
PLUGIN = os.path.abspath(PLUGIN)

failures = []


def check(condition, label, detail=""):
    if condition:
        print(f"  ok    {label}")
    else:
        print(f"  FAIL  {label} {detail}")
        failures.append(label)


class Plugin:
    """The plugin process, spoken to with newline-delimited JSON-RPC."""

    def __init__(self, settings=None, prewarm=False):
        env = dict(os.environ)
        if settings is not None:
            env["KARUCODE_PLUGIN_SETTINGS"] = json.dumps(settings)
        if not prewarm:
            # Keep these runs fast and deterministic: the cold-start path is a
            # separate, deliberate check.
            settings = dict(settings or {})
            settings["prewarm"] = False
            env["KARUCODE_PLUGIN_SETTINGS"] = json.dumps(settings)
        self.stderr = open(
            os.path.join(os.path.dirname(__file__), "..", "target", "plugin-stderr.log"), "wb"
        )
        self.proc = subprocess.Popen(
            [PLUGIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=self.stderr, bufsize=0,
        )
        self.next_id = 0

    def request(self, method, params=None, timeout=120):
        self.next_id += 1
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}}
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                raise SystemExit("the plugin closed stdout")
            frame = json.loads(line)
            if frame.get("method") == "plugin/settings/get":
                reply = {"jsonrpc": "2.0", "id": frame["id"], "result": {"settings": {}}}
                self.proc.stdin.write((json.dumps(reply) + "\n").encode())
                self.proc.stdin.flush()
                continue
            if frame.get("id") == self.next_id:
                return frame
        raise SystemExit(f"no answer to {method} within {timeout}s")

    def call(self, tool, arguments, timeout=120):
        reply = self.request("tools/call", {"name": tool, "arguments": arguments}, timeout)
        if "error" in reply:
            return {"ok": False, "content": reply["error"].get("message", "")}
        return reply.get("result", {})

    def stop(self):
        try:
            self.request("shutdown", timeout=5)
        except SystemExit:
            pass
        try:
            self.proc.terminate()
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def section(title):
    print(f"\n== {title}")


def main():
    live = "--live" in sys.argv

    section("handshake is fast and starts no backend")
    started = time.time()
    plugin = Plugin()
    init = plugin.request("initialize", {"protocol": "1"})
    elapsed = time.time() - started
    manifest = init["result"]
    check(elapsed < 10, f"initialize answered in {elapsed:.2f}s (< the host's 10s budget)")
    check(manifest["name"] == "free-search", "manifest name matches the [[plugins]] entry")
    check(manifest["protocol"] == "1", "protocol is 1")
    check("before_prompt" in manifest["hooks"], "advertises before_prompt")
    panels = [p["id"] for p in manifest["panels"]]
    check(panels == ["status"], f"declares the status panel (got {panels})")
    check(
        set(manifest["permissions"]) == {"shell", "network"},
        f"declares shell+network (got {manifest['permissions']})",
    )

    section("tools/list")
    tools = plugin.request("tools/list")["result"]["tools"]
    names = [t["name"] for t in tools]
    check(len(tools) == 11, f"exposes 11 tools (got {len(tools)})")
    # `tools/list` is alphabetical: the plugin SDK keeps tools in a BTreeMap, so
    # position carries no meaning. What matters is that the whole set is there.
    check(names == sorted(names), f"tools/list is stable and alphabetical ({names[:3]}…)")
    check("search" in names and "fetch" in names, "the two core tools are present")
    by_name = {t["name"]: t for t in tools}
    huge = [t["name"] for t in tools if len(t["description"]) > 1500]
    check(not huge, f"every description is under 1500 bytes (over: {huge})")
    check(
        all("Rule: `search` and `research` find URLs" in t["description"] for t in tools),
        "every description carries the shared rule",
    )
    # The enums must survive: they are what stops a typo becoming a wasted call.
    freshness = by_name["search"]["parameters"]["properties"]["freshness"]["anyOf"][0]["enum"]
    check(freshness == ["day", "week", "month", "year"], f"freshness enum intact ({freshness})")
    cats = by_name["search"]["parameters"]["properties"]["category"]["anyOf"][0]["enum"]
    check(len(cats) > 40, f"category enum intact ({len(cats)} values)")
    check(
        by_name["search"]["parameters"]["properties"]["max_results"]["default"] == 10,
        "max_results default is 10",
    )
    check(
        "title" not in by_name["search"]["parameters"],
        "Pydantic `title` noise is stripped",
    )
    classes = {t["name"]: t["class"] for t in tools}
    check(classes["download"] == "mutating", "download declares mutating")
    check(
        classes["search"] == "read_only",
        "search declares read_only (the operator still allowlists it)",
    )

    section("before_prompt")
    hook_a = plugin.request("hooks/run", {"hook": "before_prompt", "payload": {"session_id": "s1"}})
    hook_b = plugin.request("hooks/run", {"hook": "before_prompt", "payload": {"session_id": "s2"}})
    text = hook_a["result"]["output"]
    check(isinstance(text, str) and len(text) < 4096, f"contribution is text under 4KiB ({len(text)}B)")
    check(text == hook_b["result"]["output"], "byte-stable across calls (prompt cache)")
    check("never ask the user" in text, "says no API key is needed")
    check("not the page" in text, "states that a snippet is not the page")
    check("dated: 3/10" in text, "points at the age markers")

    section("panel, with the backend not yet started")
    panel = plugin.request("panels/read", {"panel": "status", "workspace": "/tmp"})
    data = panel["result"]["data"]
    check(data["backend"]["running"] is False, "reports not running")
    check(data["schemas"]["drift"] is None, "drift is null until a backend has run")
    check(
        "download" not in data["config"]["read_only_candidates"],
        "the allowlist candidates exclude download",
    )
    check(data["schemas"]["baked_version"] == "0.17.0", "reports the vendored schema version")
    check(
        all("description_bytes" in t for t in data["schemas"]["tools"]),
        "lists every tool it exposes",
    )

    if not live:
        print("\n(offline only; re-run with --live for a real search)")
        plugin.stop()
        return report()

    section("live: search")
    started = time.time()
    result = plugin.call("search", {"query": "rust std process group kill", "max_results": 3})
    elapsed = time.time() - started
    body = result.get("content", "")
    check(result.get("ok") is True, f"search succeeded: {body[:200]}")
    check("http" in body, "returned links")
    check(elapsed < 60, f"answered in {elapsed:.1f}s (inside the 60s host budget)")
    check(
        "Error executing tool" not in body,
        "the doubled backend error prefix is stripped",
    )
    print(f"  ...   first search took {elapsed:.1f}s (includes backend start)")

    section("live: a cache hit is fast")
    started = time.time()
    again = plugin.call("search", {"query": "rust std process group kill", "max_results": 3})
    check(again.get("ok") is True, "the repeated search succeeded")
    check(time.time() - started < elapsed, "the cached search was faster")
    print(f"  ...   cached search took {time.time() - started:.2f}s")

    section("live: an argument error reaches the model cleanly")
    bad = plugin.call("search", {"query": ""})
    check(bad.get("ok") is False, "an empty query is reported as a failure")
    check(
        "query must not be empty" in bad.get("content", ""),
        f"the backend's message survives: {bad.get('content', '')[:120]}",
    )

    section("live: panel now sees a healthy backend")
    data = plugin.request("panels/read", {"panel": "status", "workspace": "/tmp"})["result"]["data"]
    check(data["backend"]["running"] is True, "reports running")
    check(data["backend"]["server"]["version"] == "0.17.0", "reports the live version")
    check(data["backend"]["stats"]["calls"] >= 2, "counted the calls")
    drift = data["schemas"]["drift"]
    check(drift is not None and drift.get("stale") is False, f"no schema drift: {drift}")
    if drift and "upstream_description_bytes" in drift:
        print(
            f"  ...   upstream descriptions {drift['upstream_description_bytes']}B "
            f"vs curated {drift['curated_description_bytes']}B"
        )

    section("live: no orphaned backend after shutdown")
    pid = data["backend"]["pid"]
    plugin.stop()
    time.sleep(0.5)
    alive = os.path.exists(f"/proc/{pid}")
    check(not alive, f"backend pid {pid} is gone")

    return report()


def report():
    print()
    if failures:
        print(f"FAILED: {len(failures)} check(s): {', '.join(failures)}")
        return 1
    print("ALL CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
