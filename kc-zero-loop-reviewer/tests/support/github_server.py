"""A tiny GitHub REST fake for the CI end-to-end test."""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


class FakeGitHub:
    def __init__(self, *, pull: dict[str, Any], diff: str = ""):
        self.pull_row = pull
        self.diff_text = diff
        self.reviews: list[dict[str, Any]] = []
        self.statuses: list[dict[str, Any]] = []
        self.requests: list[tuple[str, str, str]] = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args: Any) -> None:
                pass

            def _json(self, payload: Any, status: int = 200) -> None:
                body = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def _text(self, payload: str) -> None:
                body = payload.encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self) -> None:  # noqa: N802
                accept = self.headers.get("Accept", "")
                owner.requests.append(("GET", self.path, accept))
                if self.path.endswith("/check-runs?filter=latest"):
                    self._json({"check_runs": []})
                    return
                if self.path.endswith("/reviews"):
                    self._json([])
                    return
                if self.path.startswith("/repos/") and "/pulls/" in self.path:
                    if "diff" in accept:
                        self._text(owner.diff_text)
                    else:
                        self._json(owner.pull_row)
                    return
                self._json({"message": "not found"}, 404)

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length) or b"{}")
                owner.requests.append(("POST", self.path, ""))
                if self.path.endswith("/reviews"):
                    owner.reviews.append(body)
                    self._json({"id": 7, "html_url": "https://example.invalid/review/7"}, 201)
                    return
                if "/statuses" in self.path:
                    owner.statuses.append(body)
                    self._json({"id": 1}, 201)
                    return
                self._json({"message": "not found"}, 404)

        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._httpd.daemon_threads = True
        self._thread: threading.Thread | None = None

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self._httpd.server_address[1]}"

    def start(self) -> "FakeGitHub":
        self._thread = threading.Thread(target=self._httpd.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        self._thread.start()
        return self

    def stop(self) -> None:
        self._httpd.shutdown()
        self._httpd.server_close()
        if self._thread is not None:
            self._thread.join(timeout=5)

    def __enter__(self) -> "FakeGitHub":
        return self.start()

    def __exit__(self, *exc: Any) -> None:
        self.stop()
