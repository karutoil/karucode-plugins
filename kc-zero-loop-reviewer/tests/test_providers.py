"""Provider adapters: method, path, headers and payload against a fake transport."""

from __future__ import annotations

import hashlib
import hmac
import json
from typing import Any, Callable

import httpx
import pytest

from zero_loop_reviewer.models import ApprovalV1, CheckConclusion, CheckRunV1
from zero_loop_reviewer.providers.base import (
    PostedReview,
    ProviderError,
    PullRequest,
    ReviewComment,
    ReviewPayload,
)
from zero_loop_reviewer.providers.bitbucket import BitbucketProvider
from zero_loop_reviewer.providers.github import GitHubProvider
from zero_loop_reviewer.providers.gitlab import GitLabProvider

HEAD = "a" * 40
BASE = "b" * 40
BOT = "zero-loop-reviewer[bot]"

Handler = Callable[[httpx.Request], httpx.Response]


class Calls(list):
    """Recorded requests, with helpers so tests read like assertions."""

    def client(self, handler: Handler) -> httpx.Client:
        def recording(request: httpx.Request) -> httpx.Response:
            self.append(request)
            return handler(request)

        return httpx.Client(transport=httpx.MockTransport(recording))

    @property
    def only(self) -> httpx.Request:
        assert len(self) == 1, f"expected one request, saw {len(self)}"
        return self[0]

    def body(self, index: int = 0) -> Any:
        return json.loads(self[index].content)


def github(calls: Calls, handler: Handler, **kwargs: Any) -> GitHubProvider:
    return GitHubProvider(
        token="t0ken",
        bot_login=BOT,
        base_url="https://api.github.test",
        client=calls.client(handler),
        max_retries=0,
        **kwargs,
    )


def gitlab(calls: Calls, handler: Handler, **kwargs: Any) -> GitLabProvider:
    return GitLabProvider(
        token="glpat-t0ken",
        bot_login=BOT,
        base_url="https://gitlab.test/api/v4",
        client=calls.client(handler),
        max_retries=0,
        **kwargs,
    )


def bitbucket(calls: Calls, handler: Handler, **kwargs: Any) -> BitbucketProvider:
    return BitbucketProvider(
        token="bb-t0ken",
        bot_login=BOT,
        base_url="https://bitbucket.test/2.0",
        client=calls.client(handler),
        max_retries=0,
        **kwargs,
    )


def json_reply(payload: Any, status: int = 200) -> Handler:
    return lambda request: httpx.Response(status, json=payload)


REVIEW = ReviewPayload(
    body="Verdict: request changes.",
    event="REQUEST_CHANGES",
    comments=[
        ReviewComment(path="src/a.py", line=12, body="Off-by-one.", side="RIGHT"),
        ReviewComment(path="src/b.py", line=3, body="Leak.", side="LEFT"),
    ],
)


# --------------------------------------------------------------------------- #
# GitHub
# --------------------------------------------------------------------------- #


def test_github_pull_maps_metadata() -> None:
    calls = Calls()
    provider = github(
        calls,
        json_reply(
            {
                "number": 7,
                "title": "Add cache",
                "body": "PR-MCI: one extra cache lookup on the cold path.",
                "user": {"login": "toil"},
                "base": {"ref": "main", "sha": BASE},
                "head": {"ref": "feat/cache", "sha": HEAD},
                "html_url": "https://github.test/o/r/pull/7",
                "draft": False,
            }
        ),
    )

    pr = provider.pull("o/r", 7)

    assert pr == PullRequest(
        repository="o/r",
        number=7,
        title="Add cache",
        description="PR-MCI: one extra cache lookup on the cold path.",
        author="toil",
        base_ref="main",
        base_sha=BASE,
        head_ref="feat/cache",
        head_sha=HEAD,
        url="https://github.test/o/r/pull/7",
        draft=False,
    )
    request = calls.only
    assert request.method == "GET"
    assert request.url.raw_path.decode() == "/repos/o/r/pulls/7"
    assert request.headers["Authorization"] == "Bearer t0ken"
    assert request.headers["Accept"] == "application/vnd.github+json"
    assert request.headers["X-GitHub-Api-Version"] == "2022-11-28"
    assert request.content == b""


