"""Tests for the Stage 1 exact-head / branch-stack gate."""

from __future__ import annotations

from dataclasses import dataclass, field

from tests.support.gitfix import make_repo, run
from zero_loop_reviewer.config import GateConfig
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import (
    ApprovalV1,
    CheckConclusion,
    CheckRunV1,
    GateDecision,
    GateSnapshot,
    StackedBranch,
)
from zero_loop_reviewer.providers.base import PullRequest
from zero_loop_reviewer.stage1.gatekeeper import HeadCommitGate, evaluate_gate

BOT = "zero-loop-reviewer[bot]"
HEAD = "a" * 40
OLD = "b" * 40
CONFIG = GateConfig(bot_login=BOT, required_checks=("ci/tests",))
#: The opt-in merge-gate mode: approvals are required, not just recorded.
APPROVAL_CONFIG = GateConfig(bot_login=BOT, required_checks=("ci/tests",), require_approval=True)


@dataclass
class FakeProvider:
    """Duck-typed provider: only the methods the gate actually calls."""

    pull_request: PullRequest
    checks: list[CheckRunV1] = field(default_factory=list)
    approval_rows: list[ApprovalV1] = field(default_factory=list)
    repo: GitRepo | None = None
    bot: str = BOT
    name: str = "fake"

    def pull(self, repository: str, number: int) -> PullRequest:
        return self.pull_request

    def check_runs(self, repository: str, sha: str) -> list[CheckRunV1]:
        # Deliberately unfiltered: the gate must do the exact-SHA filtering.
        return list(self.checks)

    def approvals(self, repository: str, number: int) -> list[ApprovalV1]:
        return list(self.approval_rows)

    def bot_login(self) -> str:
        return self.bot


def _approval(sha: str = HEAD, **kwargs) -> ApprovalV1:
    payload = {"review_id": "R1", "reviewer": "toil", "sha": sha, "state": "approved"}
    payload.update(kwargs)
    return ApprovalV1(**payload)


def _check(name: str = "ci/tests", sha: str = HEAD, **kwargs) -> CheckRunV1:
    payload = {"name": name, "sha": sha, "status": "completed", "conclusion": CheckConclusion.SUCCESS}
    payload.update(kwargs)
    return CheckRunV1(**payload)


def _snapshot(**kwargs) -> GateSnapshot:
    payload = {
        "repository": "acme/widgets",
        "pull_number": 7,
        "base_sha": OLD,
        "head_sha": HEAD,
        "expected_head_sha": HEAD,
        "required_checks": ["ci/tests"],
        "checks": [_check()],
        "approvals": [_approval()],
        "bot_login": BOT,
    }
    payload.update(kwargs)
    return GateSnapshot(**payload)


# --------------------------------------------------------------------------- #
# Pure evaluation
# --------------------------------------------------------------------------- #


def test_allows_exact_head_with_green_check_and_approval():
    result = evaluate_gate(_snapshot(), CONFIG)
    assert result.decision is GateDecision.ALLOW
    assert result.effective_head == HEAD
    assert result.missing_checks == []
    assert result.failing_checks == []
    assert result.discounted_approvals == []
    assert result.reasons and "allowed" in result.reasons[-1]


