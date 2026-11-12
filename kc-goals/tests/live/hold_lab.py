#!/usr/bin/env python3
"""Boot a throwaway karucode with the built UI, seed one goal, and stay alive.

This is the harness for looking at the inspector panel in a real browser. It
prints the URL and the session id, then holds the process open until it is
killed, so a browser (or a person) can drive the tab while the server underneath
is real: a real plugin process, a real goal file, a real `/api/plugins/.../panels`
read.

    python3 tests/live/hold_lab.py --web-dir /home/karutoil/karucode/web/dist

Nothing about it is UI-specific: `Lab` comes from `drive.py`, so the scenario is
the same `stop-early` shape the live suite uses.
"""
import argparse
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import drive  # noqa: E402

TURNS = [
    drive.goal_set(
        "Ship the goal panel",
        [
            {"text": "the panel draws the goal", "check": "test -f panel.txt"},
            {"text": "a human can see what is left"},
        ],
    ),
    {"text": "Goal recorded. Stopping here so the plugin resumes me."},
    # Resumed once: one criterion evidenced, then the agent parks the goal so a
    # human can look at it. A stopped goal is what makes the panel's Resume
    # button the interesting one.
    drive.call("goal_progress", {"note": "wrote the renderer", "next": "check it in the browser"}),
    drive.call("goal_criterion", {"criterion": "c2", "evidence": "the panel draws the criterion list"}),
    drive.call("goal_pause", {"reason": "waiting for a human to look at the panel"}),
    {"text": "Paused for review."},
]

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--web-dir", default="/home/karutoil/karucode/web/dist")
    args = parser.parse_args()

    lab = drive.Lab(
        drive.DEFAULT_BINARY,
        drive.DEFAULT_PLUGIN,
        TURNS,
        # The stall detector is off in this fixture so the goal stays active
        # while a human looks at it: the scripted model runs out of turns, and a
        # paused goal would exercise a different part of the panel.
        {"verify_commands": True},
        keep=True,
        permissions=("files.read", "files.write", "network", "shell"),
        web_dir=args.web_dir,
    )
    try:
        lab.send("Record this goal and keep working on it until it is finished.")
        lab.wait(
            lambda: lab.active_goal() is not None
            and lab.active_goal()["status"] == "paused",
            timeout=90,
            label="a paused goal",
        )
        print(f"URL=http://127.0.0.1:{lab.port}/", flush=True)
        print(f"SESSION={lab.session}", flush=True)
        print(f"PROJECT_WORKSPACE={lab.ws}", flush=True)
        print("HOLDING", flush=True)
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        pass
    finally:
        lab.cleanup()
    return 0


if __name__ == "__main__":
    sys.exit(main())