def test_github_diff_asks_for_the_diff_media_type() -> None:
    body = "diff --git a/a.py b/a.py\n--- a/a.py\n+++ b/a.py\n@@ -1 +1 @@\n-x = 1\n+x = 2\n"
    calls = Calls()
    provider = github(calls, lambda request: httpx.Response(200, text=body))

    assert provider.diff("o/r", 3) == body
    request = calls.only
    assert request.method == "GET"
    assert request.url.raw_path.decode() == "/repos/o/r/pulls/3"
    assert request.headers["Accept"] == "application/vnd.github.v3.diff"


def test_github_check_runs_map_status_and_conclusion() -> None:
    calls = Calls()
    provider = github(
        calls,
        json_reply(
            {
                "total_count": 3,
                "check_runs": [
                    {
                        "name": "ci/tests",
                        "head_sha": HEAD,
                        "status": "completed",
                        "conclusion": "success",
                        "app": {"slug": "github-actions"},
                    },
                    {
                        "name": "ci/lint",
                        "head_sha": HEAD,
                        "status": "in_progress",
                        "conclusion": None,
                        "app": {"name": "lint-app"},
                    },
                    {
                        "name": "ci/broken",
                        "head_sha": HEAD,
                        "status": "completed",
                        "conclusion": "startup_failure",
                        "app": None,
                    },
                ],
            }
        ),
    )

    runs = provider.check_runs("o/r", HEAD)

    assert [run.name for run in runs] == ["ci/tests", "ci/lint", "ci/broken"]
    assert runs[0] == CheckRunV1(
        name="ci/tests",
        sha=HEAD,
        status="completed",
        conclusion=CheckConclusion.SUCCESS,
        app="github-actions",
    )
    assert runs[1].status == "in_progress"
    assert runs[1].conclusion is CheckConclusion.PENDING
    assert runs[1].app == "lint-app"
    assert runs[2].conclusion is CheckConclusion.UNKNOWN
    assert runs[2].app == ""
    assert calls.only.url.raw_path.decode() == f"/repos/o/r/commits/{HEAD}/check-runs?filter=latest"


def test_github_approvals_map_review_metadata() -> None:
    calls = Calls()
    provider = github(
        calls,
        json_reply(
            [
                {"id": 11, "user": {"login": "alice"}, "commit_id": HEAD, "state": "APPROVED"},
                {"id": 12, "user": {"login": "bob"}, "commit_id": "c" * 40, "state": "CHANGES_REQUESTED"},
                {"id": 13, "user": {"login": "carol"}, "commit_id": HEAD, "state": "DISMISSED"},
                {"id": 14, "user": {"login": "dave"}, "commit_id": HEAD, "state": "COMMENTED"},
            ]
        ),
    )

    approvals = provider.approvals("o/r", 5)

    assert approvals[0] == ApprovalV1(
        review_id="11",
        reviewer="alice",
        sha=HEAD,
        state="approved",
        dismissed=False,
        stale=False,
    )
    assert [row.state for row in approvals] == [
        "approved",
        "changes_requested",
        "dismissed",
        "commented",
    ]
    assert approvals[2].dismissed is True
    assert approvals[1].sha == "c" * 40
    assert calls.only.url.raw_path.decode() == "/repos/o/r/pulls/5/reviews"


