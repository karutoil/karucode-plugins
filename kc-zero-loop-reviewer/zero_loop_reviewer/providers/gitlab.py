"""GitLab adapter: merge-request API v4, one note per push, no thread reads.

Endpoints
---------
``GET  /projects/{repo}/merge_requests/{iid}``              MR + head SHA
``GET  /projects/{repo}/merge_requests/{iid}/changes``      diff text
``GET  /projects/{repo}/pipelines?sha={sha}``               one row per pipeline
``GET  /projects/{repo}/merge_requests/{iid}/approvals``    approval metadata
``POST /projects/{repo}/merge_requests/{iid}/notes``        one review note
``POST /projects/{repo}/statuses/{sha}``                    commit status

Mapping choices
---------------
* ``{repo}`` is the project path *or* numeric id, URL-encoded (``group%2Fsub``).
* A pipeline becomes one :class:`CheckRunV1`, read from the newest page of
  ``/pipelines?sha=``.  GitLab pipelines have no name of their own, so it is
  ``pipeline`` (that is the name ``gate.required_checks`` must list);
  ``success`` → ``completed/SUCCESS``, ``failed`` → ``completed/FAILURE``,
  ``running`` → ``in_progress/PENDING``.
* GitLab approvals are per *merge request*, not per commit: every approved user
  is recorded against the head SHA reported by the merge request, and the head
  commit gate decides whether that still applies.  GitLab itself drops
  approvals when commits are pushed (unless "keep approvals" is enabled).
* A review is one note whose body carries the inline comments as
  ``path:line`` markdown, because GitLab notes have no line-anchored payload.
"""

from __future__ import annotations

from typing import Any, Mapping
from urllib.parse import quote

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

_PIPELINE_STATES = {
    "success": ("completed", CheckConclusion.SUCCESS),
    "failed": ("completed", CheckConclusion.FAILURE),
    "canceled": ("completed", CheckConclusion.CANCELLED),
    "cancelled": ("completed", CheckConclusion.CANCELLED),
    "skipped": ("completed", CheckConclusion.SKIPPED),
    "running": ("in_progress", CheckConclusion.PENDING),
    "pending": ("queued", CheckConclusion.PENDING),
    "created": ("queued", CheckConclusion.PENDING),
    "manual": ("queued", CheckConclusion.PENDING),
    "scheduled": ("queued", CheckConclusion.PENDING),
}

#: GitLab commit-status states are its own vocabulary; never claim more than asked.
_STATUS_STATES = {
    "success": "success",
    "failure": "failed",
    "failed": "failed",
    "error": "failed",
    "pending": "pending",
    "running": "running",
    "cancelled": "canceled",
    "canceled": "canceled",
    "skipped": "skipped",
}


def _project(repository: str) -> str:
    return quote(str(repository), safe="")


def _as_dict(payload: Any, what: str) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise ProviderError(f"gitlab: unexpected payload for {what}")
    return payload


def _rows(payload: Any) -> list[dict[str, Any]]:
    if isinstance(payload, dict):
        payload = payload.get("changes") or payload.get("approved_by") or []
    if not isinstance(payload, list):
        return []
    return [row for row in payload if isinstance(row, dict)]


