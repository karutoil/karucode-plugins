"""Webhook signature verification, event parsing and a loopback receiver.

Signatures
----------
* GitHub: ``X-Hub-Signature-256: sha256=<hex>`` — HMAC-SHA256 of the raw body.
* GitLab: ``X-Gitlab-Token: <secret>`` — constant-time equality, no HMAC.
* Bitbucket: ``X-Hub-Signature: sha256=<hex>`` — HMAC-SHA256 of the raw body.

Replay
------
A signed request that carries a parseable timestamp header (``X-Timestamp`` or
``X-Request-Timestamp``, epoch seconds/milliseconds or ISO-8601) outside the
300 s window is refused.  GitHub sends no timestamp, so an absent one is not an
error.

The receiver never echoes, stores or logs the payload: responses carry a status
word and, at most, a delivery id.
"""

from __future__ import annotations

import hashlib
import json
import logging
import threading
import time
from collections import OrderedDict
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable, Mapping

from .base import ProviderError, WebhookEvent, hmac_sha256, secure_equals

REPLAY_WINDOW_S = 300.0
MAX_BODY_BYTES = 8 * 1024 * 1024
PROVIDERS = ("github", "gitlab", "bitbucket")
_TIMESTAMP_HEADERS = ("x-timestamp", "x-request-timestamp")
_REVIEW_ACTIONS = {
    "github": frozenset({"opened", "reopened", "synchronize", "ready_for_review"}),
    "gitlab": frozenset({"open", "reopen", "update"}),
    "bitbucket": frozenset({"pullrequest:created", "pullrequest:updated"}),
}
LOG = logging.getLogger(__name__)


# --------------------------------------------------------------------------- #
# small helpers
# --------------------------------------------------------------------------- #


def header(headers: Mapping[str, str] | None, name: str) -> str:
    """Case-insensitive header read that also works for ``email.message.Message``."""
    if headers is None:
        return ""
    try:
        value = headers.get(name)  # type: ignore[arg-type]
    except (AttributeError, TypeError):  # pragma: no cover - defensive
        value = None
    if value is not None:
        return str(value or "")
    target = name.lower()
    try:
        items = headers.items()
    except AttributeError:  # pragma: no cover - defensive
        return ""
    for key, candidate in items:
        if str(key).lower() == target:
            return str(candidate or "")
    return ""


def _digest_matches(secret: str, body: bytes, value: str) -> bool:
    if not secret or not value:
        return False
    candidate = value.strip()
    if candidate.lower().startswith("sha256="):
        candidate = candidate[len("sha256=") :]
    return secure_equals(hmac_sha256(secret, body), candidate.strip().lower())


def _parse_timestamp(raw: str) -> float | None:
    text = raw.strip()
    if not text:
        return None
    try:
        value = float(text)
    except ValueError:
        iso = text[:-1] + "+00:00" if text.endswith(("Z", "z")) else text
        try:
            parsed = datetime.fromisoformat(iso)
        except ValueError:
            return None
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed.timestamp()
    if value > 1e11:  # milliseconds
        value /= 1000.0
    return value


def _timestamp(headers: Mapping[str, str] | None) -> float | None:
    for name in _TIMESTAMP_HEADERS:
        stamp = _parse_timestamp(header(headers, name))
        if stamp is not None:
            return stamp
    return None


# --------------------------------------------------------------------------- #
# verification
# --------------------------------------------------------------------------- #


def verify(
    provider: str,
    secret: str,
    body: bytes,
    headers: Mapping[str, str] | None,
    *,
    now: float | None = None,
    replay_window_s: float = REPLAY_WINDOW_S,
) -> bool:
    """True only for a correctly signed, non-replayed delivery."""
    name = str(provider or "").strip().lower()
    raw = bytes(body or b"")
    if not secret:
        return False
    if name == "github":
        signed = _digest_matches(secret, raw, header(headers, "x-hub-signature-256"))
    elif name == "gitlab":
        # The token is the shared secret itself, compared in constant time.
        signed = secure_equals(secret, header(headers, "x-gitlab-token"))
    elif name == "bitbucket":
        signed = _digest_matches(secret, raw, header(headers, "x-hub-signature"))
    else:
        return False
    if not signed:
        return False
    stamp = _timestamp(headers)
    if stamp is None:
        return True
    current = time.time() if now is None else float(now)
    return abs(current - stamp) <= float(replay_window_s)