def test_github_post_review_sends_one_review_payload() -> None:
    calls = Calls()
    provider = github(
        calls,
        json_reply({"id": 99, "html_url": "https://github.test/o/r/pull/5#pullrequestreview-99"}),
    )

    posted = provider.post_review("o/r", 5, REVIEW)

    request = calls.only
    assert request.method == "POST"
    assert request.url.raw_path.decode() == "/repos/o/r/pulls/5/reviews"
    assert calls.body() == {
        "body": "Verdict: request changes.",
        "event": "REQUEST_CHANGES",
        "comments": [
            {"path": "src/a.py", "line": 12, "side": "RIGHT", "body": "Off-by-one."},
            {"path": "src/b.py", "line": 3, "side": "LEFT", "body": "Leak."},
        ],
    }
    assert posted == PostedReview(
        id="99",
        url="https://github.test/o/r/pull/5#pullrequestreview-99",
        raw={"id": 99, "html_url": "https://github.test/o/r/pull/5#pullrequestreview-99"},
    )


def test_github_post_status_sends_context_and_target_url() -> None:
    calls = Calls()
    row = {"id": 1, "state": "failure", "context": "zero-loop-reviewer"}
    provider = github(calls, json_reply(row))

    assert provider.post_status("o/r", HEAD, "failure", "2 findings", "https://ci.test/run/1") == row
    assert calls.only.url.raw_path.decode() == f"/repos/o/r/commits/{HEAD}/statuses"
    assert calls.body() == {
        "state": "failure",
        "context": "zero-loop-reviewer",
        "description": "2 findings",
        "target_url": "https://ci.test/run/1",
    }

    assert provider.post_status("o/r", HEAD, "whatever", "pending run")["state"] == "failure"
    assert calls.body(1) == {
        "state": "pending",
        "context": "zero-loop-reviewer",
        "description": "pending run",
    }


def test_http_error_raises_provider_error_with_status() -> None:
    calls = Calls()
    provider = github(calls, json_reply({"message": "Not Found"}, status=404))

    with pytest.raises(ProviderError) as info:
        provider.pull("o/r", 1)

    assert info.value.status_code == 404
    assert "404" in str(info.value)
    assert len(calls) == 1


def test_server_error_is_retried_then_raised_with_status() -> None:
    calls = Calls()
    provider = GitHubProvider(
        token="t0ken",
        bot_login=BOT,
        base_url="https://api.github.test",
        client=calls.client(lambda request: httpx.Response(502, text="bad gateway")),
        max_retries=2,
    )

    with pytest.raises(ProviderError) as info:
        provider.diff("o/r", 1)

    assert info.value.status_code == 502
    assert len(calls) == 3


