"""Webhook signature checks, event parsing and the loopback receiver.

The receiver tests bind a real socket on 127.0.0.1:0 and are marked ``network``.
"""

from __future__ import annotations

import hashlib
import hmac
import http.client
import json
import threading
import time
from datetime import datetime, timedelta, timezone
from typing import Any, Callable, Iterator

import pytest

from zero_loop_reviewer.providers import webhook
from zero_loop_reviewer.providers.base import ProviderError, WebhookEvent

SECRET = "hook-secret"
HEAD = "a" * 40
BASE = "b" * 40


# --------------------------------------------------------------------------- #
# helpers
# --------------------------------------------------------------------------- #


def encode(payload: Any) -> bytes:
    return json.dumps(payload).encode("utf-8")


def digest(secret: str, body: bytes) -> str:
    return hmac.new(secret.encode("utf-8"), body, hashlib.sha256).hexdigest()


def github_headers(body: bytes, secret: str = SECRET, **extra: str) -> dict[str, str]:
    return {
        "X-GitHub-Event": "pull_request",
        "X-GitHub-Delivery": "delivery-1",
        "X-Hub-Signature-256": f"sha256={digest(secret, body)}",
        **extra,
    }


def gitlab_headers(secret: str = SECRET, **extra: str) -> dict[str, str]:
    return {
        "X-Gitlab-Event": "Merge Request Hook",
        "X-Gitlab-Token": secret,
        "X-Request-Id": "req-1",
        **extra,
    }


def bitbucket_headers(body: bytes, secret: str = SECRET, **extra: str) -> dict[str, str]:
    return {
        "X-Event-Key": "pullrequest:created",
        "X-Request-UUID": "uuid-1",
        "X-Hub-Signature": f"sha256={digest(secret, body)}",
        **extra,
    }


def github_payload(
    *,
    action: str = "opened",
    draft: bool = False,
    head_sha: str = HEAD,
    title: str = "Add cache",
) -> dict[str, Any]:
    return {
        "action": action,
        "number": 5,
        "pull_request": {
            "number": 5,
            "title": title,
            "draft": draft,
            "head": {"ref": "feat/cache", "sha": head_sha},
            "base": {"ref": "main", "sha": BASE},
        },
        "repository": {"full_name": "o/r"},
        "sender": {"login": "toil"},
    }


def gitlab_payload(
    *,
    action: str = "open",
    draft: bool = False,
    head_sha: str = HEAD,
    object_kind: str = "merge_request",
) -> dict[str, Any]:
    return {
        "object_kind": object_kind,
        "project": {"path_with_namespace": "group/sub/proj"},
        "object_attributes": {
            "iid": 3,
            "action": action,
            "draft": draft,
            "work_in_progress": draft,
            "target_branch": "main",
            "source_branch": "feat/cache",
            "last_commit": {"id": head_sha},
            "url": "https://gitlab.test/group/sub/proj/-/merge_requests/3",
        },
        "user": {"username": "toil"},
    }


def bitbucket_payload(
    *,
    draft: bool = False,
    head_sha: str = HEAD,
) -> dict[str, Any]:
    return {
        "repository": {"full_name": "ws/slug"},
        "pullrequest": {
            "id": 12,
            "draft": draft,
            "source": {"branch": {"name": "feat/cache"}, "commit": {"hash": head_sha}},
            "destination": {"branch": {"name": "main"}, "commit": {"hash": BASE}},
        },
        "actor": {"nickname": "toil"},
    }


# --------------------------------------------------------------------------- #
# verify
# --------------------------------------------------------------------------- #


def test_github_signature_accepts_the_hmac_and_rejects_anything_else() -> None:
    body = encode(github_payload())
    headers = github_headers(body)

    assert webhook.verify("github", SECRET, body, headers) is True
    assert webhook.verify("github", "other-secret", body, headers) is False
    assert webhook.verify("github", SECRET, body + b" ", headers) is False
    assert webhook.verify("github", SECRET, body, {}) is False
    assert webhook.verify("github", "", body, headers) is False
    assert webhook.verify("github", SECRET, body, {"X-Hub-Signature-256": "sha256=deadbeef"}) is False