# --------------------------------------------------------------------------- #
# parsing
# --------------------------------------------------------------------------- #


def _payload(body: bytes) -> dict[str, Any]:
    try:
        decoded = body.decode("utf-8") if isinstance(body, (bytes, bytearray)) else str(body)
        payload = json.loads(decoded)
    except (UnicodeDecodeError, ValueError, TypeError):
        raise ProviderError("webhook: body is not valid JSON") from None
    if not isinstance(payload, dict):
        raise ProviderError("webhook: body is not a JSON object")
    return payload


def _dict(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _str(value: Any) -> str:
    return str(value) if value is not None else ""


def _number(value: Any) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def _delivery(names: tuple[str, ...], headers: Mapping[str, str] | None) -> str:
    for name in names:
        value = header(headers, name)
        if value:
            return value
    return ""


def _parse_github(payload: dict[str, Any], headers: Mapping[str, str] | None) -> WebhookEvent:
    event_name = header(headers, "x-github-event") or _str(payload.get("event"))
    action = _str(payload.get("action"))
    pull = _dict(payload.get("pull_request"))
    head = _dict(pull.get("head"))
    base = _dict(pull.get("base"))
    common = dict(
        repository=_str(_dict(payload.get("repository")).get("full_name")),
        number=_number(pull.get("number") or payload.get("number")),
        head_sha=_str(head.get("sha")),
        base_ref=_str(base.get("ref")),
        action=action,
        delivery_id=_delivery(("x-github-delivery",), headers),
        draft=bool(pull.get("draft")),
    )
    if event_name and event_name != "pull_request":
        return WebhookEvent(kind="ignore", reason=f"event:{event_name}", **common)
    if common["draft"]:
        return WebhookEvent(kind="ignore", reason="draft", **common)
    if action not in _REVIEW_ACTIONS["github"]:
        return WebhookEvent(kind="ignore", reason=f"action:{action or 'unknown'}", **common)
    if not common["head_sha"]:
        return WebhookEvent(kind="ignore", reason="no head sha", **common)
    return WebhookEvent(kind="review", **common)


def _parse_gitlab(payload: dict[str, Any], headers: Mapping[str, str] | None) -> WebhookEvent:
    event_name = header(headers, "x-gitlab-event")
    kind = _str(payload.get("object_kind") or payload.get("event_type"))
    attrs = _dict(payload.get("object_attributes"))
    action = _str(attrs.get("action") or payload.get("event_type"))
    last_commit = _dict(attrs.get("last_commit"))
    draft = bool(attrs.get("draft") or attrs.get("work_in_progress"))
    common = dict(
        repository=_str(_dict(payload.get("project")).get("path_with_namespace")),
        number=_number(attrs.get("iid") or payload.get("iid")),
        head_sha=_str(last_commit.get("id") or attrs.get("sha")),
        base_ref=_str(attrs.get("target_branch")),
        action=action,
        delivery_id=_delivery(("x-request-id", "x-gitlab-event-uuid"), headers),
        draft=draft,
    )
    if event_name and event_name.strip().lower() != "merge request hook":
        return WebhookEvent(kind="ignore", reason=f"event:{event_name}", **common)
    if kind and kind != "merge_request":
        return WebhookEvent(kind="ignore", reason=f"object:{kind}", **common)
    if draft:
        return WebhookEvent(kind="ignore", reason="draft", **common)
    if action not in _REVIEW_ACTIONS["gitlab"]:
        return WebhookEvent(kind="ignore", reason=f"action:{action or 'unknown'}", **common)
    if not common["head_sha"]:
        return WebhookEvent(kind="ignore", reason="no head sha", **common)
    return WebhookEvent(kind="review", **common)


def _parse_bitbucket(payload: dict[str, Any], headers: Mapping[str, str] | None) -> WebhookEvent:
    event_key = (header(headers, "x-event-key") or _str(payload.get("event"))).strip().lower()
    pull = _dict(payload.get("pullrequest"))
    source = _dict(pull.get("source"))
    destination = _dict(pull.get("destination"))
    common = dict(
        repository=_str(_dict(payload.get("repository")).get("full_name")),
        number=_number(pull.get("id")),
        head_sha=_str(_dict(source.get("commit")).get("hash")),
        base_ref=_str(_dict(destination.get("branch")).get("name")),
        action=event_key.split(":", 1)[1] if ":" in event_key else event_key,
        delivery_id=_delivery(("x-request-uuid", "x-hook-uuid"), headers),
        draft=bool(pull.get("draft")),
    )
    if event_key not in _REVIEW_ACTIONS["bitbucket"]:
        return WebhookEvent(kind="ignore", reason=f"event:{event_key or 'unknown'}", **common)
    if common["draft"]:
        return WebhookEvent(kind="ignore", reason="draft", **common)
    if not common["head_sha"]:
        return WebhookEvent(kind="ignore", reason="no head sha", **common)
    return WebhookEvent(kind="review", **common)


def parse(provider: str, body: bytes, headers: Mapping[str, str] | None = None) -> WebhookEvent:
    """Map a provider payload onto :class:`WebhookEvent`.  Raises ``ProviderError``
    for an unparseable body; verification is a separate step."""
    name = str(provider or "").strip().lower()
    payload = _payload(body)
    if name == "github":
        return _parse_github(payload, headers)
    if name == "gitlab":
        return _parse_gitlab(payload, headers)
    if name == "bitbucket":
        return _parse_bitbucket(payload, headers)
    raise ProviderError(f"webhook: unknown provider {provider!r}")


# --------------------------------------------------------------------------- #
# receiver
# --------------------------------------------------------------------------- #


class _Handler(BaseHTTPRequestHandler):
    """One request at a time, on a :class:`ThreadingHTTPServer`."""

    server_version = "zero-loop-reviewer-webhook/1.0"
    sys_version = ""

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002 - stdlib name
        # Deliberately silent: request lines are fine, payloads never reach it.
        return None

    def _respond(self, status: int, payload: dict[str, Any] | None = None) -> None:
        data = b"" if payload is None else json.dumps(payload).encode("utf-8")
        self.send_response(status)
        if data:
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if data:
            self.wfile.write(data)

    def _read_body(self) -> bytes | None:
        raw_length = self.headers.get("Content-Length")
        try:
            length = int(raw_length or "")
        except (TypeError, ValueError):
            return None
        if length < 0 or length > MAX_BODY_BYTES:
            return None
        return self.rfile.read(length)

    def _not_allowed(self) -> None:
        self._respond(405, {"status": "method_not_allowed"})

    do_GET = _not_allowed
    do_HEAD = _not_allowed
    do_PUT = _not_allowed
    do_DELETE = _not_allowed
    do_PATCH = _not_allowed

    def do_POST(self) -> None:  # noqa: N802 - stdlib name
        owner: WebhookServer = self.server.owner  # type: ignore[attr-defined]
        body = self._read_body()
        if body is None:
            self._respond(400, {"status": "bad_request"})
            return
        if not verify(
            owner.provider,
            owner.secret,
            body,
            self.headers,
            now=owner.now(),
            replay_window_s=owner.replay_window_s,
        ):
            self._respond(401, {"status": "bad_signature"})
            return
        try:
            event = parse(owner.provider, body, self.headers)
        except ProviderError:
            self._respond(400, {"status": "bad_request"})
            return
        if event.kind != "review":
            self._respond(204)
            return
        if not owner.claim(event):
            self._respond(204)
            return
        owner.dispatch(event)
        self._respond(202, {"status": "accepted", "delivery_id": event.delivery_id})


class WebhookServer:
    """Loopback webhook receiver: verify, de-duplicate, hand off, answer fast.

    ``handler`` runs on a background thread so a slow review cannot hold the
    provider's delivery open.  The receiver binds ``127.0.0.1`` by default and
    refuses a non-loopback bind without a secret.
    """

    def __init__(
        self,
        host: str = "127.0.0.1",
        port: int = 0,
        *,
        secret: str,
        provider: str,
        handler: Callable[[WebhookEvent], None],
        replay_window_s: float = REPLAY_WINDOW_S,
        dedupe_capacity: int = 4096,
        clock: Callable[[], float] | None = None,
    ):
        name = str(provider or "").strip().lower()
        if name not in PROVIDERS:
            raise ProviderError(f"webhook: unknown provider {provider!r}")
        if not secret and not _is_loopback(host):
            raise ProviderError(
                f"webhook: refusing to bind {host!r} without a secret; set ci.webhook_secret_env"
            )
        if not callable(handler):
            raise ProviderError("webhook: handler must be callable")
        self.host = host
        self._requested_port = int(port)
        self.secret = secret
        self.provider = name
        self.handler = handler
        self.replay_window_s = float(replay_window_s)
        self.clock = clock
        self._dedupe_capacity = max(1, int(dedupe_capacity))
        self._seen: OrderedDict[str, None] = OrderedDict()
        self._lock = threading.Lock()
        self._httpd: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None
        self.last_error: BaseException | None = None

    # -- lifecycle ---------------------------------------------------------
    def start(self) -> None:
        if self._httpd is not None:
            return
        httpd = ThreadingHTTPServer((self.host, self._requested_port), _Handler)
        httpd.daemon_threads = True
        httpd.owner = self  # type: ignore[attr-defined]
        self._httpd = httpd
        self._thread = threading.Thread(
            target=httpd.serve_forever,
            kwargs={"poll_interval": 0.02},
            name="zero-loop-webhook",
            daemon=True,
        )
        self._thread.start()

    def stop(self) -> None:
        httpd, thread = self._httpd, self._thread
        self._httpd = None
        self._thread = None
        if httpd is not None:
            httpd.shutdown()
            httpd.server_close()
        if thread is not None:
            thread.join(timeout=5.0)

    def __enter__(self) -> "WebhookServer":
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.stop()

    @property
    def port(self) -> int:
        if self._httpd is None:
            return self._requested_port
        return int(self._httpd.server_address[1])

    @property
    def url(self) -> str:
        host = "127.0.0.1" if self.host in ("0.0.0.0", "") else self.host
        return f"http://{host}:{self.port}/"

    def now(self) -> float:
        return float(time.time() if self.clock is None else self.clock())

    # -- delivery bookkeeping ---------------------------------------------
    def claim(self, event: WebhookEvent) -> bool:
        """True the first time this delivery is seen, False for a replay/duplicate."""
        key = event.delivery_id or hashlib.sha256(
            f"{event.repository}|{event.number}|{event.head_sha}|{event.action}".encode("utf-8")
        ).hexdigest()
        with self._lock:
            if key in self._seen:
                return False
            self._seen[key] = None
            while len(self._seen) > self._dedupe_capacity:
                self._seen.popitem(last=False)
            return True

    def dispatch(self, event: WebhookEvent) -> None:
        thread = threading.Thread(
            target=self._run,
            args=(event,),
            name="zero-loop-webhook-handler",
            daemon=True,
        )
        thread.start()

    def _run(self, event: WebhookEvent) -> None:
        try:
            self.handler(event)
        except Exception as exc:  # noqa: BLE001 - a handler bug must not kill the receiver
            self.last_error = exc
            LOG.exception("webhook handler failed for delivery %s", event.delivery_id or "<none>")


def _is_loopback(host: str) -> bool:
    value = str(host or "").strip().lower()
    return value in ("localhost", "::1", "[::1]") or value.startswith("127.")