def test_transport_error_raises_provider_error() -> None:
    calls = Calls()

    def boom(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("unreachable", request=request)

    provider = github(calls, boom)

    with pytest.raises(ProviderError):
        provider.check_runs("o/r", HEAD)
    assert len(calls) == 1


def test_unexpected_payload_raises_provider_error() -> None:
    calls = Calls()
    provider = github(calls, json_reply(["not", "a", "pull request"]))

    with pytest.raises(ProviderError):
        provider.pull("o/r", 1)


def test_bot_login_comes_from_configuration() -> None:
    calls = Calls()
    assert github(calls, json_reply({})).bot_login() == BOT
    assert gitlab(calls, json_reply({})).bot_login() == BOT
    assert bitbucket(calls, json_reply({})).bot_login() == BOT


def test_github_provider_verifies_its_webhook() -> None:
    calls = Calls()
    secret = "s3cret"
    body = b'{"action": "opened"}'
    digest = hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    headers = {"X-Hub-Signature-256": f"sha256={digest}"}
    provider = github(calls, json_reply({}), webhook_secret=secret)

    assert provider.verify_webhook(body, headers) is True
    assert provider.verify_webhook(body, {"X-Hub-Signature-256": "sha256=00"}) is False
    event = provider.parse_webhook(
        body,
        {"X-GitHub-Event": "pull_request", "X-GitHub-Delivery": "abc"},
    )
    assert event.kind == "ignore"
    assert event.delivery_id == "abc"


# --------------------------------------------------------------------------- #
# GitLab
# --------------------------------------------------------------------------- #

MR_PATH = "/api/v4/projects/group%2Fsub%2Fproj/merge_requests/3"


def test_gitlab_pull_url_encodes_the_project_path() -> None:
    calls = Calls()
    provider = gitlab(
        calls,
        json_reply(
            {
                "iid": 3,
                "title": "Add cache",
                "description": "PR-MCI: description",
                "author": {"username": "toil"},
                "target_branch": "main",
                "source_branch": "feat/cache",
                "sha": HEAD,
                "web_url": "https://gitlab.test/group/sub/proj/-/merge_requests/3",
                "draft": True,
                "diff_refs": {"base_sha": BASE, "head_sha": HEAD, "start_sha": BASE},
            }
        ),
    )

    pr = provider.pull("group/sub/proj", 3)

    assert pr == PullRequest(
        repository="group/sub/proj",
        number=3,
        title="Add cache",
        description="PR-MCI: description",
        author="toil",
        base_ref="main",
        base_sha=BASE,
        head_ref="feat/cache",
        head_sha=HEAD,
        url="https://gitlab.test/group/sub/proj/-/merge_requests/3",
        draft=True,
    )
    assert calls.only.url.raw_path.decode() == MR_PATH
    assert calls.only.headers["Authorization"] == "Bearer glpat-t0ken"


def test_gitlab_diff_rebuilds_unified_text_from_changes() -> None:
    calls = Calls()
    provider = gitlab(
        calls,
        json_reply(
            {
                "changes": [
                    {
                        "old_path": "src/a.py",
                        "new_path": "src/a.py",
                        "diff": "@@ -1,2 +1,2 @@\n x = 1\n-y = 2\n+y = 3\n",
                    },
                    {"old_path": "src/old.py", "new_path": "src/new.py", "diff": "@@ -1 +1 @@\n-a\n+b"},
                    {"old_path": "src/empty.py", "new_path": "src/empty.py", "diff": ""},
                ]
            }
        ),
    )

    diff = provider.diff("group/sub/proj", 3)

    assert calls.only.url.raw_path.decode() == f"{MR_PATH}/changes"
    assert diff.startswith("diff --git a/src/a.py b/src/a.py\n--- a/src/a.py\n+++ b/src/a.py\n")
    assert "+y = 3\n" in diff
    assert "diff --git a/src/old.py b/src/new.py\n--- a/src/old.py\n+++ b/src/new.py\n" in diff
    assert diff.rstrip("\n").endswith("-a\n+b")
    assert "src/empty.py" not in diff


def test_gitlab_check_runs_map_pipeline_states() -> None:
    calls = Calls()
    provider = gitlab(
        calls,
        json_reply(
            [
                {"id": 1, "sha": HEAD, "status": "success", "source": "push"},
                {"id": 2, "sha": HEAD, "status": "failed", "source": "push"},
                {"id": 3, "sha": HEAD, "status": "running", "source": "push"},
                {"id": 4, "sha": HEAD, "status": "manual", "source": "web"},
            ]
        ),
    )

    runs = provider.check_runs("group/sub/proj", HEAD)

    assert [run.name for run in runs] == ["pipeline"] * 4
    assert [(run.status, run.conclusion) for run in runs] == [
        ("completed", CheckConclusion.SUCCESS),
        ("completed", CheckConclusion.FAILURE),
        ("in_progress", CheckConclusion.PENDING),
        ("queued", CheckConclusion.PENDING),
    ]
    assert all(run.sha == HEAD and run.app == "gitlab" for run in runs)
    assert calls.only.url.raw_path.decode() == (
        f"/api/v4/projects/group%2Fsub%2Fproj/pipelines?sha={HEAD}"
    )


def test_gitlab_approvals_use_the_merge_request_head_sha() -> None:
    calls = Calls()

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.raw_path.decode() == MR_PATH:
            return httpx.Response(200, json={"iid": 3, "sha": "f" * 40, "diff_refs": {"head_sha": HEAD}})
        assert request.url.raw_path.decode() == f"{MR_PATH}/approvals"
        return httpx.Response(
            200,
            json={"approved": True, "approved_by": [{"user": {"username": "alice"}}]},
        )

    provider = gitlab(calls, handler)

    approvals = provider.approvals("group/sub/proj", 3)

    assert approvals == [
        ApprovalV1(review_id="", reviewer="alice", sha=HEAD, state="approved", dismissed=False, stale=False)
    ]
    assert [call.url.raw_path.decode() for call in calls] == [MR_PATH, f"{MR_PATH}/approvals"]


def test_gitlab_post_review_is_one_note_with_inline_paths() -> None:
    calls = Calls()
    provider = gitlab(calls, json_reply({"id": 4242, "web_url": "https://gitlab.test/note/4242"}))

    posted = provider.post_review("group/sub/proj", 3, REVIEW)

    request = calls.only
    assert request.method == "POST"
    assert request.url.raw_path.decode() == f"{MR_PATH}/notes"
    body = calls.body()
    assert set(body) == {"body"}
    assert body["body"].startswith("Verdict: request changes.")
    assert "**`src/a.py:12`** (RIGHT)" in body["body"]
    assert "Off-by-one." in body["body"]
    assert "**`src/b.py:3`** (LEFT)" in body["body"]
    assert posted.id == "4242"
    assert posted.url == "https://gitlab.test/note/4242"


def test_gitlab_post_status_maps_failure_to_failed() -> None:
    calls = Calls()
    row = {"id": 9, "status": "failed"}
    provider = gitlab(calls, json_reply(row))

    assert provider.post_status("group/sub/proj", HEAD, "failure", "2 findings") == row
    assert calls.only.url.raw_path.decode() == f"/api/v4/projects/group%2Fsub%2Fproj/statuses/{HEAD}"
    assert calls.body() == {
        "state": "failed",
        "name": "zero-loop-reviewer",
        "description": "2 findings",
    }


def test_gitlab_http_error_carries_status() -> None:
    calls = Calls()
    provider = gitlab(calls, json_reply({"message": "404 Project Not Found"}, status=404))

    with pytest.raises(ProviderError) as info:
        provider.pull("group/sub/proj", 3)
    assert info.value.status_code == 404


# --------------------------------------------------------------------------- #
# Bitbucket
# --------------------------------------------------------------------------- #

PP_PATH = "/2.0/repositories/ws/slug/pullrequests/12"


def _bitbucket_pr() -> dict[str, Any]:
    return {
        "id": 12,
        "title": "Add cache",
        "description": "PR-MCI: description",
        "state": "OPEN",
        "draft": False,
        "author": {"nickname": "toil", "display_name": "Toil"},
        "source": {"branch": {"name": "feat/cache"}, "commit": {"hash": HEAD}},
        "destination": {"branch": {"name": "main"}, "commit": {"hash": BASE}},
        "links": {"html": {"href": "https://bitbucket.test/ws/slug/pull-requests/12"}},
        "participants": [
            {"user": {"nickname": "alice"}, "approved": True, "role": "REVIEWER"},
            {"user": {"nickname": "bob"}, "approved": False, "role": "REVIEWER"},
        ],
    }


def test_bitbucket_pull_maps_metadata() -> None:
    calls = Calls()
    provider = bitbucket(calls, json_reply(_bitbucket_pr()))

    pr = provider.pull("ws/slug", 12)

    assert pr == PullRequest(
        repository="ws/slug",
        number=12,
        title="Add cache",
        description="PR-MCI: description",
        author="toil",
        base_ref="main",
        base_sha=BASE,
        head_ref="feat/cache",
        head_sha=HEAD,
        url="https://bitbucket.test/ws/slug/pull-requests/12",
        draft=False,
    )
    assert calls.only.url.raw_path.decode() == PP_PATH
    assert calls.only.headers["Authorization"] == "Bearer bb-t0ken"


def test_bitbucket_diff_is_plain_text() -> None:
    body = "diff --git a/a.py b/a.py\n--- a/a.py\n+++ b/a.py\n@@ -1 +1 @@\n-x = 1\n+x = 2\n"
    calls = Calls()
    provider = bitbucket(calls, lambda request: httpx.Response(200, text=body))

    assert provider.diff("ws/slug", 12) == body
    assert calls.only.url.raw_path.decode() == f"{PP_PATH}/diff"
    assert calls.only.method == "GET"


def test_bitbucket_check_runs_map_build_states() -> None:
    calls = Calls()
    provider = bitbucket(
        calls,
        json_reply(
            {
                "values": [
                    {"key": "ci/tests", "name": "Tests", "state": "SUCCESSFUL"},
                    {"key": "ci/lint", "state": "FAILED"},
                    {"key": "ci/build", "state": "INPROGRESS"},
                    {"key": "ci/old", "state": "STOPPED"},
                    {"key": "ci/weird", "state": "SOMETHING"},
                ]
            }
        ),
    )

    runs = provider.check_runs("ws/slug", HEAD)

    assert [(run.name, run.status, run.conclusion) for run in runs] == [
        ("Tests", "completed", CheckConclusion.SUCCESS),
        ("ci/lint", "completed", CheckConclusion.FAILURE),
        ("ci/build", "in_progress", CheckConclusion.PENDING),
        ("ci/old", "completed", CheckConclusion.CANCELLED),
        ("ci/weird", "completed", CheckConclusion.UNKNOWN),
    ]
    assert all(run.sha == HEAD and run.app == "bitbucket" for run in runs)
    assert calls.only.url.raw_path.decode() == f"/2.0/repositories/ws/slug/commit/{HEAD}/statuses"


def test_bitbucket_approvals_have_no_commit_sha() -> None:
    calls = Calls()
    provider = bitbucket(calls, json_reply(_bitbucket_pr()))

    approvals = provider.approvals("ws/slug", 12)

    assert len(calls) == 1
    assert approvals == [
        ApprovalV1(review_id="", reviewer="alice", sha="", state="approved", dismissed=False, stale=False)
    ]


def test_bitbucket_post_review_sends_content_raw() -> None:
    calls = Calls()
    provider = bitbucket(
        calls,
        json_reply({"id": 77, "links": {"html": {"href": "https://bitbucket.test/comment/77"}}}),
    )

    posted = provider.post_review("ws/slug", 12, REVIEW)

    request = calls.only
    assert request.method == "POST"
    assert request.url.raw_path.decode() == f"{PP_PATH}/comments"
    body = calls.body()
    assert set(body) == {"content"}
    assert body["content"]["raw"].startswith("Verdict: request changes.")
    assert "**`src/a.py:12`** (RIGHT)" in body["content"]["raw"]
    assert posted.id == "77"
    assert posted.url == "https://bitbucket.test/comment/77"


def test_bitbucket_post_status_uses_the_build_endpoint() -> None:
    calls = Calls()
    row = {"key": "zero-loop-reviewer", "state": "FAILED"}
    provider = bitbucket(calls, json_reply(row))

    assert provider.post_status("ws/slug", HEAD, "failure", "2 findings", "https://ci.test/1") == row
    assert calls.only.url.raw_path.decode() == f"/2.0/repositories/ws/slug/commit/{HEAD}/statuses/build"
    assert calls.body() == {
        "state": "FAILED",
        "key": "zero-loop-reviewer",
        "name": "zero-loop-reviewer",
        "description": "2 findings",
        "url": "https://ci.test/1",
    }

    provider.post_status("ws/slug", HEAD, "something-new", "still going")
    assert calls.body(1)["state"] == "INPROGRESS"


def test_bitbucket_http_error_carries_status() -> None:
    calls = Calls()
    provider = bitbucket(calls, json_reply({"error": "nope"}, status=403))

    with pytest.raises(ProviderError) as info:
        provider.approvals("ws/slug", 12)
    assert info.value.status_code == 403