def test_github_signature_accepts_a_bare_digest() -> None:
    body = encode(github_payload())
    headers = {"X-Hub-Signature-256": digest(SECRET, body)}

    assert webhook.verify("github", SECRET, body, headers) is True


def test_gitlab_token_is_compared_without_an_hmac() -> None:
    body = encode(gitlab_payload())

    assert webhook.verify("gitlab", SECRET, body, gitlab_headers()) is True
    assert webhook.verify("gitlab", SECRET, body, gitlab_headers("wrong")) is False
    assert webhook.verify("gitlab", SECRET, body, {}) is False
    # A GitHub-style HMAC header is not a GitLab credential.
    assert (
        webhook.verify("gitlab", SECRET, body, {"X-Gitlab-Token": f"sha256={digest(SECRET, body)}"})
        is False
    )


def test_bitbucket_signature() -> None:
    body = encode(bitbucket_payload())
    headers = bitbucket_headers(body)

    assert webhook.verify("bitbucket", SECRET, body, headers) is True
    assert webhook.verify("bitbucket", "other", body, headers) is False
    assert webhook.verify("bitbucket", SECRET, body, {"X-Hub-Signature": "sha256=00"}) is False


def test_unknown_provider_never_verifies() -> None:
    body = b"{}"
    assert webhook.verify("gitea", SECRET, body, github_headers(body)) is False


def test_replay_window_with_a_timestamp_header() -> None:
    body = encode(github_payload())
    now = 1_700_000_000.0

    fresh = github_headers(body, **{"X-Timestamp": str(int(now - 10))})
    stale = github_headers(body, **{"X-Timestamp": str(int(now - 301))})
    future = github_headers(body, **{"X-Timestamp": str(int(now + 301))})
    no_timestamp = github_headers(body)

    assert webhook.verify("github", SECRET, body, fresh, now=now) is True
    assert webhook.verify("github", SECRET, body, stale, now=now) is False
    assert webhook.verify("github", SECRET, body, future, now=now) is False
    assert webhook.verify("github", SECRET, body, no_timestamp, now=now) is True


def test_replay_window_accepts_iso_and_millisecond_timestamps() -> None:
    body = encode(github_payload())
    now = datetime.now(timezone.utc)
    iso = (now - timedelta(seconds=5)).isoformat().replace("+00:00", "Z")
    millis = str(int((now - timedelta(seconds=5)).timestamp() * 1000))

    assert webhook.verify("github", SECRET, body, github_headers(body, **{"X-Timestamp": iso})) is True
    assert (
        webhook.verify("github", SECRET, body, github_headers(body, **{"X-Timestamp": millis})) is True
    )
    # unparseable timestamps are ignored, not fatal
    assert (
        webhook.verify("github", SECRET, body, github_headers(body, **{"X-Timestamp": "not-a-time"}))
        is True
    )


# --------------------------------------------------------------------------- #
# parse
# --------------------------------------------------------------------------- #


def test_github_parse_review_event() -> None:
    body = encode(github_payload(action="synchronize"))

    event = webhook.parse("github", body, github_headers(body))

    assert event == WebhookEvent(
        kind="review",
        repository="o/r",
        number=5,
        head_sha=HEAD,
        base_ref="main",
        action="synchronize",
        delivery_id="delivery-1",
        reason="",
        draft=False,
    )


def test_github_ignores_drafts_other_actions_and_other_events() -> None:
    draft_body = encode(github_payload(draft=True))
    draft = webhook.parse("github", draft_body, github_headers(draft_body))
    assert draft.kind == "ignore"
    assert draft.reason == "draft"
    assert draft.draft is True

    closed_body = encode(github_payload(action="closed"))
    closed = webhook.parse("github", closed_body, github_headers(closed_body))
    assert closed.kind == "ignore"
    assert closed.reason == "action:closed"

    comment_body = encode({"action": "created", "issue": {"number": 5}})
    commented = webhook.parse(
        "github", comment_body, {"X-GitHub-Event": "issue_comment", "X-GitHub-Delivery": "d-2"}
    )
    assert commented.kind == "ignore"
    assert commented.reason == "event:issue_comment"

    empty_body = encode(github_payload(head_sha=""))
    empty = webhook.parse("github", empty_body, github_headers(empty_body))
    assert empty.kind == "ignore"
    assert empty.reason == "no head sha"


