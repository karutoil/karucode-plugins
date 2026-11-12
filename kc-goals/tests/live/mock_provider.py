#!/usr/bin/env python3
"""A scripted OpenAI-compatible endpoint for driving kc-goals end to end.

No third-party dependencies. Behaviour comes from a JSON script, one entry per
model turn:

    {"call": {"name": "goal_set", "arguments": {...}}}   one tool call
    {"calls": [{"name": ..., "arguments": {...}}, ...]}   several in one turn
    {"text": "..."}                                       plain answer (a stop)
    {"sleep": 10, "text": "..."}                          hold the run open, then answer

The turn index is the number of assistant messages already in the request, so the
script is deterministic without the server telling us anything: turn 0 is the
first model call, turn 1 is the call after its tool result, and so on. Every
request is appended to the log as JSONL, which is how the driver checks what the
plugin actually put in the prompt.

Deliberately not clever: if the script runs out it answers with a marker text, so
a runaway continuation shows up as a run rather than as a hang.
"""

import argparse
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
SCRIPT = []
LOG_LOCK = threading.Lock()


def log_request(record):
    if not ARGS.log:
        return
    record["ts"] = int(time.time() * 1000)
    with LOG_LOCK:
        with open(ARGS.log, "a") as handle:
            handle.write(json.dumps(record) + "\n")


def textual_content(message):
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(
            part.get("text", "") for part in content if isinstance(part, dict)
        )
    return ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.path in ("/health", "/healthz"):
            self._json(200, {"ok": True})
        elif self.path.rstrip("/") == "/v1/models":
            self._json(200, {"data": [{"id": "mock-coder", "object": "model"}]})
        else:
            self._json(404, {"error": "not found"})

    def _json(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            payload = json.loads(raw)
        except Exception:
            payload = {}
        messages = payload.get("messages") or []
        turn = sum(1 for m in messages if m.get("role") == "assistant")
        entry = SCRIPT[turn] if turn < len(SCRIPT) else {"text": "SCRIPT-EXHAUSTED"}
        log_request(
            {
                "turn": turn,
                "entry": entry,
                "messages": [
                    {
                        "role": m.get("role"),
                        "content": textual_content(m)[:6000],
                        "tool_calls": [
                            c.get("function", {}).get("name")
                            for c in (m.get("tool_calls") or [])
                        ],
                    }
                    for m in messages
                ],
            }
        )
        sleep = entry.get("sleep")
        if sleep:
            time.sleep(float(sleep))
        self._stream(payload.get("model", "mock-coder"), entry)

    def _stream(self, model, entry):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def send(obj):
            data = f"data: {json.dumps(obj)}\n\n".encode()
            try:
                self.wfile.write(hex(len(data))[2:].encode() + b"\r\n" + data + b"\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                raise ClientGone()

        def base(delta, finish=None):
            return {
                "id": "chatcmpl-goals",
                "object": "chat.completion.chunk",
                "created": int(time.time()),
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }

        calls = []
        if "call" in entry:
            calls = [entry["call"]]
        elif "calls" in entry:
            calls = entry["calls"]
        elif "text" not in entry:
            calls = []

        try:
            send(base({"role": "assistant", "content": ""}))
            if calls:
                for index, call in enumerate(calls):
                    send(
                        base(
                            {
                                "tool_calls": [
                                    {
                                        "index": index,
                                        "id": f"call_{index}_{int(time.time() * 1000)}",
                                        "type": "function",
                                        "function": {
                                            "name": call["name"],
                                            "arguments": "",
                                        },
                                    }
                                ]
                            }
                        )
                    )
                    args = json.dumps(call.get("arguments", {}))
                    for i in range(0, len(args), 64):
                        send(
                            base(
                                {
                                    "tool_calls": [
                                        {
                                            "index": index,
                                            "function": {"arguments": args[i : i + 64]},
                                        }
                                    ]
                                }
                            )
                        )
                send(base({}, "tool_calls"))
            else:
                text = entry.get("text", "")
                for word in text.split(" "):
                    send(base({"content": word + " "}))
                send(base({}, "stop"))

            send(
                {
                    "id": "chatcmpl-goals",
                    "object": "chat.completion.chunk",
                    "created": int(time.time()),
                    "model": model,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 64,
                        "completion_tokens": 8,
                        "total_tokens": 72,
                        "prompt_tokens_details": {"cached_tokens": 32},
                    },
                }
            )
            data = b"data: [DONE]\n\n"
            self.wfile.write(hex(len(data))[2:].encode() + b"\r\n" + data + b"\r\n")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except ClientGone:
            # The driver cancelled the run; expected in the cancel scenario.
            self.close_connection = True


class ClientGone(Exception):
    pass


def main():
    global ARGS, SCRIPT
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--script", required=True, help="JSON file of turns")
    parser.add_argument("--log", default=None, help="JSONL of every request")
    ARGS = parser.parse_args()
    with open(ARGS.script) as handle:
        SCRIPT = json.load(handle)["turns"]
    server = ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler)
    server.daemon_threads = True
    print(f"mock provider on 127.0.0.1:{ARGS.port} with {len(SCRIPT)} turns", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(main() or 0)
