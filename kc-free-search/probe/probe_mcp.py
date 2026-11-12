#!/usr/bin/env python3
"""Probe free-search-mcp over stdio: time the handshake, dump tool schemas."""
import json, subprocess, sys, time

CMD = ["uvx", "--from", "free-search-mcp", "search-mcp"]

def rpc(proc, msg):
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()
    line = proc.stdout.readline()
    if not line:
        raise SystemExit("backend closed stdout")
    return json.loads(line)

def main():
    t0 = time.time()
    proc = subprocess.Popen(CMD, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, bufsize=0)
    init = rpc(proc, {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                      "params": {"protocolVersion": "2025-06-18",
                                 "capabilities": {},
                                 "clientInfo": {"name": "probe", "version": "0"}}})
    t_init = time.time() - t0
    proc.stdin.write((json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n").encode())
    proc.stdin.flush()
    t1 = time.time()
    tools = rpc(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    t_tools = time.time() - t1
    print(f"initialize: {t_init:.2f}s   tools/list: {t_tools:.2f}s")
    print("serverInfo:", json.dumps(init["result"].get("serverInfo")))
    print("protocolVersion:", init["result"].get("protocolVersion"))
    print("instructions:", (init["result"].get("instructions") or "")[:200])
    tl = tools["result"]["tools"]
    print(f"\n{len(tl)} tools:")
    for t in tl:
        props = list((t.get("inputSchema") or {}).get("properties", {}).keys())
        print(f"  - {t['name']:20s} {len(json.dumps(t)):6d}B  {props}")
    if len(sys.argv) > 1 and sys.argv[1] == "--dump":
        json.dump(tools["result"]["tools"], open("/tmp/fs_tools.json", "w"), indent=2)
        print("\nwrote /tmp/fs_tools.json")
    proc.terminate()

main()
