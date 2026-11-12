#!/usr/bin/env python3
"""Probe the result shapes the plugin has to translate: json format, images,
resource links, notifications, and the slowest real calls."""
import json, subprocess, time

CMD = ["uvx", "--from", "free-search-mcp", "search-mcp"]

def send(proc, msg):
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()

def recv(proc):
    """Read frames until a response arrives, reporting notifications seen."""
    notes = []
    while True:
        line = proc.stdout.readline()
        if not line:
            raise SystemExit("backend closed stdout")
        msg = json.loads(line)
        if "method" in msg and "id" not in msg:
            notes.append(msg.get("method"))
            continue
        return msg, notes

def rpc(proc, i, method, params):
    send(proc, {"jsonrpc": "2.0", "id": i, "method": method, "params": params})
    return recv(proc)

def call(proc, i, name, args):
    t = time.time()
    r, notes = rpc(proc, i, "tools/call", {"name": name, "arguments": args})
    dt = time.time() - t
    res = r.get("result", {})
    blocks = res.get("content", [])
    shape = []
    for b in blocks:
        t_ = b.get("type")
        if t_ == "text":
            shape.append(f"text:{len(b.get('text',''))}B")
        elif t_ == "image":
            shape.append(f"image:{b.get('mimeType')}:{len(b.get('data',''))}B")
        elif t_ == "resource_link":
            shape.append(f"link:{b.get('uri','')[:60]}")
        else:
            shape.append(t_)
    err = "ERROR " + json.dumps(res)[:120] if res.get("isError") else ""
    print(f"{name:16s} {dt:6.2f}s {shape} notes={sorted(set(notes))} {err}")
    if "structuredContent" in res:
        print(f"                 structuredContent keys={list(res['structuredContent'])}")
    return res

def main():
    proc = subprocess.Popen(CMD, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, bufsize=0)
    rpc(proc, 1, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "probe", "version": "0"}})
    send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
    i = 10

    print("--- json format (structuredContent?) ---")
    call(proc, i, "search", {"query": "sqlite fts5", "format": "json"}); i += 1

    print("--- image inline ---")
    call(proc, i, "fetch", {"url": "https://upload.wikimedia.org/wikipedia/commons/4/47/PNG_transparency_demonstration_1.png",
                            "inline": True}); i += 1

    print("--- image described ---")
    call(proc, i, "fetch", {"url": "https://upload.wikimedia.org/wikipedia/commons/4/47/PNG_transparency_demonstration_1.png"}); i += 1

    print("--- error shape ---")
    call(proc, i, "fetch", {"url": "http://127.0.0.1:9/nope"}); i += 1
    call(proc, i, "search", {"query": ""}); i += 1
    call(proc, i, "compare", {"question": "x", "urls": ["https://a.example"]}); i += 1

    print("--- slow calls (host call budget is 60s) ---")
    call(proc, i, "research", {"question": "state of the art in retrieval augmented generation 2026", "depth": 3}); i += 1
    call(proc, i, "paper_graph", {"paper": "arXiv:1706.03762", "limit": 5}); i += 1
    proc.terminate()

main()