def test_gitlab_parse_review_event() -> None:
    for action in ("open", "reopen", "update"):
        body = encode(gitlab_payload(action=action))
        event = webhook.parse("gitlab", body, gitlab_headers())
        assert event.kind == "review", action
        assert event.repository == "group/sub/proj"
        assert event.number == 3
        assert event.head_sha == HEAD
        assert event.base_ref == "main"
        assert event.action == action
        assert event.delivery_id == "req-1"


def test_gitlab_ignores_drafts_merges_and_other_events() -> None:
    draft_body = encode(gitlab_payload(draft=True))
    draft = webhook.parse("gitlab", draft_body, gitlab_headers())
    assert (draft.kind, draft.reason) == ("ignore", "draft")

    merge_body = encode(gitlab_payload(action="merge"))
    merge = webhook.parse("gitlab", merge_body, gitlab_headers())
    assert (merge.kind, merge.reason) == ("ignore", "action:merge")

    push_body = encode(gitlab_payload(object_kind="push"))
    push = webhook.parse("gitlab", push_body, gitlab_headers())
    assert push.kind == "ignore"
    assert push.reason == "object:push"

    other = webhook.parse("gitlab", encode(gitlab_payload()), {"X-Gitlab-Event": "Push Hook"})
    assert other.kind == "ignore"
    assert other.reason == "event:Push Hook"


def test_bitbucket_parse_review_event() -> None:
    body = encode(bitbucket_payload())

    event = webhook.parse("bitbucket", body, bitbucket_headers(body))

    assert event.kind == "review"
    assert event.repository == "ws/slug"
    assert event.number == 12
    assert event.head_sha == HEAD
    assert event.base_ref == "main"
    assert event.action == "created"
    assert event.delivery_id == "uuid-1"


def test_bitbucket_ignores_other_events_and_drafts() -> None:
    body = encode(bitbucket_payload())
    updated = webhook.parse("bitbucket", body, bitbucket_headers(body, **{"X-Event-Key": "pullrequest:updated"}))
    assert (updated.kind, updated.action) == ("review", "updated")

    merged = webhook.parse("bitbucket", body, bitbucket_headers(body, **{"X-Event-Key": "pullrequest:fulfilled"}))
    assert (merged.kind, merged.reason) == ("ignore", "event:pullrequest:fulfilled")

    draft_body = encode(bitbucket_payload(draft=True))
    draft = webhook.parse("bitbucket", draft_body, bitbucket_headers(draft_body))
    assert (draft.kind, draft.reason) == ("ignore", "draft")


def test_unparseable_bodies_raise_provider_error() -> None:
    with pytest.raises(ProviderError):
        webhook.parse("github", b"{not json", {})
    with pytest.raises(ProviderError):
        webhook.parse("github", b'["a", "list"]', {})
    with pytest.raises(ProviderError):
        webhook.parse("github", b"\xff\xfe", {})
    with pytest.raises(ProviderError):
        webhook.parse("gitea", b"{}", {})


# --------------------------------------------------------------------------- #
# receiver
# --------------------------------------------------------------------------- #


class Recorder:
    """Thread-safe event sink; the receiver calls it from a background thread."""

    def __init__(self) -> None:
        self.events: list[WebhookEvent] = []
        self._lock = threading.Lock()

    def __call__(self, event: WebhookEvent) -> None:
        with self._lock:
            self.events.append(event)

    def count(self) -> int:
        with self._lock:
            return len(self.events)

    def wait(self, count: int = 1, timeout: float = 5.0) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.count() >= count:
                return True
            time.sleep(0.01)
        return False


