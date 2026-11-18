"""Bitbucket Cloud adapter: 2.0 API, one comment per push, no thread reads.

Endpoints
---------
``GET  /repositories/{repo}/pullrequests/{id}``                  PR + head SHA
``GET  /repositories/{repo}/pullrequests/{id}/diff``             diff text
``GET  /repositories/{repo}/commit/{sha}/statuses``              build statuses
``GET  /repositories/{repo}/pullrequests/{id}`` (participants)   approvals
``POST /repositories/{repo}/pullrequests/{id}/comments``         one comment
``POST /repositories/{repo}/commit/{sha}/statuses/build``        build status

Mapping choices
---------------
* The diff endpoint answers ``text/plain``; the adapter returns it verbatim.
* One build status row becomes one :class:`CheckRunV1`, read from the first
  page of ``/statuses`` (Bitbucket pages at 100 rows, newest first).
  ``SUCCESSFUL`` → ``completed/SUCCESS``, ``FAILED`` → ``completed/FAILURE``,
  ``INPROGRESS`` → ``in_progress/PENDING``, ``STOPPED`` → ``completed/CANCELLED``.
* Bitbucket approval is a per-pull-request participant flag with **no commit
  attached**, and Bitbucket does not reset it when commits are pushed.  The
  adapter therefore records ``sha=""``; the head gate discounts such an
  approval ("no commit SHA recorded") instead of pretending it covers the head.
  Reporting a stale approval as covering head would be a false approval.
* A review is one comment whose ``content.raw`` carries the inline comments as
  ``path:line`` markdown, because Bitbucket has no review object.
"""

from __future__ import annotations

from typing import Any, Mapping

from ..models import ApprovalV1, CheckConclusion, CheckRunV1
from . import webhook
from .base import (
    HttpProvider,
    PostedReview,
    ProviderError,
    PullRequest,
    ReviewPayload,
    WebhookEvent,
)

_STATUS_STATES = {
    "SUCCESSFUL": ("completed", CheckConclusion.SUCCESS),
    "FAILED": ("completed", CheckConclusion.FAILURE),
    "INPROGRESS": ("in_progress", CheckConclusion.PENDING),
    "STOPPED": ("completed", CheckConclusion.CANCELLED),
}

#: Bitbucket build states are its own vocabulary; unknown states never report success.
_BUILD_STATES = {
    "success": "SUCCESSFUL",
    "failure": "FAILED",
    "failed": "FAILED",
    "error": "FAILED",
    "cancelled": "STOPPED",
    "canceled": "STOPPED",
    "skipped": "STOPPED",
}


def _as_dict(payload: Any, what: str) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise ProviderError(f"bitbucket: unexpected payload for {what}")
    return payload


def _as_text(payload: Any) -> str:
    """``HttpProvider.request`` returns ``{"text": ...}`` for a non-JSON body."""
    if isinstance(payload, str):
        return payload
    if isinstance(payload, dict) and isinstance(payload.get("text"), str):
        return payload["text"]
    return ""


def _rows(payload: Any) -> list[dict[str, Any]]:
    if isinstance(payload, dict):
        payload = payload.get("values") or payload.get("participants") or []
    if not isinstance(payload, list):
        return []
    return [row for row in payload if isinstance(row, dict)]