def _int(value: Any, fallback: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return fallback


def _str(value: Any) -> str:
    return str(value) if value is not None else ""


def _review_note(payload: ReviewPayload) -> str:
    """One markdown note: the summary, then every inline comment as ``path:line``."""
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


class GitLabProvider(HttpProvider):
    """``GitProvider`` over the GitLab REST API v4."""

    name = "gitlab"
    status_context = "zero-loop-reviewer"

    def __init__(
        self,
        *,
        token: str,
        bot_login: str,
        base_url: str = "https://gitlab.com/api/v4",
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

    def _mr_path(self, repository: str, number: int) -> str:
        return f"/projects/{_project(repository)}/merge_requests/{int(number)}"

    # -- reads -------------------------------------------------------------
    def pull(self, repository: str, number: int) -> PullRequest:
        row = _as_dict(self.request("GET", self._mr_path(repository, number)), "merge request")
        refs = row.get("diff_refs") if isinstance(row.get("diff_refs"), dict) else {}
        author = row.get("author") if isinstance(row.get("author"), dict) else {}
        return PullRequest(
            repository=repository,
            number=_int(row.get("iid"), int(number)),
            title=_str(row.get("title")),
            description=_str(row.get("description")),
            author=_str(author.get("username")),
            base_ref=_str(row.get("target_branch")),
            base_sha=_str(refs.get("base_sha")),
            head_ref=_str(row.get("source_branch")),
            head_sha=_str(refs.get("head_sha") or row.get("sha")),
            url=_str(row.get("web_url")),
            draft=bool(row.get("draft") or row.get("work_in_progress")),
        )

    def diff(self, repository: str, number: int) -> str:
        """Rebuild one unified diff from the per-file ``changes`` payload."""
        row = _as_dict(
            self.request("GET", f"{self._mr_path(repository, number)}/changes"),
            "merge request changes",
        )
        chunks: list[str] = []
        for change in _rows(row.get("changes")):
            old = _str(change.get("old_path")) or _str(change.get("new_path"))
            new = _str(change.get("new_path")) or old
            body = _str(change.get("diff"))
            if not body:
                continue
            if not body.endswith("\n"):
                body += "\n"
            chunks.append(f"diff --git a/{old} b/{new}\n--- a/{old}\n+++ b/{new}\n{body}")
        return "".join(chunks)

    def check_runs(self, repository: str, sha: str) -> list[CheckRunV1]:
        payload = self.request(
            "GET",
            f"/projects/{_project(repository)}/pipelines?sha={quote(str(sha), safe='')}",
        )
        runs: list[CheckRunV1] = []
        for row in _rows(payload):
            state = _str(row.get("status")).strip().lower()
            status, conclusion = _PIPELINE_STATES.get(state, ("queued", CheckConclusion.UNKNOWN))
            runs.append(
                CheckRunV1(
                    name=_str(row.get("name")) or "pipeline",
                    sha=_str(row.get("sha")) or str(sha),
                    status=status,
                    conclusion=conclusion,
                    app="gitlab",
                )
            )
        return runs

    def approvals(self, repository: str, number: int) -> list[ApprovalV1]:
        """Approvals are per MR, so the head SHA is attached to each of them."""
        head_sha = self.pull(repository, number).head_sha
        row = _as_dict(
            self.request("GET", f"{self._mr_path(repository, number)}/approvals"),
            "approvals",
        )
        out: list[ApprovalV1] = []
        for entry in _rows(row.get("approved_by")):
            user = entry.get("user") if isinstance(entry.get("user"), dict) else entry
            out.append(
                ApprovalV1(
                    review_id="",
                    reviewer=_str(user.get("username") or user.get("name")),
                    sha=head_sha,
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
                f"{self._mr_path(repository, number)}/notes",
                json_body={"body": _review_note(payload)},
            ),
            "posted note",
        )
        return PostedReview(id=_str(row.get("id")), url=_str(row.get("web_url")), raw=row)

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
            "name": self.status_context,
            "description": description[:140],
        }
        if target_url:
            body["target_url"] = target_url
        return _as_dict(
            self.request(
                "POST",
                f"/projects/{_project(repository)}/statuses/{quote(str(sha), safe='')}",
                json_body=body,
            ),
            "commit status",
        )

    # -- webhooks ----------------------------------------------------------
    def verify_webhook(self, body: bytes, headers: Mapping[str, str]) -> bool:
        return webhook.verify(self.name, self._webhook_secret, body, headers)

    def parse_webhook(self, body: bytes, headers: Mapping[str, str]) -> WebhookEvent:
        """Parse only.  Callers must call :meth:`verify_webhook` first."""
        return webhook.parse(self.name, body, headers)
