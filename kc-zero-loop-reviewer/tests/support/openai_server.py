"""A local OpenAI-compatible endpoint for tests.

It answers by looking for ``SCHEMA: <Name>`` in the user message, so a test can
exercise the real HTTP client, the real prompt builders and the real JSON
validation without a model.  Every request is recorded for assertions.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


class FakeOpenAI:
    def __init__(self, answers: dict[str, Any], *, model: str = "fake-deepseek"):
        self.answers = answers
        self.model = model
        self.requests: list[dict[str, Any]] = []
        self.failures: list[int] = []
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args: Any) -> None:  # silence
                pass

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length) or b"{}")
                owner.requests.append(
                    {
                        "path": self.path,
                        "headers": {key.lower(): value for key, value in self.headers.items()},
                        "body": body,
                    }
                )
                if owner.failures:
                    status = owner.failures.pop(0)
                    payload = json.dumps({"error": {"message": "scripted failure"}}).encode()
                    self.send_response(status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                text = ""
                for message in body.get("messages", []):
                    text += str(message.get("content", ""))
                name = owner._schema_name(text)
                answer = owner.answers.get(name)
                if answer is None:
                    status = 400
                    payload = json.dumps({"error": {"message": f"no scripted answer for schema {name}"}}).encode()
                    self.send_response(status)
                else:
                    status = 200
                    content = answer if isinstance(answer, str) else json.dumps(answer)
                    payload = json.dumps(
                        {
                            "id": "chatcmpl-test",
                            "model": owner.model,
                            "choices": [
                                {"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}
                            ],
                            "usage": {"prompt_tokens": 100, "completion_tokens": 50},
                        }
                    ).encode()
                    self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self._handler = Handler
        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._httpd.daemon_threads = True

    def _schema_name(self, text: str) -> str:
        marker = "SCHEMA: "
        index = text.find(marker)
        if index < 0:
            return ""
        rest = text[index + len(marker) :]
        return rest.split("\n", 1)[0].strip()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self._httpd.server_address[1]}/v1"

    def start(self) -> "FakeOpenAI":
        self._thread = threading.Thread(target=self._httpd.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        self._thread.start()
        return self

    def stop(self) -> None:
        self._httpd.shutdown()
        self._httpd.server_close()
        if self._thread is not None:
            self._thread.join(timeout=5)

    def __enter__(self) -> "FakeOpenAI":
        return self.start()

    def __exit__(self, *exc: Any) -> None:
        self.stop()
