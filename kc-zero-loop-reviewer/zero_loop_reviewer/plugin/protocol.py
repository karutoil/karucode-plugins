"""Newline-delimited JSON-RPC 2.0 over stdin/stdout, protocol version 1.

The reader loop stays free while a tool runs, so a long review does not block
``shutdown`` and a ``plugin/settings/get`` call can be answered mid-review.
"""

from __future__ import annotations

import json
import logging
import sys
import threading
from dataclasses import dataclass, field
from typing import Any, Callable, Mapping

from .manifest import NAME, PROTOCOL, VERSION, manifest, tool_defs

LOG = logging.getLogger("zero_loop_reviewer.plugin")


@dataclass
class ToolResult:
    ok: bool
    content: str = ""
    meta: dict[str, Any] = field(default_factory=dict)


class HostError(RuntimeError):
    def __init__(self, method: str, error: Mapping[str, Any]):
        super().__init__(f"{method}: {error.get('message', 'host error')}")
        self.method = method
        self.error = dict(error)


class HostClient:
    """Requests the plugin makes to the host, over the same stream."""

    def __init__(self, writer: Callable[[dict[str, Any]], None]):
        self._writer = writer
        self._lock = threading.Lock()
        self._next_id = 0
        self._pending: dict[int, tuple[threading.Event, dict[str, Any]]] = {}

    def call(self, method: str, params: Mapping[str, Any] | None = None, timeout: float = 10.0) -> Any:
        with self._lock:
            self._next_id += 1
            request_id = self._next_id
            slot: dict[str, Any] = {}
            event = threading.Event()
            self._pending[request_id] = (event, slot)
        self._writer({"jsonrpc": "2.0", "id": request_id, "method": method, "params": dict(params or {})})
        if not event.wait(timeout):
            with self._lock:
                self._pending.pop(request_id, None)
            raise TimeoutError(f"the host did not answer {method} within {timeout}s")
        if "error" in slot:
            raise HostError(method, slot["error"])
        return slot.get("result")

    def resolve(self, message: Mapping[str, Any]) -> bool:
        request_id = message.get("id")
        if not isinstance(request_id, int):
            return False
        with self._lock:
            pending = self._pending.pop(request_id, None)
        if pending is None:
            return False
        event, slot = pending
        if "error" in message:
            slot["error"] = message["error"]
        else:
            slot["result"] = message.get("result")
        event.set()
        return True


class Plugin:
    def __init__(
        self,
        *,
        tools: Mapping[str, Callable[[dict[str, Any], dict[str, Any], HostClient], ToolResult]] | None = None,
        panels: Mapping[str, Callable[[dict[str, Any]], dict[str, Any]]] | None = None,
    ):
        self.tools = dict(tools or {})
        self.panels = dict(panels or {})

    def handle(self, method: str, params: dict[str, Any], host: HostClient) -> Any:
        if method == "initialize":
            return manifest()
        if method == "tools/list":
            return {"tools": [tool for tool in tool_defs() if tool["name"] in self.tools]}
        if method == "tools/call":
            name = str(params.get("name", ""))
            handler = self.tools.get(name)
            if handler is None:
                raise KeyError(f"unknown tool {name!r}")
            arguments = params.get("arguments")
            if not isinstance(arguments, dict):
                arguments = {}
            context = params.get("context")
            if not isinstance(context, dict):
                context = {}
            result = handler(arguments, context, host)
            return {"ok": bool(result.ok), "content": result.content, "meta": result.meta or None}
        if method == "panels/read":
            panel = str(params.get("panel", "review_status"))
            handler = self.panels.get(panel)
            if handler is None:
                raise KeyError(f"unknown panel {panel!r}")
            data = handler(params)
            return {"title": str(data.get("title", "Zero-loop review")), "data": data.get("data")}
        if method == "shutdown":
            return None
        raise KeyError(f"unknown method {method!r}")


def _json_line(value: Mapping[str, Any]) -> bytes:
    return (json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")


def serve(plugin: Plugin | None = None) -> int:
    plugin = plugin or Plugin()
    write_lock = threading.Lock()
    out = sys.stdout.buffer

    def write(message: dict[str, Any]) -> None:
        with write_lock:
            out.write(_json_line(message))
            out.flush()

    host = HostClient(write)
    slots = threading.Semaphore(4)
    threads: list[threading.Thread] = []
    LOG.info("%s %s serving protocol %s", NAME, VERSION, PROTOCOL)

    def answer(request_id: Any, method: str, params: dict[str, Any]) -> None:
        try:
            result = plugin.handle(method, params, host)
        except KeyError as exc:
            write({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": str(exc)}})
        except Exception as exc:  # a tool bug is reported, never fatal to the plugin
            LOG.exception("tool call failed")
            write(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32603, "message": f"{type(exc).__name__}: {exc}"},
                }
            )
            return
        write({"jsonrpc": "2.0", "id": request_id, "result": result})

    try:
        for raw in sys.stdin.buffer:
            line = raw.decode("utf-8", "replace").strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                LOG.warning("ignoring a malformed frame")
                continue
            if not isinstance(message, dict):
                continue
            if "method" not in message:
                host.resolve(message)
                continue
            method = str(message.get("method", ""))
            request_id = message.get("id")
            params = message.get("params")
            if not isinstance(params, dict):
                params = {}
            if request_id is None:
                continue  # notifications (notifications/initialized) need no answer
            if method == "shutdown":
                write({"jsonrpc": "2.0", "id": request_id, "result": None})
                break
            # Daemon workers: a long review can never keep the process alive
            # after the host has asked it to shut down.
            slots.acquire()
            try:
                thread = threading.Thread(
                    target=_answer_and_release,
                    args=(answer, request_id, method, params, slots),
                    daemon=True,
                    name="zero-loop-tool",
                )
                thread.start()
                threads.append(thread)
            except Exception:
                slots.release()
                raise
    finally:
        # Workers are daemons; give the in-flight ones a brief moment only.
        for thread in threads:
            thread.join(timeout=0.05)
    return 0


def _answer_and_release(
    answer: Callable[[Any, str, dict[str, Any]], None],
    request_id: Any,
    method: str,
    params: dict[str, Any],
    slots: threading.Semaphore,
) -> None:
    try:
        answer(request_id, method, params)
    finally:
        slots.release()
