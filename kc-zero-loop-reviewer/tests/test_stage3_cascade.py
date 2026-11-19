"""Stage 3: the dual-commit cascade (retention matrix, worktrees, honest
degradation) and the k=5 grounded-vote fallback."""

from __future__ import annotations

import os
import shutil
import subprocess
import uuid
from dataclasses import replace
from pathlib import Path

import pytest

from tests.support.gitfix import make_repo
from tests.support.llm import ExplodingLLM, ScriptedLLM
from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.errors import LLMError, FailureKind
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import (
    Candidate,
    Category,
    ContextSlice,
    DiffBundle,
    Evidence,
    FindingStatus,
    OblivulResult,
    ProbeClassification,
    ProbeResult,
    ProbeSpec,
    ProbeStatus,
    StageId,
    Verification,
    VerificationKind,
    sha256_hex,
)
from zero_loop_reviewer.stage3.cascade import (
    RETAINED_CLASSIFICATIONS,
    classify_pair,
    fallback_verify,
    is_retained,
    run_cascade,
)
from zero_loop_reviewer.stage3.sandbox import DEFAULT_IMAGE, PodmanSandbox, SandboxConfigView

BUGGY = "def add(a, b):\n    return a - b\n"
FIXED = "def add(a, b):\n    return a + b\n"
GOOD_PROBE = (
    "from calc import add\n"
    "\n"
    "\n"
    "def test_add_returns_the_sum():\n"
    "    assert add(2, 3) == 5\n"
    "    assert add(0, 0) == 0\n"
)


def _pass() -> ProbeResult:
    return ProbeResult(status=ProbeStatus.PASS, exit_code=0, stdout="1 passed")


def _fail() -> ProbeResult:
    return ProbeResult(
        status=ProbeStatus.FAIL_BEHAVIORAL,
        exit_code=1,
        stdout="E       assert -1 == 5\n1 failed",
        assertion_message="E       assert -1 == 5",
    )


def _status(status: ProbeStatus, **kwargs) -> ProbeResult:
    return ProbeResult(status=status, **kwargs)


def _candidate() -> Candidate:
    return Candidate(
        id="c-1",
        stage=StageId.SECSEM,
        category=Category.LOGIC,
        title="add() subtracts instead of adding",
        root_cause="calc.add returns a - b, so every sum is wrong.",
        severity=2,
        evidence=Evidence(kind="analyzer", analyzer="semgrep", rule_id="zero-loop.add"),
    )


def _context() -> ContextSlice:
    return ContextSlice(text="calc.py:\n1: def add(a, b):\n2:     return a - b", tokens=20, budget=128_000)


def _probe_answer() -> dict:
    return {
        "test_path": ".probe/test_add.py",
        "content": GOOD_PROBE,
        "target_symbols": ["add"],
        "framework": "pytest",
        "rationale": "add() returns the wrong value",
    }


def _probe_llm() -> ScriptedLLM:
    return ScriptedLLM(answers={"probe": [_probe_answer()]})


def _repo_with_delta(tmp_path: Path):
    fixture = make_repo(tmp_path)
    base_sha = fixture.commit("buggy add", **{"calc.py": BUGGY})
    head_sha = fixture.commit("fix add", **{"calc.py": FIXED})
    repo = GitRepo(fixture.root)
    bundle = repo.diff(base_sha, head_sha)
    return repo, bundle, base_sha, head_sha


# --------------------------------------------------------------------------- #
# The retention matrix
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "base_status, head_status, expected",
    [
        (ProbeStatus.FAIL_BEHAVIORAL, ProbeStatus.PASS, ProbeClassification.FIX_CONFIRMED),
        (ProbeStatus.FAIL_BEHAVIORAL, ProbeStatus.FAIL_BEHAVIORAL, ProbeClassification.BOTH_FAIL),
        (ProbeStatus.PASS, ProbeStatus.FAIL_BEHAVIORAL, ProbeClassification.REGRESSION_CONFIRMED),
        (ProbeStatus.PASS, ProbeStatus.PASS, ProbeClassification.BOTH_PASS),
        (ProbeStatus.SETUP_ERROR, ProbeStatus.PASS, ProbeClassification.SETUP_ERROR),
        (ProbeStatus.PASS, ProbeStatus.SETUP_ERROR, ProbeClassification.SETUP_ERROR),
        (ProbeStatus.TIMEOUT, ProbeStatus.PASS, ProbeClassification.INCONCLUSIVE),
        (ProbeStatus.PASS, ProbeStatus.SANDBOX_UNAVAILABLE, ProbeClassification.INCONCLUSIVE),
        (ProbeStatus.INPUT_DELIVERY_FAILED, ProbeStatus.PASS, ProbeClassification.INCONCLUSIVE),
        (ProbeStatus.CANCELLED, ProbeStatus.FAIL_BEHAVIORAL, ProbeClassification.INCONCLUSIVE),
    ],
)
def test_classify_pair_matrix(base_status, head_status, expected):
    pair = classify_pair(_status(base_status), _status(head_status))
    assert pair.classification is expected
    assert pair.detail