@pytest.fixture()
def serve() -> Iterator[Callable[..., tuple[webhook.WebhookServer, Recorder]]]:
    started: list[webhook.WebhookServer] = []

    def build(
        provider: str = "github",
        secret: str = SECRET,
        clock: Callable[[], float] | None = None,
    ) -> tuple[webhook.WebhookServer, Recorder]:
        recorder = Recorder()
        server = webhook.WebhookServer(
            "127.0.0.1",
            0,
            secret=secret,
            provider=provider,
            handler=recorder,
            clock=clock,
        )
        server.start()
        started.append(server)
        return server, recorder

    yield build

    for server in started:
        server.stop()


def call(
    port: int,
    method: str,
    path: str,
    body: bytes = b"",
    headers: dict[str, str] | None = None,
) -> tuple[int, str]:
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        merged = {"Content-Type": "application/json", "Content-Length": str(len(body))}
        merged.update(headers or {})
        conn.request(method, path, body=body, headers=merged)
        response = conn.getresponse()
        return response.status, response.read().decode("utf-8")
    finally:
        conn.close()


@pytest.mark.network
def test_server_accepts_a_signed_event(serve: Callable[..., Any]) -> None:
    server, recorder = serve()
    assert server.port > 0
    body = encode(github_payload())

    status, text = call(server.port, "POST", "/webhook", body, github_headers(body))

    assert status == 202
    assert recorder.wait(1) is True
    assert recorder.events == [
        WebhookEvent(
            kind="review",
            repository="o/r",
            number=5,
            head_sha=HEAD,
            base_ref="main",
            action="opened",
            delivery_id="delivery-1",
            reason="",
            draft=False,
        )
    ]
    payload = json.loads(text)
    assert payload == {"status": "accepted", "delivery_id": "delivery-1"}
    assert "Add cache" not in text  # the payload is never echoed


@pytest.mark.network
def test_server_rejects_a_bad_signature(serve: Callable[..., Any]) -> None:
    server, recorder = serve()
    body = encode(github_payload())

    status, text = call(server.port, "POST", "/webhook", body, github_headers(body, secret="wrong"))

    assert status == 401
    assert json.loads(text) == {"status": "bad_signature"}
    assert recorder.count() == 0


@pytest.mark.network
def test_server_rejects_an_unsigned_request(serve: Callable[..., Any]) -> None:
    server, recorder = serve()
    body = encode(github_payload())

    status, _ = call(server.port, "POST", "/webhook", body, {"X-GitHub-Event": "pull_request"})

    assert status == 401
    assert recorder.count() == 0


@pytest.mark.network
def test_server_rejects_a_replayed_timestamp(serve: Callable[..., Any]) -> None:
    server, recorder = serve()
    body = encode(github_payload())
    stale = str(int(time.time()) - webhook.REPLAY_WINDOW_S - 60)

    status, text = call(server.port, "POST", "/webhook", body, github_headers(body, **{"X-Timestamp": stale}))

    assert status == 401
    assert json.loads(text) == {"status": "bad_signature"}
    assert recorder.count() == 0


@pytest.mark.network
def test_server_returns_204_for_ignored_actions(serve: Callable[..., Any]) -> None:
    server, recorder = serve()
    body = encode(github_payload(action="closed"))

    status, text = call(server.port, "POST", "/webhook", body, github_headers(body))

    assert status == 204
    assert text == ""
    assert recorder.count() == 0


@pytest.mark.network
def test_server_deduplicates_a_delivery(serve: Callable[..., Any]) -> None:
    server, recorder = serve()
    body = encode(github_payload())
    headers = github_headers(body)

    first, _ = call(server.port, "POST", "/webhook", body, headers)
    assert recorder.wait(1) is True
    second, text = call(server.port, "POST", "/webhook", body, headers)

    assert (first, second) == (202, 204)
    assert text == ""
    assert recorder.count() == 1

    # No delivery header: the event fingerprint de-duplicates instead.
    bare = {"X-GitHub-Event": "pull_request", "X-Hub-Signature-256": f"sha256={digest(SECRET, body)}"}
    third, _ = call(server.port, "POST", "/webhook", body, bare)
    fourth, _ = call(server.port, "POST", "/webhook", body, bare)
    assert (third, fourth) == (202, 204)
    assert recorder.wait(2) is True
    assert recorder.count() == 2


