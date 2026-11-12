#!/usr/bin/env python3
"""Time real tool calls through free-search-mcp, warm process."""
import json, subprocess, sys, time

CMD = ["uvx", "--from", "free-search-mcp", "search-mcp"]

def send(proc, msg):
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()

def recv(proc):
    line = proc.stdout.readline()
    if not line:
        raise SystemExit("backend closed stdout")
    return json.loads(line)

def rpc(proc, i, method, params):
    send(proc, {"jsonrpc": "2.0", "id": i, "method": method, "params": params})
    return recv(proc)

def call(proc, i, name, args):
    t = time.time()
    r = rpc(proc, i, "tools/call", {"name": name, "arguments": args})
    dt = time.time() - t
    res = r.get("result", {})
    blocks = res.get("content", [])
    texts = [b for b in blocks if b.get("type") == "text"]
    n = len(texts[0]["text"]) if texts else 0
    kinds = sorted({b.get("type") for b in blocks})
    flag = "isError" if res.get("isError") else "ok"
    print(f"{name:16s} {dt:6.2f}s  {flag:8s} blocks={kinds} text={n}B")
    if texts and n:
        print("   head:", texts[0]["text"].replace("\n", " ")[:160])
    return r

def main():
    t0 = time.time()
    proc = subprocess.Popen(CMD, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, bufsize=0)
    rpc(proc, 1, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "probe", "version": "0"}})
    send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
    print(f"handshake {time.time()-t0:.2f}s")
    i = 10
    for name, args in [
        ("search", {"query": "karucode plugin sdk"}),
        ("search", {"query": "karucode plugin sdk"}),
        ("fetch", {"url": "https://docs.rs/serde/latest/serde/"}),
        ("engines", {}),
        ("cache_search", {"query": "serde"}),
        ("research", {"question": "what is reciprocal rank fusion", "depth": 2}),
    ]:
        try:
            call(proc, i, name, args)
        except Exception as e:
            print(f"{name} FAILED: {e}")
        i += 1
    proc.terminate()

main()