def test_setup_errors_are_discarded_never_retained():
    pair = classify_pair(_status(ProbeStatus.SETUP_ERROR), _status(ProbeStatus.FAIL_BEHAVIORAL))
    assert pair.classification is ProbeClassification.SETUP_ERROR
    assert is_retained(pair.classification) is False


def test_retention_set_matches_the_matrix():
    assert RETAINED_CLASSIFICATIONS == {
        ProbeClassification.OMISSION_CONFIRMED,
        ProbeClassification.FIX_CONFIRMED,
        ProbeClassification.REGRESSION_CONFIRMED,
        ProbeClassification.BOTH_FAIL,
    }
    assert is_retained(ProbeClassification.BOTH_PASS) is False
    assert is_retained(ProbeClassification.INCONCLUSIVE) is False


# --------------------------------------------------------------------------- #
# run_cascade with a deterministic sandbox double
# --------------------------------------------------------------------------- #


class FakeSandbox:
    def __init__(self, results: list[ProbeResult], *, validation: ProbeStatus = ProbeStatus.PASS):
        self.results = results
        self.validation = ProbeResult(status=validation)
        self.calls: list[tuple[Path, ProbeSpec, str]] = []
        self.contents: list[str] = []

    def available(self) -> tuple[bool, str]:
        return True, "fake"

    def run_probe(self, worktree, spec, *, expected_sha, timeout_s=None):
        index = len(self.calls)
        self.calls.append((Path(worktree), spec, expected_sha))
        source = Path(worktree) / "calc.py"
        self.contents.append(source.read_text(encoding="utf-8") if source.exists() else "")
        if index == 0:
            # The first run is the synth-time validation against the head tree.
            return self.validation
        return self.results[index - 1]

    def sources(self) -> list[str]:
        return list(self.contents)


class UnavailableSandbox:
    def available(self) -> tuple[bool, str]:
        return False, "podman is not installed"

    def run_probe(self, *args, **kwargs):  # pragma: no cover - never reached
        raise AssertionError("run_probe must not be called when the sandbox is unavailable")


class BrokenSandbox:
    def available(self) -> tuple[bool, str]:
        raise OSError("the container runtime socket disappeared")

    def run_probe(self, *args, **kwargs):  # pragma: no cover - never reached
        raise AssertionError("run_probe must not be called when the runtime is broken")


def test_cascade_proves_the_fix_through_two_real_worktrees(tmp_path):
    repo, bundle, base_sha, head_sha = _repo_with_delta(tmp_path)
    sandbox = FakeSandbox([_fail(), _pass()])
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, degraded = run_cascade(
        _candidate(), _context(), _probe_llm(), sandbox, repo, bundle, settings
    )
    assert len(pairs) == 1
    assert pairs[0].classification is ProbeClassification.FIX_CONFIRMED
    # A confirmed fix is evidence for approval, not a finding to request.
    assert [c.status for c in candidates] == [FindingStatus.SUPPRESSED]
    fixed = candidates[0]
    assert "fixes the probed behaviour" in fixed.suppression_reason
    assert fixed.verification.kind is VerificationKind.PROBE
    assert fixed.evidence.kind == "probe"
    assert fixed.evidence.probe is pairs[0]
    assert degraded == []
    # The probe ran against two different checkouts of the same repository.
    assert sandbox.sources()[1:] == [BUGGY, FIXED]
    for path, spec, expected in sandbox.calls:
        assert spec.test_path == ".probe/test_add.py"
        assert expected == sha256_hex(GOOD_PROBE.encode("utf-8"))
        assert str(path).startswith(str(settings.paths.work_dir))
        assert not path.exists(), "the cascade must clean up its worktrees"
    assert repo.head_sha() == head_sha and repo.resolve(base_sha)