def _dict(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _str(value: Any) -> str:
    return str(value) if value is not None else ""


def _int(value: Any, fallback: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return fallback


def _review_comment(payload: ReviewPayload) -> str:
    """One raw comment body: the summary, then every inline comment as ``path:line``."""
    parts: list[str] = []
    if payload.body:
        parts.append(payload.body.strip())
    for comment in payload.comments:
        parts.append(
            f"**`{comment.path}:{int(comment.line)}`** ({comment.side or 'RIGHT'})\n\n{comment.body}"
        )
    if not parts:
        parts.append("_zero-loop-reviewer: no findings._")
    return "\n\n".join(parts)


class BitbucketProvider(HttpProvider):
    """``GitProvider`` over the Bitbucket Cloud REST API 2.0."""

    name = "bitbucket"
    status_context = "zero-loop-reviewer"

    def __init__(
        self,
        *,
        token: str,
        bot_login: str,
        base_url: str = "https://api.bitbucket.org/2.0",
        client: Any = None,
        timeout_s: float = 30.0,
        max_retries: int = 2,
        webhook_secret: str = "",
        status_context: str = "",
    ):
        super().__init__(
            base_url=base_url,
            token=token,
            bot_login=bot_login,
            client=client,
            timeout_s=timeout_s,
            max_retries=max_retries,
        )
        self._webhook_secret = webhook_secret
        if status_context:
            self.status_context = status_context

    # -- transport ---------------------------------------------------------
    def _headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.token}", "Accept": "application/json"}

    def _pr_path(self, repository: str, number: int) -> str:
        return f"/repositories/{repository}/pullrequests/{int(number)}"

    def _pr_json(self, repository: str, number: int) -> dict[str, Any]:
        return _as_dict(self.request("GET", self._pr_path(repository, number)), "pull request")

    # -- reads -------------------------------------------------------------
    def pull(self, repository: str, number: int) -> PullRequest:
        row = self._pr_json(repository, number)
        source = _dict(row.get("source"))
        destination = _dict(row.get("destination"))
        author = _dict(row.get("author"))
        return PullRequest(
            repository=repository,
            number=_int(row.get("id"), int(number)),
            title=_str(row.get("title")),
            description=_str(row.get("description")),
            author=_str(author.get("nickname") or author.get("display_name")),
            base_ref=_str(_dict(destination.get("branch")).get("name")),
            base_sha=_str(_dict(destination.get("commit")).get("hash")),
            head_ref=_str(_dict(source.get("branch")).get("name")),
            head_sha=_str(_dict(source.get("commit")).get("hash")),
            url=_str(_dict(_dict(row.get("links")).get("html")).get("href")),
            draft=bool(row.get("draft")),
        )

    def diff(self, repository: str, number: int) -> str:
        payload = self.request("GET", f"{self._pr_path(repository, number)}/diff")
        return _as_text(payload)

    def check_runs(self, repository: str, sha: str) -> list[CheckRunV1]:
        payload = self.request("GET", f"/repositories/{repository}/commit/{sha}/statuses")
        runs: list[CheckRunV1] = []
        for row in _rows(payload):
            state = _str(row.get("state")).strip().upper()
            status, conclusion = _STATUS_STATES.get(state, ("completed", CheckConclusion.UNKNOWN))
            runs.append(
                CheckRunV1(
                    name=_str(row.get("name")) or _str(row.get("key")),
                    sha=str(sha),
                    status=status,
                    conclusion=conclusion,
                    app="bitbucket",
                )
            )
        return runs

    def approvals(self, repository: str, number: int) -> list[ApprovalV1]:
        """Per-PR participant flags; ``sha`` stays empty (see the module docstring)."""
        row = self._pr_json(repository, number)
        out: list[ApprovalV1] = []
        for participant in _rows(row.get("participants")):
            if not participant.get("approved"):
                continue
            user = _dict(participant.get("user"))
            out.append(
                ApprovalV1(
                    review_id="",
                    reviewer=_str(user.get("nickname") or user.get("display_name")),
                    sha="",
                    state="approved",
                    dismissed=False,
                    stale=False,
                )
            )
        return out

    # -- writes ------------------------------------------------------------
    def post_review(self, repository: str, number: int, payload: ReviewPayload) -> PostedReview:
        row = _as_dict(
            self.request(
                "POST",
                f"{self._pr_path(repository, number)}/comments",
                json_body={"content": {"raw": _review_comment(payload)}},
            ),
            "posted comment",
        )
        return PostedReview(
            id=_str(row.get("id")),
            url=_str(_dict(_dict(row.get("links")).get("html")).get("href")),
            raw=row,
        )

    def post_status(
        self,
        repository: str,
        sha: str,
        state: str,
        description: str,
        target_url: str = "",
    ) -> dict[str, Any]:
        body: dict[str, Any] = {
            "state": _BUILD_STATES.get(str(state).strip().lower(), "INPROGRESS"),
            "key": self.status_context,
            "name": self.status_context,
            "description": description[:140],
        }
        if target_url:
            body["url"] = target_url
        return _as_dict(
            self.request(
                "POST",
                f"/repositories/{repository}/commit/{sha}/statuses/build",
                json_body=body,
            ),
            "build status",
        )

    # -- webhooks ----------------------------------------------------------
    def verify_webhook(self, body: bytes, headers: Mapping[str, str]) -> bool:
        return webhook.verify(self.name, self._webhook_secret, body, headers)

    def parse_webhook(self, body: bytes, headers: Mapping[str, str]) -> WebhookEvent:
        """Parse only.  Callers must call :meth:`verify_webhook` first."""
        return webhook.parse(self.name, body, headers)