def test_refuses_stale_head():
    result = evaluate_gate(_snapshot(expected_head_sha=OLD), CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert result.effective_head == ""
    assert any("stale head" in reason for reason in result.reasons)


def test_refuses_stale_approval_only_on_older_commit():
    snapshot = _snapshot(approvals=[_approval(sha=OLD)])
    result = evaluate_gate(snapshot, APPROVAL_CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert result.discounted_approvals, "the stale approval must be reported"
    assert any("approved" in entry and "head is" in entry for entry in result.discounted_approvals)
    assert any("no approval covers head" in reason for reason in result.reasons)


def test_a_review_trigger_does_not_require_an_approval():
    """The default mode reviews a fresh PR: no human approval exists yet."""
    result = evaluate_gate(_snapshot(), CONFIG)
    assert result.decision is GateDecision.ALLOW
    assert result.discounted_approvals == []


def test_discounted_dismissed_and_stale_approvals():
    snapshot = _snapshot(
        approvals=[
            _approval(review_id="R1", dismissed=True),
            _approval(review_id="R2", reviewer="other", stale=True),
        ]
    )
    result = evaluate_gate(snapshot, APPROVAL_CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert len(result.discounted_approvals) == 2
    assert any("dismissed" in entry for entry in result.discounted_approvals)
    assert any("stale" in entry for entry in result.discounted_approvals)


def test_bot_review_never_counts_as_an_approval():
    snapshot = _snapshot(approvals=[_approval(reviewer=BOT)])
    result = evaluate_gate(snapshot, APPROVAL_CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert any("self-approval by the review bot" in entry for entry in result.discounted_approvals)


def test_actor_is_the_bot_is_refused():
    result = evaluate_gate(_snapshot(actor_login=BOT), CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert any("self-approval" in reason for reason in result.reasons)


def test_green_check_on_older_commit_is_a_missing_check():
    snapshot = _snapshot(checks=[_check(sha=OLD)])
    result = evaluate_gate(snapshot, CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert result.missing_checks == ["ci/tests"]
    assert result.failing_checks == []


def test_failing_required_check_refuses():
    snapshot = _snapshot(checks=[_check(conclusion=CheckConclusion.FAILURE)])
    result = evaluate_gate(snapshot, CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert result.failing_checks == ["ci/tests"]
    assert result.missing_checks == []


def test_one_failing_run_of_a_required_check_refuses():
    snapshot = _snapshot(checks=[_check(), _check(conclusion=CheckConclusion.NEUTRAL)])
    result = evaluate_gate(snapshot, CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert result.failing_checks == ["ci/tests"]


def test_required_check_names_come_from_config_and_snapshot():
    snapshot = _snapshot(required_checks=["lint"])
    result = evaluate_gate(snapshot, CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert result.missing_checks == ["lint"]


def test_stacked_branches_can_be_refused_by_configuration():
    snapshot = _snapshot(stacked=[StackedBranch(branch="stack", sha=HEAD, role="intermediate")])
    result = evaluate_gate(snapshot, GateConfig(bot_login=BOT, required_checks=("ci/tests",), allow_stacked_branches=False))
    assert result.decision is GateDecision.REFUSE
    assert any("stacked branches are disabled" in reason for reason in result.reasons)


def test_stale_stacked_branch_is_refused():
    snapshot = _snapshot(
        head_is_ancestor_of_branch=False,
        stacked=[StackedBranch(branch="feature/stack-2", sha=OLD, role="intermediate")],
    )
    result = evaluate_gate(snapshot, CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert result.stale_branches == ["feature/stack-2"]
    assert any("not an ancestor" in reason for reason in result.reasons)


# --------------------------------------------------------------------------- #
# Collection from a provider (real git for ancestry)
# --------------------------------------------------------------------------- #


def _stacked_repo(tmp_path):
    fixture = make_repo(tmp_path, "stack")
    base = fixture.commit("base", **{"pkg__a.py": "def parse(x):\n    return x\n"})
    head = fixture.commit("head", **{"pkg__b.py": "def use():\n    return parse(1)\n"})
    return fixture, base, head


def test_collect_and_evaluate_with_a_stale_stacked_branch(tmp_path):
    fixture, base, head = _stacked_repo(tmp_path)
    # The stacked branch was left behind at the base commit: the head is *not*
    # its ancestor, so the stack is stale.
    run(fixture.root, "branch", "feature/stack-2", base)
    provider = FakeProvider(
        pull_request=PullRequest(repository="acme/widgets", number=7, base_sha=base, head_sha=head),
        checks=[_check(sha=head)],
        approval_rows=[_approval(sha=head)],
        repo=GitRepo(fixture.root),
    )
    gate = HeadCommitGate(
        provider, CONFIG, stacked=[StackedBranch(branch="feature/stack-2", sha=base, role="intermediate")]
    )
    snapshot = gate.collect("acme/widgets", 7, expected_head_sha=head, actor_login="toil")
    assert snapshot.head_sha == head
    assert snapshot.base_sha == base
    assert snapshot.expected_head_sha == head
    assert snapshot.bot_login == BOT
    assert snapshot.head_is_ancestor_of_branch is False
    assert [entry.branch for entry in snapshot.stacked] == ["feature/stack-2"]

    result = gate.evaluate("acme/widgets", 7, expected_head_sha=head, actor_login="toil")
    assert result.decision is GateDecision.REFUSE
    assert result.stale_branches == ["feature/stack-2"]


def test_collect_and_evaluate_with_a_healthy_stack(tmp_path):
    fixture, base, head = _stacked_repo(tmp_path)
    fixture.branch("feature/stack-2")
    stacked_commit = fixture.commit("on top", **{"pkg__c.py": "def more():\n    return 2\n"})
    fixture.checkout("main")
    provider = FakeProvider(
        pull_request=PullRequest(repository="acme/widgets", number=7, base_sha=base, head_sha=head),
        checks=[_check(sha=head), _check(sha=stacked_commit)],
        approval_rows=[_approval(sha=head)],
        repo=GitRepo(fixture.root),
    )
    gate = HeadCommitGate(
        provider,
        CONFIG,
        stacked=[StackedBranch(branch="feature/stack-2", sha=stacked_commit, role="intermediate")],
    )
    result = gate.evaluate("acme/widgets", 7, expected_head_sha=head, actor_login="toil")
    assert result.decision is GateDecision.ALLOW, result.reasons
    assert result.stale_branches == []
    assert result.effective_head == head


def test_collect_reports_a_missing_stacked_branch(tmp_path):
    fixture, base, head = _stacked_repo(tmp_path)
    provider = FakeProvider(
        pull_request=PullRequest(repository="acme/widgets", number=7, base_sha=base, head_sha=head),
        checks=[_check(sha=head)],
        approval_rows=[_approval(sha=head)],
        repo=GitRepo(fixture.root),
    )
    gate = HeadCommitGate(
        provider, CONFIG, stacked=[StackedBranch(branch="feature/missing", sha="", role="intermediate")]
    )
    result = gate.evaluate("acme/widgets", 7)
    assert result.decision is GateDecision.REFUSE
    assert "feature/missing" in result.stale_branches
    assert any("cannot resolve" in note for note in gate.last_notes)


MID = "c" * 40


def _stacked():
    return [StackedBranch(branch="stack/middle", sha=MID, role="intermediate")]


def test_required_checks_must_pass_on_each_intermediate_stacked_branch():
    snapshot = _snapshot(
        stacked=_stacked(),
        checks=[_check(), _check(sha=MID)],
    )
    result = evaluate_gate(snapshot, CONFIG)
    assert result.decision is GateDecision.ALLOW, result.reasons


def test_a_green_check_only_on_head_does_not_cover_a_stacked_branch():
    snapshot = _snapshot(stacked=_stacked(), checks=[_check()])
    result = evaluate_gate(snapshot, CONFIG)
    assert result.decision is GateDecision.REFUSE
    assert any("stacked branch stack/middle" in reason for reason in result.reasons)


def test_a_failing_check_on_a_stacked_branch_refuses():
    snapshot = _snapshot(
        stacked=_stacked(),
        checks=[_check(), _check(sha=MID, conclusion=CheckConclusion.FAILURE)],
    )
    result = evaluate_gate(snapshot, CONFIG)
    assert result.decision is GateDecision.REFUSE