def test_cascade_rebuilds_the_head_as_base_plus_the_delta_patch(tmp_path):
    repo, bundle, base_sha, _ = _repo_with_delta(tmp_path)
    patch = (
        "--- a/calc.py\n"
        "+++ b/calc.py\n"
        "@@ -1,2 +1,2 @@\n"
        " def add(a, b):\n"
        "-    return a - b\n"
        "+    return a + b\n"
    )
    rebuilt = DiffBundle(base_sha=base_sha, head_sha="0" * 40, files=[], patch_text=patch)
    sandbox = FakeSandbox([_fail(), _pass()])
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, _ = run_cascade(_candidate(), _context(), _probe_llm(), sandbox, repo, rebuilt, settings)
    assert pairs[0].classification is ProbeClassification.FIX_CONFIRMED
    assert candidates[0].status is FindingStatus.SUPPRESSED
    assert sandbox.sources()[1:] == [BUGGY, FIXED]


def test_both_pass_suppresses_the_candidate(tmp_path):
    repo, bundle, _, _ = _repo_with_delta(tmp_path)
    sandbox = FakeSandbox([_pass(), _pass()])
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, _ = run_cascade(_candidate(), _context(), _probe_llm(), sandbox, repo, bundle, settings)
    assert pairs[0].classification is ProbeClassification.BOTH_PASS
    assert candidates[0].status is FindingStatus.SUPPRESSED
    assert candidates[0].verification.kind is VerificationKind.UNVERIFIED
    assert "not reproducible" in candidates[0].suppression_reason


def test_a_regression_is_verified(tmp_path):
    repo, bundle, _, _ = _repo_with_delta(tmp_path)
    sandbox = FakeSandbox([_pass(), _fail()])
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, _ = run_cascade(_candidate(), _context(), _probe_llm(), sandbox, repo, bundle, settings)
    assert pairs[0].classification is ProbeClassification.REGRESSION_CONFIRMED
    assert candidates[0].status is FindingStatus.VERIFIED


def test_a_setup_error_on_the_base_becomes_an_advisory(tmp_path):
    repo, bundle, _, _ = _repo_with_delta(tmp_path)
    sandbox = FakeSandbox(
        [_status(ProbeStatus.SETUP_ERROR, failure_kind="import", stderr="No module named 'nope'"), _pass()]
    )
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, degraded = run_cascade(_candidate(), _context(), _probe_llm(), sandbox, repo, bundle, settings)
    assert pairs[0].classification is ProbeClassification.SETUP_ERROR
    assert candidates[0].status is FindingStatus.ADVISORY
    assert candidates[0].verification.kind is VerificationKind.UNVERIFIED
    assert any("probe_setup_error" in reason and "import" in reason for reason in degraded)
    assert any("published as advisory" in reason for reason in degraded)


def test_a_sandbox_failure_is_never_turned_into_approval(tmp_path):
    repo, bundle, _, _ = _repo_with_delta(tmp_path)
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, degraded = run_cascade(
        _candidate(), _context(), _probe_llm(), UnavailableSandbox(), repo, bundle, settings
    )
    assert pairs == []
    assert candidates[0].status is FindingStatus.UNVERIFIED
    assert any("sandbox_unavailable" in reason for reason in degraded)
    assert candidates[0].suppression_reason == ""


def test_a_broken_runtime_degrades_instead_of_crashing(tmp_path):
    repo, bundle, _, _ = _repo_with_delta(tmp_path)
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, degraded = run_cascade(
        _candidate(), _context(), _probe_llm(), BrokenSandbox(), repo, bundle, settings
    )
    assert pairs == []
    assert candidates[0].status is FindingStatus.UNVERIFIED
    assert any("sandbox_unavailable" in reason and "OSError" in reason for reason in degraded)


def test_an_inconclusive_pair_leaves_the_candidate_unverified(tmp_path):
    repo, bundle, _, _ = _repo_with_delta(tmp_path)
    sandbox = FakeSandbox([_status(ProbeStatus.SANDBOX_UNAVAILABLE), _pass()])
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, degraded = run_cascade(_candidate(), _context(), _probe_llm(), sandbox, repo, bundle, settings)
    assert pairs[0].classification is ProbeClassification.INCONCLUSIVE
    assert candidates[0].status is FindingStatus.UNVERIFIED
    assert any("probe_inconclusive" in reason for reason in degraded)


