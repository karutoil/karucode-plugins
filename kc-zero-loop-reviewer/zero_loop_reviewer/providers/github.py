"""GitHub REST adapter for the four operations the reviewer needs.

Touched: pull-request metadata, the diff text, check runs on an exact SHA,
review metadata for the head gate, one posted review, one commit status.
Review *bodies* are never read back: ``approvals`` only maps metadata, so a
comment thread can never leak into the review.

Endpoints (REST v3, API version 2022-11-28)
-------------------------------------------
``GET  /repos/{repo}/pulls/{n}``                            PR + head SHA
``GET  /repos/{repo}/pulls/{n}`` (Accept: vnd.github.v3.diff) diff text
``GET  /repos/{repo}/commits/{sha}/check-runs?filter=latest`` check runs
``GET  /repos/{repo}/pulls/{n}/reviews``                     review metadata
``POST /repos/{repo}/pulls/{n}/reviews``                     one review
``POST /repos/{repo}/commits/{sha}/statuses``                commit status
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

API_VERSION = "2022-11-28"
DIFF_ACCEPT = "application/vnd.github.v3.diff"

_REVIEW_STATES = {
    "APPROVED": "approved",
    "CHANGES_REQUESTED": "changes_requested",
    "COMMENTED": "commented",
    "DISMISSED": "dismissed",
    "PENDING": "commented",
}

#: anything unrecognised becomes ``pending``: a status must never claim success
#: unless it was asked for.
_STATUS_STATES = {
    "success": "success",
    "failure": "failure",
    "failed": "failure",
    "error": "error",
    "cancelled": "error",
    "timed_out": "failure",
    "pending": "pending",
    "running": "pending",
}

_CONCLUSIONS = frozenset(conclusion.value for conclusion in CheckConclusion)


def _as_dict(payload: Any, what: str) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise ProviderError(f"github: unexpected payload for {what}")
    return payload


def _as_rows(payload: Any, what: str) -> list[dict[str, Any]]:
    if not isinstance(payload, list):
        raise ProviderError(f"github: unexpected payload for {what}")
    return [row for row in payload if isinstance(row, dict)]


def _as_text(payload: Any) -> str:
    """``HttpProvider.request`` returns ``{"text": ...}`` for a non-JSON body."""
    if isinstance(payload, str):
        return payload
    if isinstance(payload, dict) and isinstance(payload.get("text"), str):
        return payload["text"]
    return ""


def _int(value: Any, fallback: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return fallback


def _login(row: Any) -> str:
    if not isinstance(row, dict):
        return ""
    return str(row.get("login") or "")


def _check_conclusion(value: Any, status: str) -> CheckConclusion:
    raw = str(value or "").strip().lower()
    if raw in _CONCLUSIONS:
        return CheckConclusion(raw)
    if not raw:
        return CheckConclusion.PENDING if status != "completed" else CheckConclusion.UNKNOWN
    return CheckConclusion.UNKNOWN


class GitHubProvider(HttpProvider):
    """``GitProvider`` over the GitHub REST API."""

    name = "github"
    status_context = "zero-loop-reviewer"

    def __init__(
        self,
        *,
        token: str,
        bot_login: str,
        base_url: str = "https://api.github.com",
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
        return {
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": API_VERSION,
        }

    # -- reads -------------------------------------------------------------
    def pull(self, repository: str, number: int) -> PullRequest:
        row = _as_dict(
            self.request("GET", f"/repos/{repository}/pulls/{int(number)}"),
            "pull request",
        )
        base = row.get("base") if isinstance(row.get("base"), dict) else {}
        head = row.get("head") if isinstance(row.get("head"), dict) else {}
        return PullRequest(
            repository=repository,
            number=_int(row.get("number"), int(number)),
            title=str(row.get("title") or ""),
            description=str(row.get("body") or ""),
            author=_login(row.get("user")),
            base_ref=str(base.get("ref") or ""),
            base_sha=str(base.get("sha") or ""),
            head_ref=str(head.get("ref") or ""),
            head_sha=str(head.get("sha") or ""),
            url=str(row.get("html_url") or ""),
            draft=bool(row.get("draft")),
        )

    def diff(self, repository: str, number: int) -> str:
        payload = self.request(
            "GET",
            f"/repos/{repository}/pulls/{int(number)}",
            headers={"Accept": DIFF_ACCEPT},
        )
        return _as_text(payload)

    def check_runs(self, repository: str, sha: str) -> list[CheckRunV1]:
        payload = _as_dict(
            self.request("GET", f"/repos/{repository}/commits/{sha}/check-runs?filter=latest"),
            "check runs",
        )
        runs: list[CheckRunV1] = []
        for row in payload.get("check_runs") or []:
            if not isinstance(row, dict):
                continue
            status = str(row.get("status") or "completed")
            app = row.get("app") if isinstance(row.get("app"), dict) else {}
            runs.append(
                CheckRunV1(
                    name=str(row.get("name") or ""),
                    sha=str(row.get("head_sha") or sha),
                    status=status,
                    conclusion=_check_conclusion(row.get("conclusion"), status),
                    app=str(app.get("slug") or app.get("name") or ""),
                )
            )
        return runs

    def approvals(self, repository: str, number: int) -> list[ApprovalV1]:
        rows = _as_rows(
            self.request("GET", f"/repos/{repository}/pulls/{int(number)}/reviews"),
            "reviews",
        )
        out: list[ApprovalV1] = []
        for row in rows:
            state = str(row.get("state") or "").upper()
            out.append(
                ApprovalV1(
                    review_id=str(row.get("id") or ""),
                    reviewer=_login(row.get("user")),
                    sha=str(row.get("commit_id") or ""),
                    state=_REVIEW_STATES.get(state, "commented"),  # type: ignore[arg-type]
                    dismissed=state == "DISMISSED",
                    stale=False,
                )
            )
        return out

    # -- writes ------------------------------------------------------------
    def post_review(self, repository: str, number: int, payload: ReviewPayload) -> PostedReview:
        body = {
            "body": payload.body,
            "event": payload.event or "COMMENT",
            "comments": [
                {
                    "path": comment.path,
                    "line": int(comment.line),
                    "side": comment.side or "RIGHT",
                    "body": comment.body,
                    **(
                        {
                            "start_line": int(comment.start_line),
                            "start_side": comment.start_side or "RIGHT",
                        }
                        if comment.start_line is not None
                        else {}
                    ),
                }
                for comment in payload.comments
            ],
        }
        row = _as_dict(
            self.request("POST", f"/repos/{repository}/pulls/{int(number)}/reviews", json_body=body),
            "posted review",
        )
        return PostedReview(id=str(row.get("id") or ""), url=str(row.get("html_url") or ""), raw=row)

    def post_status(
        self,
        repository: str,
        sha: str,
        state: str,
        description: str,
        target_url: str = "",
    ) -> dict[str, Any]:
        body: dict[str, Any] = {
            "state": _STATUS_STATES.get(str(state).strip().lower(), "pending"),
            "context": self.status_context,
            "description": description[:140],
        }
        if target_url:
            body["target_url"] = target_url
        return _as_dict(
            self.request("POST", f"/repos/{repository}/commits/{sha}/statuses", json_body=body),
            "commit status",
        )

    # -- webhooks ----------------------------------------------------------
    def verify_webhook(self, body: bytes, headers: Mapping[str, str]) -> bool:
        return webhook.verify(self.name, self._webhook_secret, body, headers)

    def parse_webhook(self, body: bytes, headers: Mapping[str, str]) -> WebhookEvent:
        """Parse only.  Callers must call :meth:`verify_webhook` first."""
        return webhook.parse(self.name, body, headers)
