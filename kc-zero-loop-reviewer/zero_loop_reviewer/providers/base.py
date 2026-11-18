"""Git-provider interface shared by GitHub, GitLab and Bitbucket.

The reviewer posts exactly **one** review per push.  Nothing here reads comment
threads: ``approvals`` is metadata for the head-commit gate, and the PR
description is returned separately so only the PR-MCI check can see it.
"""

from __future__ import annotations

import hashlib
import hmac
from dataclasses import dataclass, field
from typing import Any, Mapping, Protocol

import httpx

from ..config import Settings
from ..errors import FailureKind, ReviewError
from ..models import ApprovalV1, CheckRunV1


class ProviderError(ReviewError):
    def __init__(self, message: str, *, detail: str = "", status_code: int | None = None):
        super().__init__(message, detail=detail)
        self.kind = FailureKind.INVALID_INPUT
        self.status_code = status_code


@dataclass(frozen=True)
class PullRequest:
    repository: str
    number: int
    title: str = ""
    description: str = ""
    author: str = ""
    base_ref: str = ""
    base_sha: str = ""
    head_ref: str = ""
    head_sha: str = ""
    url: str = ""
    draft: bool = False

    @property
    def description_sha256(self) -> str:
        return hashlib.sha256(self.description.encode("utf-8")).hexdigest()


@dataclass(frozen=True)
class ReviewComment:
    path: str
    line: int
    body: str
    side: str = "RIGHT"
    start_line: int | None = None
    start_side: str | None = None


@dataclass(frozen=True)
class ReviewPayload:
    body: str
    comments: list[ReviewComment] = field(default_factory=list)
    event: str = "COMMENT"  # APPROVE | REQUEST_CHANGES | COMMENT


@dataclass(frozen=True)
class PostedReview:
    id: str = ""
    url: str = ""
    raw: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class WebhookEvent:
    kind: str  # review | ignore
    repository: str = ""
    number: int = 0
    head_sha: str = ""
    base_ref: str = ""
    action: str = ""
    delivery_id: str = ""
    reason: str = ""
    draft: bool = False


class GitProvider(Protocol):
    name: str

    def pull(self, repository: str, number: int) -> PullRequest: ...

    def diff(self, repository: str, number: int) -> str: ...

    def check_runs(self, repository: str, sha: str) -> list[CheckRunV1]: ...

    def approvals(self, repository: str, number: int) -> list[ApprovalV1]: ...

    def bot_login(self) -> str: ...

    def post_review(self, repository: str, number: int, payload: ReviewPayload) -> PostedReview: ...

    def post_status(self, repository: str, sha: str, state: str, description: str, target_url: str = "") -> dict[str, Any]: ...

    def verify_webhook(self, body: bytes, headers: Mapping[str, str]) -> bool: ...

    def parse_webhook(self, body: bytes, headers: Mapping[str, str]) -> WebhookEvent: ...


class HttpProvider:
    """Shared HTTP behaviour: one client, bounded retries, no redirects."""

    name = "http"

    def __init__(
        self,
        *,
        base_url: str,
        token: str,
        bot_login: str,
        client: httpx.Client | None = None,
        timeout_s: float = 30.0,
        max_retries: int = 2,
        user_agent: str = "zero-loop-reviewer/1.0",
    ):
        self.base_url = base_url.rstrip("/")
        self.token = token
        self._bot_login = bot_login
        self._client = client or httpx.Client(
            timeout=timeout_s, follow_redirects=False, headers={"User-Agent": user_agent}
        )
        self.max_retries = max_retries

    def bot_login(self) -> str:
        return self._bot_login

    def _headers(self) -> dict[str, str]:
        raise NotImplementedError

    def request(
        self,
        method: str,
        path: str,
        *,
        json_body: Any = None,
        headers: Mapping[str, str] | None = None,
        retry: bool | None = None,
    ) -> Any:
        merged = {**self._headers(), **dict(headers or {})}
        url = path if path.startswith("http") else f"{self.base_url}{path}"
        # A review POST is not idempotent: retrying a 502 could post the same
        # review twice.  Only idempotent methods are retried by default.
        retryable = (method.upper() in ("GET", "HEAD")) if retry is None else retry
        last: Exception | None = None
        for attempt in range(self.max_retries + 1):
            try:
                response = self._client.request(method, url, headers=merged, json=json_body)
            except httpx.HTTPError as exc:
                last = exc
                if not retryable or attempt >= self.max_retries:
                    raise ProviderError(f"{self.name}: transport error", detail=str(exc)) from None
                continue
            if response.status_code in (429, 500, 502, 503, 504) and attempt < self.max_retries and retryable:
                continue
            if response.status_code >= 400:
                raise ProviderError(
                    f"{self.name}: HTTP {response.status_code} for {method} {path}",
                    detail=response.text[:2000],
                    status_code=response.status_code,
                )
            if not response.content:
                return {}
            try:
                return response.json()
            except ValueError:
                return {"text": response.text}
        raise ProviderError(f"{self.name}: request failed", detail=str(last))

    def close(self) -> None:
        self._client.close()


def hmac_sha256(secret: str, body: bytes) -> str:
    return hmac.new(secret.encode("utf-8"), body, hashlib.sha256).hexdigest()


def secure_equals(left: str, right: str) -> bool:
    return hmac.compare_digest(left.encode("utf-8"), right.encode("utf-8"))


def token_from_env(settings: Settings, environ: Mapping[str, str]) -> str:
    name = settings.ci.token_env
    value = environ.get(name, "").strip()
    if not value:
        raise ProviderError(f"no token: set ${name} or configure ci.token_env")
    return value


def build_provider(
    settings: Settings,
    environ: Mapping[str, str],
    *,
    client: httpx.Client | None = None,
) -> GitProvider:
    provider = settings.ci.provider
    token = token_from_env(settings, environ)
    if provider == "github":
        from .github import GitHubProvider

        return GitHubProvider(
            token=token,
            bot_login=settings.gate.bot_login,
            base_url=settings.ci.api_base or "https://api.github.com",
            client=client,
        )
    if provider == "gitlab":
        from .gitlab import GitLabProvider

        return GitLabProvider(
            token=token,
            bot_login=settings.gate.bot_login,
            base_url=settings.ci.api_base or "https://gitlab.com/api/v4",
            client=client,
        )
    if provider == "bitbucket":
        from .bitbucket import BitbucketProvider

        return BitbucketProvider(
            token=token,
            bot_login=settings.gate.bot_login,
            base_url=settings.ci.api_base or "https://api.bitbucket.org/2.0",
            client=client,
        )
    raise ProviderError(f"unknown ci.provider {provider!r}")
