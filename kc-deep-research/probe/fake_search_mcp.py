#!/usr/bin/env python3
"""Scripted free-search-mcp fixture: real MCP stdio, no network."""
import json
import os
import sys
import time

LOG = os.environ.get("DEEP_PROBE_LOG")
DELAY = float(os.environ.get("DEEP_PROBE_DELAY_MS", "0")) / 1000


def log(value):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"pid": os.getpid(), **value}) + "\n")


def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def page(url, error=False):
    if error or "bad" in url:
        return {"url": url, "error": "scripted per-URL fetch failure"}
    content = (
        "Fresh Rust ownership provides memory safety through ownership and borrowing. "
        "The architecture supports tools and capabilities with security, safety and consent. "
        "Pricing includes a free example with data and practical applications. "
        "Definitions and background explain limitations, criticism, risks and open questions. "
        f"This unique parsed document is available at {url}. "
    ) * 8
    return {
        "url": url,
        "title": "Fresh Rust ownership reference",
        "content": content,
        "published_date": "2026-01-15",
        "retrieved_at": "2026-02-01T00:00:00Z",
        "method": "http",
        "cache_age_seconds": 7 if not error else None,
        "truncated": False,
    }


def main():
    log({"event": "start"})
    sequence = 0
    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method", "")
        if method == "notifications/initialized":
            continue
        request_id = message.get("id")
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": request_id, "result": {
                "protocolVersion": "2025-06-18",
                "serverInfo": {"name": "scripted-search-mcp", "version": "0.17.0"},
            }})
            continue
        if method != "tools/call":
            continue
        params = message["params"]
        name = params["name"]
        args = params.get("arguments", {})
        log({"event": "call", "tool": name, "arguments": args})
        if DELAY:
            time.sleep(DELAY)
        if name == "search":
            if "Slow" in args.get("query", ""):
                time.sleep(1.0)
            sequence += 1
            results = []
            for suffix, host in [("a", f"source{sequence}a.example"),
                                 ("bad", f"bad{sequence}.example"),
                                 ("b", f"source{sequence}b.example")]:
                results.append({
                    "title": f"Rust ownership result {sequence}{suffix}",
                    "url": f"https://{host}/rust-ownership-{suffix}",
                    "snippet": "THIS SNIPPET MUST NEVER BE EVIDENCE",
                })
            result = {"results": results, "engines": ["scripted"], "cached": False}
            send({"jsonrpc": "2.0", "id": request_id, "result": {
                "structuredContent": result,
                "content": [{"type": "text", "text": json.dumps(result)}],
                "isError": False,
            }})
        elif name in ("fetch_batch", "fetch"):
            if name == "fetch_batch":
                pages = [page(url) for url in args["urls"]]
                structured = {"result": pages}
                text = json.dumps(pages)
            else:
                single = page(args["url"], error=args["url"].endswith("force-error"))
                structured = single
                text = json.dumps(single)
            send({"jsonrpc": "2.0", "id": request_id, "result": {
                "structuredContent": structured,
                "content": [{"type": "text", "text": text}],
                "isError": False,
            }})
        else:
            send({"jsonrpc": "2.0", "id": request_id,
                  "error": {"code": -32601, "message": f"unknown {name}"}})
    log({"event": "eof"})


if __name__ == "__main__":
    main()