def test_no_probe_leaves_the_candidate_untouched(tmp_path):
    repo, bundle, _, _ = _repo_with_delta(tmp_path)
    llm = ScriptedLLM(answers={"probe": [{"test_path": "../../x.py", "content": GOOD_PROBE}] * 3})
    sandbox = FakeSandbox([])
    settings = Settings().with_paths(tmp_path / "state")
    candidates, pairs, degraded = run_cascade(_candidate(), _context(), llm, sandbox, repo, bundle, settings)
    assert pairs == [] and sandbox.calls == []
    assert candidates[0].status is FindingStatus.UNVERIFIED
    assert any("no probe survived validation" in reason for reason in degraded)


def test_healthz_stops_the_cascade_before_any_probe(tmp_path):
    repo, bundle, _, _ = _repo_with_delta(tmp_path)
    sandbox = FakeSandbox([_fail(), _pass()])
    settings = Settings().with_paths(tmp_path / "state")
    calls: list[int] = []

    def healthz() -> bool:
        calls.append(1)
        return False

    candidates, pairs, degraded = run_cascade(
        _candidate(), _context(), _probe_llm(), sandbox, repo, bundle, settings, healthz=healthz
    )
    assert calls == [1]
    assert sandbox.calls == []
    assert candidates[0].status is FindingStatus.UNVERIFIED
    assert any("deadline" in reason for reason in degraded)


# --------------------------------------------------------------------------- #
# fallback_verify — k=5 votes
# --------------------------------------------------------------------------- #


def _votes(*verdicts: str) -> ScriptedLLM:
    return ScriptedLLM(
        answers={
            "fallback": [
                {"verdict": verdict, "confidence": 0.8, "rationale": f"vote {index}"}
                for index, verdict in enumerate(verdicts)
            ]
        }
    )


def test_fallback_majority_accept_leaves_the_candidate_unverified_with_a_degraded_reason():
    llm = _votes("accept", "accept", "accept", "reject", "uncertain")
    updated = fallback_verify(_candidate(), _context(), llm, settings=Settings())
    assert len(llm.calls) == 5
    assert all(call["stage"] == "fallback" for call in llm.calls)
    assert updated.status is FindingStatus.UNVERIFIED
    assert updated.verification.kind is VerificationKind.UNVERIFIED
    assert "accept=3" in updated.verification.detail
    assert "degraded: fallback_accept_without_perturbation" in updated.verification.detail


def test_fallback_accept_verifies_only_a_candidate_that_already_carries_a_perturbation():
    candidate = _candidate().model_copy(
        update={
            "verification": Verification(
                kind=VerificationKind.UNVERIFIED,
                counterfactual=OblivulResult(passed=True, detail="guard removed raised measured risk"),
            )
        }
    )
    llm = _votes("accept", "accept", "accept", "accept", "reject")
    updated = fallback_verify(candidate, _context(), llm, settings=Settings())
    assert updated.status is FindingStatus.VERIFIED
    assert updated.verification.kind is VerificationKind.PERTURBATION
    assert updated.verification.counterfactual is not None


def test_fallback_majority_reject_suppresses():
    llm = _votes("reject", "reject", "reject", "accept", "uncertain")
    updated = fallback_verify(_candidate(), _context(), llm, settings=Settings())
    assert updated.status is FindingStatus.SUPPRESSED
    assert "reject=3" in updated.suppression_reason
    assert updated.verification.kind is VerificationKind.UNVERIFIED


def test_fallback_without_a_majority_stays_unverified():
    llm = _votes("accept", "accept", "reject", "reject", "uncertain")
    updated = fallback_verify(_candidate(), _context(), llm, settings=Settings())
    assert updated.status is FindingStatus.UNVERIFIED
    assert "no majority" in updated.verification.detail


def test_fallback_abstains_when_the_model_fails():
    llm = ExplodingLLM(LLMError("upstream down", kind=FailureKind.TIMEOUT))
    updated = fallback_verify(_candidate(), _context(), llm, settings=Settings())
    assert llm.calls == 5
    assert updated.status is FindingStatus.UNVERIFIED
    assert "uncertain=5" in updated.verification.detail


def test_fallback_votes_come_from_settings_and_are_overridable():
    settings = Settings()
    assert settings.llm.monitor.votes == 5
    llm = _votes("accept", "accept", "accept")
    fallback_verify(_candidate(), _context(), llm, settings=settings, votes=3)
    assert len(llm.calls) == 3


def test_fallback_never_touches_an_already_verified_candidate():
    candidate = _candidate().model_copy(
        update={"status": FindingStatus.VERIFIED, "verification": Verification(kind=VerificationKind.PROBE)}
    )
    llm = _votes("reject", "reject", "reject", "reject", "reject")
    assert fallback_verify(candidate, _context(), llm, settings=Settings()) is candidate
    assert llm.calls == []


