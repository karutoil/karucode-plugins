#!/usr/bin/env python3
"""Reproduce `group_looks_like_a_backend` field parsing for a live group.

The plugin refused to reap groups that demonstrably contained `search-mcp`, so
this prints exactly what the /proc walk sees for one group, to compare against
what the Rust predicate computes.
"""
import sys
import os

group = int(sys.argv[1])

for entry in os.listdir("/proc"):
    if not entry.isdigit():
        continue
    try:
        with open(f"/proc/{entry}/stat") as f:
            stat = f.read()
    except OSError:
        continue
    # Rust: stat.rsplit_once(')') then split_whitespace:
    #   state ppid pgrp
    rest = stat.rsplit(")", 1)[1]
    fields = rest.split()
    state, ppid, pgrp = fields[0], fields[1], fields[2]
    if int(pgrp) != group:
        continue
    try:
        with open(f"/proc/{entry}/cmdline", "rb") as f:
            cmdline = f.read().replace(b"\0", b" ").decode("utf-8", "replace")
    except OSError:
        cmdline = "<unreadable>"
    looks = any(k in cmdline for k in ("search-mcp", "free-search-mcp", "uvx"))
    print(
        f"pid={entry} state={state} ppid={ppid} pgrp={pgrp} "
        f"looks_like_backend={looks} cmd={cmdline.strip()[:80]}"
    )
