#!/usr/bin/env python3
"""Drive kc-goals end to end against a real karucode server and a scripted model.

This is the test that matters: the unit suite proves the rulebook, and this proves
the loop. A throwaway server (temp data dir, temp workspace, its own port) with the
plugin registered talks to a scripted OpenAI-compatible endpoint, so "the model
stops early" is a deterministic input rather than something to hope for.

Scenarios:

  stop-early  the model records a goal, stops, and is resumed exactly once; then it
              evidences every criterion, completes the goal, and the loop stops.
  cap         a model that never records progress is resumed exactly
              `max_continuations` times and then paused - the loop is bounded.
  cancel      a human cancelling the run pauses the goal instead of restarting it.

Usage:
  python3 tests/live/drive.py                     # every scenario
  python3 tests/live/drive.py --scenario cap --keep
  python3 tests/live/drive.py --binary /path/to/karucode --plugin-bin /path/to/kc-goals
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
PY = sys.executable
DEFAULT_BINARY = Path("/home/karutoil/karucode/target/release/karucode")
DEFAULT_PLUGIN = HERE.parent.parent / "target" / "release" / "kc-goals"
MOCK = HERE / "mock_provider.py"

RESULTS = []
SCENARIOS = {"stop-early", "unlimited", "capped", "stall", "checks", "cancel"}
# A plugin tool is exposed to the model under this prefix; a script that names the
# bare tool gets "unknown tool `goal_set`", which is worth spelling out here.
TOOL = "plugin__kc-goals__"


def call(name, arguments):
    """A plugin tool call: the model must use the namespaced name."""
    return {"call": {"name": TOOL + name, "arguments": arguments}}


def builtin(name, arguments):
    """A first-party tool call (`run_command`, `write_file`, …) - no prefix."""
    return {"call": {"name": name, "arguments": arguments}}


def check(name, fn):
    try:
        detail = fn()
        RESULTS.append((name, True, detail or ""))
        print(f"  PASS  {name}" + (f" — {detail}" if detail else ""), flush=True)
    except AssertionError as error:
        RESULTS.append((name, False, str(error)))
        print(f"  FAIL  {name}: {error}", flush=True)
    except Exception as error:  # a broken harness must not look like a pass
        RESULTS.append((name, False, f"{type(error).__name__}: {error}"))
        print(f"  ERROR {name}: {type(error).__name__}: {error}", flush=True)


def need(condition, message):
    if not condition:
        raise AssertionError(message)


def free_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def wait_http(url, timeout=30):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as response:
                if response.status < 500:
                    return True
        except Exception as error:
            last = error
            time.sleep(0.1)
    raise RuntimeError(f"{url} never answered ({last})")


class Client:
    def __init__(self, port):
        self.base = f"http://127.0.0.1:{port}"

    def raw(self, method, path, body=None, timeout=30):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        request.add_header("Content-Type", "application/json")
        request.add_header("X-Karucode-Client", "web")
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read()
                return response.status, json.loads(raw) if raw else None
        except urllib.error.HTTPError as error:
            raw = error.read()
            try:
                return error.code, json.loads(raw)
            except Exception:
                return error.code, raw

    def get(self, path, **kw):
        return self.raw("GET", path, **kw)

    def post(self, path, body=None, **kw):
        return self.raw("POST", path, body, **kw)

    def ok(self, method, path, body=None, expect=200):
        status, payload = self.raw(method, path, body)
        assert status in (expect, 200, 201, 202, 204), f"{method} {path} -> {status}: {payload}"
        return payload


DEFAULT_SETTINGS = {
    "tick_secs": 2,
    "min_gap_secs": 0,
}


class Lab:
    """One throwaway server + workspace + scripted model."""

    def __init__(
        self,
        binary,
        plugin,
        turns,
        settings,
        keep=False,
        permissions=("files.read", "files.write", "network"),
        web_dir=None,
    ):
        self.turns = turns
        self.settings = dict(DEFAULT_SETTINGS)
        self.settings.update(settings)
        self.keep = keep
        self.tmp = Path(tempfile.mkdtemp(prefix="kc-goals-live-"))
        self.ws = self.tmp / "ws"
        self.ws.mkdir(parents=True)
        (self.ws / "README.md").write_text("# kc-goals live workspace\n")
        self.mock_port = free_port()
        self.port = free_port()
        self.script_path = self.tmp / "script.json"
        self.script_path.write_text(json.dumps({"turns": turns}, indent=2))
        self.mock_log = self.tmp / "mock.jsonl"

        self.mock = subprocess.Popen(
            [PY, str(MOCK), "--port", str(self.mock_port), "--script", str(self.script_path),
             "--log", str(self.mock_log)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
        )
        wait_http(f"http://127.0.0.1:{self.mock_port}/health", timeout=15)

        settings_lines = ["[plugins.settings]"]
        settings_lines.append(f'server = "http://127.0.0.1:{self.port}"')
        for key, value in self.settings.items():
            settings_lines.append(f"{key} = {json.dumps(value)}")
        config = [
            'host = "127.0.0.1"',
            'log_level = "warn"',
            "[[plugins]]",
            'name = "kc-goals"',
            f'command = ["{plugin}"]',
            "enabled = true",
            f'permissions = {json.dumps(list(permissions))}',
            *settings_lines,
        ]
        self.web_dir = web_dir
        self.config_path = self.tmp / "config.toml"
        self.config_path.write_text("\n".join(config) + "\n")

        env = os.environ.copy()
        if self.web_dir:
            env["KARUCODE_WEB_DIR"] = str(self.web_dir)
        env.update(
            {
                "KARUCODE_HOST": "127.0.0.1",
                "KARUCODE_PORT": str(self.port),
                "KARUCODE_WORKSPACE": str(self.ws),
                "KARUCODE_DATA_DIR": str(self.tmp / "data"),
                "KARUCODE_CONFIG": str(self.config_path),
                "KARUCODE_BASE_URL": f"http://127.0.0.1:{self.mock_port}/v1",
                "KARUCODE_MODEL": "mock-coder",
                "KARUCODE_APPROVAL_MODE": "auto",
                "KARUCODE_LOG": "warn",
                "RUST_LOG": "warn",
                # AI titles would spend a scripted turn on a title request, which
                # would make the turn counter meaningless.
                "KARUCODE_TITLES": "0",
                "KARUCODE_RETRY_MAX_RETRIES": "0",
                "KARUCODE_RETRY_MAX_STREAM_RESTARTS": "1",
            }
        )
        self.server_log = self.tmp / "server.log"
        self.log_handle = open(self.server_log, "w")
        self.server = subprocess.Popen(
            [str(binary)], cwd=str(self.ws), env=env,
            stdout=self.log_handle, stderr=subprocess.STDOUT,
        )
        try:
            wait_http(f"http://127.0.0.1:{self.port}/api/health", timeout=40)
        except Exception:
            self.close()
            raise RuntimeError("server did not start:\n" + self.tail_log())
        self.client = Client(self.port)
        self.session = self.client.ok("POST", "/api/sessions", {"title": "kc-goals live"})["id"]

    # --- helpers -----------------------------------------------------------

    def tail_log(self, lines=40):
        try:
            return "\n".join(self.server_log.read_text().splitlines()[-lines:])
        except Exception:
            return "(no log)"

    def goal_path(self):
        return self.ws / ".karucode" / "goals" / f"{self.session}.json"

    def goal_doc(self):
        if not self.goal_path().exists():
            return None
        return json.loads(self.goal_path().read_text())

    def active_goal(self):
        doc = self.goal_doc()
        if not doc or not doc.get("goals"):
            return None
        return doc["goals"][-1]

    def info(self):
        return self.client.ok("GET", f"/api/sessions/{self.session}/info")

    def runs(self):
        return self.info()["runs"]

    def send(self, text):
        out = self.client.ok("POST", f"/api/sessions/{self.session}/messages", {"content": text})
        return out["run_id"]

    def wait(self, predicate, timeout=60, label="condition"):
        deadline = time.time() + timeout
        while time.time() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.2)
        raise AssertionError(f"timed out waiting for {label}")

    def wait_idle(self, timeout=60):
        self.wait(lambda: not self.info()["running"], timeout=timeout, label="an idle session")

    def wait_goal(self, status, timeout=90):
        try:
            return self.wait(
                lambda: (
                    self.active_goal()
                    if self.active_goal() and self.active_goal()["status"] == status
                    else None
                ),
                timeout=timeout,
                label=f"goal status {status} (now: {(self.active_goal() or {}).get('status')})",
            )
        except AssertionError as error:
            raise AssertionError(
                f"{error}\n  transcript: " + "\n    ".join(self.transcript_tail())
                + "\n  server log: " + self.tail_log(12)
            )

    def requests(self):
        if not self.mock_log.exists():
            return []
        return [json.loads(line) for line in self.mock_log.read_text().splitlines() if line.strip()]

    def transcript_tail(self, count=6):
        """The last few messages, so a timed-out wait says what the model saw."""
        try:
            messages = self.client.ok("GET", f"/api/sessions/{self.session}")["messages"]
            return [
                f"{m.get('role')}: {(m.get('content') or '')[:160]!r}"
                for m in messages[-count:]
            ]
        except Exception as error:
            return [f"(no transcript: {error})"]

    def plugin(self):
        for entry in self.client.ok("GET", "/api/plugins")["plugins"]:
            if entry["name"] == "kc-goals":
                return entry
        return None

    def close(self):
        for process in ("server", "mock"):
            handle = getattr(self, process, None)
            if handle is not None and handle.poll() is None:
                handle.terminate()
                try:
                    handle.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    handle.kill()
        try:
            self.log_handle.close()
        except Exception:
            pass

    def cleanup(self):
        self.close()
        if self.keep:
            print(f"  (kept {self.tmp})", flush=True)
        else:
            shutil.rmtree(self.tmp, ignore_errors=True)


def goal_set(title, criteria, **extra):
    args = {"title": title, "statement": title, "criteria": criteria}
    args.update(extra)
    return call("goal_set", args)


def scenario_stop_early(binary, plugin, keep):
    """The case the plugin exists for: a model that stops before the goal is met."""
    print("\n== scenario: stop-early ==", flush=True)
    turns = [
        goal_set("Ship the widget", ["widget builds", "widget tested"]),
        {"text": "Goal recorded. I will pick this up later."},
        call("goal_progress", {"note": "wrote the widget", "next": "run the tests"}),
        call("goal_criterion", {
            "criterion": "c1", "evidence": "cargo build --release finished clean"}),
        call("goal_criterion", {"criterion": "c2", "evidence": "142 unit tests pass"}),
        call("goal_complete", {"summary": "shipped the widget"}),
        {"text": "Done: the goal is complete."},
        {"text": "SCRIPT-EXHAUSTED: this run must never happen"},
    ]
    # No budget is passed: the default is "run until it is finished".
    lab = Lab(binary, plugin, turns, {})
    try:
        def ready():
            entry = lab.plugin() or {}
            need(
                entry.get("status") == "ready",
                f"plugin status {entry.get('status')}: {entry.get('error')}",
            )
            names = [tool["name"] for tool in entry["tools"]]
            # `goal_check` runs commands, so the host withholds it unless the
            # registration grants `shell`. This lab does not, so nine tools.
            need("goal_check" not in names, f"goal_check was exposed without `shell`: {names}")
            need(len(names) == 9, f"{len(names)} tools: {names}")
            return ", ".join(names)

        check("the plugin is ready with nine tools", ready)

        lab.send("Please work on the widget until it is finished.")
        goal = lab.wait_goal("done", timeout=120)

        def resumed():
            runs = lab.runs()
            need(runs["total"] == 2, f"{runs['total']} runs: {runs}")
            need(goal["progress"]["continuations"] == 1,
                 f"{goal['progress']['continuations']} continuations")
            return f"{runs['total']} runs, 1 continuation"

        check("the model stopped early and the goal was resumed", resumed)

        def continuation_text():
            texts = [m["content"] for r in lab.requests() for m in r["messages"]
                     if m["role"] == "user"]
            need(any(t.startswith("[kc-goals] Continuing goal g1") for t in texts),
                 f"no continuation among {len(texts)} user turns")
            return next(t for t in texts if t.startswith("[kc-goals]")).splitlines()[0]

        check("the continuation arrived as a user turn with the goal restated",
              continuation_text)

        def system_block():
            systems = [m["content"] for r in lab.requests() for m in r["messages"]
                       if m["role"] == "system"]
            hits = sum(1 for s in systems if "[kc-goals]" in s)
            need(hits > 0, "the orientation block never reached the system prompt")
            need(hits < len(systems),
                 f"the block was in all {len(systems)} requests, including those before the goal")
            return f"{hits} of {len(systems)} requests (absent before the goal existed)"

        check("the resumed run saw the orientation block in the system prompt",
              system_block)

        def evidenced():
            need(goal["status"] == "done", goal["status"])
            for criterion in goal["criteria"]:
                need(criterion["met"], f"{criterion['id']} is not met")
                need(criterion.get("evidence"), f"{criterion['id']} has no evidence")
            return "; ".join(f"{c['id']}={c['evidence'][:28]}" for c in goal["criteria"])

        check("the goal closed with evidence on every criterion", evidenced)

        def stopped():
            time.sleep(6)
            runs = lab.runs()
            need(runs["total"] == 2, f"{runs['total']} runs: a third run happened")
            marker = [m for r in lab.requests() for m in r["messages"]
                      if "SCRIPT-EXHAUSTED" in (m.get("content") or "")]
            need(not marker, "the exhausted-turn marker appeared: something kept going")
            return f"{runs['total']} runs after 6s idle"

        check("the loop stopped: no third run after the goal closed", stopped)

        def bounded_prompt():
            prompt = next(m["content"] for r in lab.requests() for m in r["messages"]
                          if m["role"] == "user" and m["content"].startswith("[kc-goals]"))
            need(len(prompt) < 3000, f"{len(prompt)} chars")
            return f"{len(prompt)} chars"

        check("the continuation prompt cost is bounded", bounded_prompt)

        def panel():
            data = lab.client.ok(
                "GET", f"/api/plugins/kc-goals/panels/goals?session={lab.session}")["data"]
            need(data["goals"], "no goals in the panel")
            card = data["goals"][0]
            need(card["status"] == "done", card["status"])
            need(card["score"] == "2/2", card["score"])
            need(data["error"] is None, data["error"])
            return f"{card['goal']} {card['score']} ({len(data['goals'])} goal(s), {len(data['log'])} log lines)"

        check("the panel lists the finished goal", panel)
    finally:
        lab.cleanup()


def scenario_unlimited(binary, plugin, keep):
    """With no budget, six consecutive stops are all resumed - and nothing pauses.

    Each run records a progress note before stopping, because a run that does
    *nothing* is a stall and the engine is right to pause it (see `stall`). The
    point here is the absence of a *count*: six resumes, no cap, no pause.
    """
    print("\n== scenario: unlimited ==", flush=True)
    turns = [goal_set("Keep going", ["never satisfied"])]
    for i in range(6):
        turns.append(call("goal_progress", {"note": f"step {i + 1}", "next": "another step"}))
        turns.append({"text": f"Did step {i + 1}, now stopping."})
    turns.append(call("goal_criterion", {"criterion": "c1", "evidence": "by hand, at last"}))
    turns.append(call("goal_complete", {"summary": "closed after six resumes"}))
    turns.append({"text": "Done."})
    lab = Lab(binary, plugin, turns, {})
    try:
        lab.send("Work on it until it is finished.")
        lab.wait(lambda: lab.runs()["total"] >= 6, timeout=180, label="six runs")

        def no_cap():
            goal = lab.active_goal()
            need(goal is not None, "the goal was not written")
            need(goal["status"] == "active", f"status {goal['status']}: {goal.get('pause_reason')}")
            need(
                goal["budget"]["max_continuations"] == 0
                and goal["budget"]["max_minutes"] == 0,
                f"budget: {goal['budget']}",
            )
            need(
                goal["progress"]["continuations"] >= 5,
                f"{goal['progress']['continuations']} continuations",
            )
            return f"{goal['progress']['continuations']} resumes, no cap, no pause"

        check("the goal is still active after six resumes, with no cap recorded", no_cap)

        goal = lab.wait_goal("done", timeout=180)

        def closed_after_many():
            need(lab.runs()["total"] >= 7, f"{lab.runs()['total']} runs: {lab.runs()}")
            need(goal["status"] == "done", goal["status"])
            marker = [
                m
                for r in lab.requests()
                for m in r["messages"]
                if "SCRIPT-EXHAUSTED" in (m.get("content") or "")
            ]
            need(not marker, "the script ran past its turns")
            return f"{lab.runs()['total']} runs, closed cleanly"

        check("it keeps going indefinitely and stops the moment the goal is done", closed_after_many)
    finally:
        lab.cleanup()


def scenario_capped(binary, plugin, keep):
    """An explicit cap still pauses: a guard rail a human set, not a default."""
    print("\n== scenario: capped ==", flush=True)
    turns = [
        goal_set("Loop forever", ["never true"]),
        {"text": "Stopped. Nothing recorded."},
        {"text": "Stopped again. Nothing recorded."},
        {"text": "Stopped a third time. Nothing recorded."},
        {"text": "SCRIPT-EXHAUSTED: the cap was not honoured"},
    ]
    lab = Lab(binary, plugin, turns, {"max_continuations": 2, "max_stalled_resumes": 9})
    try:
        lab.send("Work on it forever.")
        goal = lab.wait_goal("paused", timeout=120)

        def budgeted():
            runs = lab.runs()
            need(runs["total"] == 3, f"{runs['total']} runs: {runs}")
            need(
                goal["progress"]["continuations"] == 2,
                f"{goal['progress']['continuations']} continuations",
            )
            return f"{runs['total']} runs, 2 continuations"

        check("exactly the budgeted number of resumptions ran", budgeted)

        def named():
            need("continuation cap" in goal["pause_reason"], goal["pause_reason"])
            return goal["pause_reason"]

        check("the pause names the cap, not something vague", named)

        def never_past_the_script():
            marker = [
                m
                for r in lab.requests()
                for m in r["messages"]
                if "SCRIPT-EXHAUSTED" in (m.get("content") or "")
            ]
            need(not marker, "the script ran past its budget")
            return "bounded"

        check("the exhaustion marker never appears", never_past_the_script)
    finally:
        lab.cleanup()


def scenario_stall(binary, plugin, keep):
    """A model that does nothing at all is paused, not resumed forever."""
    print("\n== scenario: stall ==", flush=True)
    turns = [goal_set("Do nothing", ["never true"])]
    for i in range(8):
        turns.append({"text": f"Nothing at all ({i + 1})."})
    lab = Lab(binary, plugin, turns, {"max_stalled_resumes": 2})
    try:
        lab.send("Stand very still.")
        goal = lab.wait_goal("paused", timeout=120)

        def paused_for_doing_nothing():
            need("did nothing" in (goal["pause_reason"] or ""), goal["pause_reason"])
            need(goal["progress"]["continuations"] <= 3, f"{goal['progress']}")
            return goal["pause_reason"][:100]

        check("a model that only says it is done is paused, not resumed forever", paused_for_doing_nothing)

        def idle_and_recoverable():
            time.sleep(3)
            need(not lab.info()["running"], "still running")
            need(goal["progress"]["engine_note"], "the engine left no note")
            return "paused; goal_resume would re-arm it"

        check("the pause is the engine's, and the goal is kept", idle_and_recoverable)
    finally:
        lab.cleanup()


def scenario_checks(binary, plugin, keep):
    """A criterion judged by a command, end to end."""
    print("\n== scenario: checks ==", flush=True)
    turns = [
        goal_set(
            "Make the file",
            [
                {"text": "made.txt exists", "check": "test -f made.txt"},
                {"text": "it says hi", "check": "grep -qx hi made.txt"},
            ],
        ),
        {"text": "Recorded; stopping before doing anything."},
        builtin("run_command", {"command": "printf 'hi\\n' > made.txt"}),
        call("goal_check", {}),
        call("goal_complete", {"summary": "the commands agreed"}),
        {"text": "Done."},
        {"text": "SCRIPT-EXHAUSTED: this run must never happen"},
    ]
    lab = Lab(
        binary,
        plugin,
        turns,
        {"verify_commands": True},
        permissions=("files.read", "files.write", "network", "shell"),
    )
    try:
        def ready():
            entry = lab.plugin() or {}
            names = [tool["name"] for tool in entry["tools"]]
            need("goal_check" in names, f"goal_check missing: {names}")
            need(len(names) == 10, f"{len(names)} tools: {names}")
            return ", ".join(names)

        check("with `shell` granted the tenth tool appears", ready)

        lab.send("Make made.txt and prove it.")
        goal = lab.wait_goal("done", timeout=150)

        def evidenced_by_commands():
            for criterion in goal["criteria"]:
                need(criterion["met"], f"{criterion['id']} is not met")
                need(criterion.get("check_ok") is True, f"{criterion['id']}: {criterion}")
                need(
                    "verified by" in (criterion.get("evidence") or ""),
                    f"{criterion['id']} evidence: {criterion.get('evidence')}",
                )
            return "; ".join(c["evidence"][:40] for c in goal["criteria"])

        check("both criteria were closed by a passing command", evidenced_by_commands)

        def transcript_shows_the_verdicts():
            messages = lab.client.ok("GET", f"/api/sessions/{lab.session}")["messages"]
            text = "\n".join((m.get("content") or "") for m in messages)
            need("PASS  c1" in text, "no PASS for c1 in the transcript")
            need("PASS  c2" in text, "no PASS for c2 in the transcript")
            return "PASS c1 + PASS c2 in the transcript"

        check("the check verdicts are visible in the conversation", transcript_shows_the_verdicts)

        def exactly_two_runs():
            time.sleep(4)
            need(lab.runs()["total"] == 2, f"{lab.runs()['total']} runs")
            return "2 runs"

        check("the loop stopped after the goal closed", exactly_two_runs)
    finally:
        lab.cleanup()


def scenario_cancel(binary, plugin, keep):
    """A human pressing stop outranks the engine, and is not fought."""
    print("\n== scenario: cancel ==", flush=True)
    turns = [
        goal_set("Cancel me", ["never true"]),
        {"sleep": 10, "text": "still thinking about it"},
        {"text": "SCRIPT-EXHAUSTED: a cancelled run must not be continued"},
    ]
    lab = Lab(binary, plugin, turns, {"max_continuations": 5})
    try:
        lab.send("Do the thing.")
        # Cancel *after* the goal exists: a cancel that lands before `goal_set` ran
        # would leave nothing to pause, and would test the wrong thing.
        lab.wait(lambda: lab.active_goal() is not None, timeout=30,
                 label="the goal to be recorded")
        lab.wait(lambda: lab.info()["running"], timeout=30, label="a running run")
        status, _ = lab.client.raw("POST", f"/api/sessions/{lab.session}/cancel", {})
        need(status in (200, 202, 204), f"cancel answered {status}")
        goal = lab.wait_goal("paused", timeout=60)

        def not_fought():
            need("cancelled" in (goal["pause_reason"] or ""), goal["pause_reason"])
            need(lab.runs()["total"] == 1, f"{lab.runs()['total']} runs: it restarted anyway")
            marker = [m for r in lab.requests() for m in r["messages"]
                      if "SCRIPT-EXHAUSTED" in (m.get("content") or "")]
            need(not marker, "the cancel was fought")
            return (goal["pause_reason"] or "")[:110]

        check("a cancelled run pauses the goal instead of restarting it", not_fought)

        def resumable():
            need(goal["status"] == "paused", goal["status"])
            need(goal["progress"]["engine_note"], "the engine left no note")
            return f"engine_note: {(goal['progress']['engine_note'] or '')[:80]}"

        check("the pause is attributed to the engine and is resumable", resumable)
    finally:
        lab.cleanup()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default=str(DEFAULT_BINARY))
    parser.add_argument("--plugin-bin", default=str(DEFAULT_PLUGIN))
    parser.add_argument("--scenario", default="all",
                        choices=sorted(SCENARIOS | {"all"}))
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()

    binary = Path(args.binary)
    plugin = Path(args.plugin_bin)
    for path, label in ((binary, "karucode binary"), (plugin, "plugin binary")):
        if not path.exists():
            print(f"missing {label}: {path}", file=sys.stderr)
            return 2
    print(f"karucode: {binary}\nplugin:   {plugin}", flush=True)

    scenarios = {
        "stop-early": scenario_stop_early,
        "unlimited": scenario_unlimited,
        "capped": scenario_capped,
        "stall": scenario_stall,
        "checks": scenario_checks,
        "cancel": scenario_cancel,
    }
    chosen = scenarios if args.scenario == "all" else {args.scenario: scenarios[args.scenario]}
    for run in chosen.values():
        run(binary, plugin, args.keep)

    failures = [name for name, ok, _ in RESULTS if not ok]
    print(f"\n{len(RESULTS) - len(failures)}/{len(RESULTS)} checks passed", flush=True)
    for name, ok, detail in RESULTS:
        if not ok:
            print(f"  FAILED: {name}: {detail}", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