# --------------------------------------------------------------------------- #
# Real podman through the whole cascade
# --------------------------------------------------------------------------- #


def _sandbox_image() -> str:
    if shutil.which("podman") is None:
        pytest.skip("podman is not installed, so the container sandbox cannot be exercised")
    tried: list[str] = []
    for image in (os.environ.get("ZERO_LOOP_TEST_IMAGE"), "localhost/zero-loop-reviewer-probe:py312", DEFAULT_IMAGE):
        if not image:
            continue
        tried.append(image)
        try:
            proc = subprocess.run(
                ["podman", "run", "--rm", "--uts=host", image, "python3", "-m", "pytest", "--version"],
                capture_output=True,
                text=True,
                timeout=300,
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if proc.returncode == 0:
            return image
    pytest.skip(f"no local image can run pytest (tried {tried}); set ZERO_LOOP_TEST_IMAGE")


def _container_visible_state() -> Path:
    home = Path.home()
    if not home.as_posix().startswith("/home"):
        pytest.skip(f"this podman only bind-mounts /home, and HOME={home}")
    state = home / ".cache" / f"zero-loop-stage3-cascade-{uuid.uuid4().hex[:10]}"
    state.mkdir(parents=True, exist_ok=True)
    return state


@pytest.mark.integration
def test_real_podman_cascade_confirms_the_fix(tmp_path):
    image = _sandbox_image()
    state = _container_visible_state()
    try:
        repo, bundle, _, _ = _repo_with_delta(tmp_path)
        settings = Settings().with_paths(state / "state")
        settings = replace(settings, sandbox=replace(settings.sandbox, image=image))
        sandbox = PodmanSandbox(SandboxConfigView.from_settings(settings))
        candidates, pairs, degraded = run_cascade(
            _candidate(), _context(), _probe_llm(), sandbox, repo, bundle, settings
        )
        assert len(pairs) == 1, degraded
        pair = pairs[0]
        assert pair.base.status is ProbeStatus.FAIL_BEHAVIORAL, pair.base.stderr
        assert pair.head.status is ProbeStatus.PASS, pair.head.stderr
        assert pair.classification is ProbeClassification.FIX_CONFIRMED
        assert candidates[0].status is FindingStatus.SUPPRESSED
        assert candidates[0].verification.kind is VerificationKind.PROBE
        assert candidates[0].verification.probe is pair
        leftovers = list(settings.paths.work_dir.glob("probe-*"))
        assert leftovers == []
    finally:
        shutil.rmtree(state, ignore_errors=True)


def test_both_fail_is_retained_as_advisory_not_verified() -> None:
    from zero_loop_reviewer.stage3.cascade import _apply_pair

    candidate = Candidate(
        id="advisory",
        stage=StageId.PROBE,
        title="test fails on both sides",
        evidence=Evidence(kind="llm_hypothesis"),
        cited_sites=[],
    )
    pair = classify_pair(_status(ProbeStatus.FAIL_BEHAVIORAL), _status(ProbeStatus.FAIL_BEHAVIORAL))
    result = _apply_pair(candidate, pair)
    assert result.status is FindingStatus.ADVISORY
    assert "needs repair" in result.suppression_reason


def test_setup_error_is_advisory_with_a_test_repair_note() -> None:
    from zero_loop_reviewer.stage3.cascade import _apply_pair

    candidate = Candidate(
        id="setup",
        stage=StageId.PROBE,
        title="probe could not run",
        evidence=Evidence(kind="llm_hypothesis"),
        cited_sites=[],
    )
    pair = classify_pair(_status(ProbeStatus.SETUP_ERROR), _status(ProbeStatus.PASS))
    result = _apply_pair(candidate, pair)
    assert result.status is FindingStatus.ADVISORY
    assert "needs repair" in result.suppression_reason


def test_advisory_fallback_requires_a_unanimous_jury() -> None:
    candidate = _candidate().model_copy(update={"status": FindingStatus.ADVISORY})
    accepted = fallback_verify(
        candidate, _context(), _votes("accept", "accept", "accept", "accept", "accept"), settings=Settings()
    )
    assert accepted.status is FindingStatus.ADVISORY
    mixed = fallback_verify(
        candidate,
        _context(),
        _votes("accept", "accept", "accept", "accept", "reject"),
        settings=Settings(),
    )
    assert mixed.status is FindingStatus.SUPPRESSED
    assert "rejected by at least one" in mixed.suppression_reason