@pytest.mark.network
def test_server_returns_400_for_unparseable_json(serve: Callable[..., Any]) -> None:
    server, recorder = serve()
    body = b"{not json"

    status, text = call(server.port, "POST", "/webhook", body, github_headers(body))

    assert status == 400
    assert json.loads(text) == {"status": "bad_request"}
    assert recorder.count() == 0


@pytest.mark.network
def test_server_serves_gitlab_and_bitbucket(serve: Callable[..., Any]) -> None:
    gitlab_server, gitlab_recorder = serve(provider="gitlab")
    gitlab_body = encode(gitlab_payload())
    gitlab_status, _ = call(gitlab_server.port, "POST", "/hooks/gitlab", gitlab_body, gitlab_headers())
    assert gitlab_status == 202
    assert gitlab_recorder.wait(1) is True
    assert gitlab_recorder.events[0].repository == "group/sub/proj"

    bitbucket_server, bitbucket_recorder = serve(provider="bitbucket")
    bitbucket_body = encode(bitbucket_payload())
    bitbucket_status, _ = call(
        bitbucket_server.port,
        "POST",
        "/hooks/bitbucket",
        bitbucket_body,
        bitbucket_headers(bitbucket_body),
    )
    assert bitbucket_status == 202
    assert bitbucket_recorder.wait(1) is True
    assert bitbucket_recorder.events[0].number == 12

    bad_status, _ = call(bitbucket_server.port, "POST", "/hooks/bitbucket", bitbucket_body, {"X-Gitlab-Token": SECRET})
    assert bad_status == 401
    assert bitbucket_recorder.count() == 1


@pytest.mark.network
def test_server_rejects_other_methods(serve: Callable[..., Any]) -> None:
    server, _ = serve()

    status, text = call(server.port, "GET", "/webhook")

    assert status == 405
    assert json.loads(text) == {"status": "method_not_allowed"}


@pytest.mark.network
def test_server_rejects_a_missing_content_length(serve: Callable[..., Any]) -> None:
    server, recorder = serve()
    body = encode(github_payload())

    conn = http.client.HTTPConnection("127.0.0.1", server.port, timeout=10)
    try:
        conn.putrequest("POST", "/webhook", skip_accept_encoding=True)
        conn.putheader("X-Hub-Signature-256", f"sha256={digest(SECRET, body)}")
        conn.endheaders()
        response = conn.getresponse()
        status = response.status
        response.read()
    finally:
        conn.close()

    assert status == 400
    assert recorder.count() == 0


@pytest.mark.network
def test_server_stop_is_idempotent(serve: Callable[..., Any]) -> None:
    server, _ = serve()
    server.stop()
    server.stop()

    assert server.port == 0


def test_non_loopback_bind_without_a_secret_is_refused() -> None:
    with pytest.raises(ProviderError):
        webhook.WebhookServer("0.0.0.0", 9999, secret="", provider="github", handler=lambda event: None)


def test_unknown_provider_is_refused() -> None:
    with pytest.raises(ProviderError):
        webhook.WebhookServer(provider="gitea", secret=SECRET, handler=lambda event: None)


def test_handler_errors_never_kill_the_receiver(serve: Callable[..., Any]) -> None:
    recorder = Recorder()

    def explode(event: WebhookEvent) -> None:
        recorder(event)
        raise RuntimeError("handler bug")

    server = webhook.WebhookServer(secret=SECRET, provider="github", handler=explode)
    with server:
        body = encode(github_payload())
        status, _ = call(server.port, "POST", "/webhook", body, github_headers(body))
        assert status == 202
        assert recorder.wait(1) is True
        deadline = time.monotonic() + 5.0
        while server.last_error is None and time.monotonic() < deadline:
            time.sleep(0.01)
        assert isinstance(server.last_error, RuntimeError)
