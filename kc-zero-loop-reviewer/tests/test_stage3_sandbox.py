"""Stage 3: the probe sandbox — argv/script contract, outcome classification,
the dev-only local runner, and a real podman dual-commit run."""

from __future__ import annotations

import os
import shutil
import subprocess
import uuid
from dataclasses import replace
from pathlib import Path

import pytest

from tests.support.gitfix import make_repo
from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import ProbeSpec, ProbeStatus, sha256_hex
from zero_loop_reviewer.stage3.cascade import classify_pair
from zero_loop_reviewer.stage3.sandbox import (
    DEFAULT_IMAGE,
    EXIT_DELIVERY_FAILED,
    EXIT_RUNNER_MISSING,
    DisabledSandbox,
    LocalSandbox,
    PodmanSandbox,
    SandboxConfigView,
    assertion_summary,
    build_sandbox,
    classify_probe_run,
    container_argv,
    has_project_traceback,
    probe_argv,
    probe_relative_path,
    probe_script,
)
from zero_loop_reviewer.stage1.contract import run_bounded

FIXED = "def add(a, b):\n    return a + b\n"
BUGGY = "def add(a, b):\n    return a - b\n"

#: A real reproduction test: fails against BUGGY, passes against FIXED.
GOOD_PROBE = (
    "from calc import add\n"
    "\n"
    "\n"
    "def test_add_returns_the_sum():\n"
    "    assert add(2, 3) == 5\n"
    "    assert add(0, 0) == 0\n"
)


# --------------------------------------------------------------------------- #
# Pure contract: argv, script, paths
# --------------------------------------------------------------------------- #


def _view(**kwargs) -> SandboxConfigView:
    base = dict(work_dir=Path("/home/reviewer/.local/state/zero-loop-reviewer/work"))
    base.update(kwargs)
    return SandboxConfigView(**base)


def test_container_argv_carries_every_isolation_flag():
    scratch = Path("/home/reviewer/.local/state/work/probe-abc123")
    script = probe_script(".probe/test_probe.py", "a" * 64, image=DEFAULT_IMAGE)
    argv = container_argv(_view(), scratch, script)
    text = " ".join(argv)
    assert argv[:2] == ["podman", "run"]
    assert "--rm" in argv
    assert "--network=none" in argv
    assert "--read-only" in argv
    assert "--cap-drop=ALL" in argv
    assert argv[argv.index("--security-opt") + 1] == "no-new-privileges"
    assert "--memory=1024m" in argv
    assert "--pids-limit=256" in argv
    assert "--uts=host" in argv
    assert f"--volume={scratch}:/w:rw" in argv
    assert "--workdir=/w" in argv
    assert argv[-3:] == ["/bin/sh", "-c", script]
    assert DEFAULT_IMAGE in argv
    assert "no-new-privileges" in text and "--uts=host" in text


def test_container_argv_binds_the_scratch_copy_not_the_review_worktree():
    worktree = Path("/home/reviewer/review-tree")
    scratch = Path("/home/reviewer/.local/state/work/probe-xyz")
    argv = container_argv(_view(), scratch, "true")
    volumes = [token for token in argv if token.startswith("--volume=")]
    assert volumes == [f"--volume={scratch}:/w:rw"]
    assert not any(str(worktree) in token for token in argv)


def test_probe_script_verifies_the_hash_before_it_runs_pytest():
    sha = sha256_hex(GOOD_PROBE.encode("utf-8"))
    script = probe_script(".probe/test_probe.py", sha, image="img:1")
    assert f'if [ "$got" != "{sha}" ]' in script
    assert "sha256sum" in script
    assert str(EXIT_DELIVERY_FAILED) in script
    assert str(EXIT_RUNNER_MISSING) in script
    assert "python3 -m pytest ./.probe/test_probe.py -q -p no:cacheprovider" in script
    assert script.index("sha256sum") < script.index("python3 -m pytest ./")


def test_probe_script_unittest_framework_skips_the_pytest_preflight():
    script = probe_script(".probe/test_x.py", "b" * 64, framework="unittest")
    assert "python3 -m unittest -v /w/.probe/test_x.py" in script
    assert "python3 -m pytest --version" not in script


def test_local_probe_argv_matches_the_pytest_contract():
    assert probe_argv("python3", ".probe/test_x.py") == [
        "python3",
        "-m",
        "pytest",
        "./.probe/test_x.py",
        "-q",
        "-p",
        "no:cacheprovider",
    ]
    assert probe_argv("python3", ".probe/test_x.py", framework="unittest") == [
        "python3",
        "-m",
        "unittest",
        "-v",
        "./.probe/test_x.py",
    ]


@pytest.mark.parametrize(
    "path",
    [".probe/test_probe.py", "tests/test_calc.py", "./tests/test_calc.py"],
)
def test_probe_relative_path_accepts_worktree_paths(path):
    assert probe_relative_path(path).endswith(".py")
    assert not probe_relative_path(path).startswith("/")


@pytest.mark.parametrize(
    "path",
    ["/etc/passwd.py", "../escape.py", "tests/../../escape.py", "", "tests/test.py; rm -rf /", "tests/notes.txt"],
)
def test_probe_relative_path_refuses_anything_that_could_leave_the_worktree(path):
    with pytest.raises(Exception):
        probe_relative_path(path)


# --------------------------------------------------------------------------- #
# Outcome classification
# --------------------------------------------------------------------------- #


def test_classification_table():
    pass_run = classify_probe_run(exit_code=0, stdout="1 passed in 0.01s")
    assert pass_run.status is ProbeStatus.PASS

    failed = classify_probe_run(
        exit_code=1,
        stdout="E       assert -1 == 5\nE        +  where -1 = add(2, 3)\n\n.probe/test_probe.py:6: AssertionError\n1 failed in 0.02s",
    )
    assert failed.status is ProbeStatus.FAIL_BEHAVIORAL
    assert "assert -1 == 5" in failed.assertion_message
    assert failed.failure_kind == "assertion"

    import_error = classify_probe_run(
        exit_code=2,
        stdout="ERROR collecting .probe/test_x.py",
        stderr="E   ModuleNotFoundError: No module named 'nope'",
    )
    assert import_error.status is ProbeStatus.SETUP_ERROR
    assert import_error.failure_kind == "import"

    assert classify_probe_run(exit_code=2, stderr="usage: pytest").status is ProbeStatus.SETUP_ERROR
    assert classify_probe_run(exit_code=3, stderr="INTERNALERROR").status is ProbeStatus.SETUP_ERROR
    no_tests = classify_probe_run(exit_code=5, stdout="no tests ran in 0.01s")
    assert no_tests.status is ProbeStatus.SETUP_ERROR
    assert no_tests.failure_kind == "no_tests"

    assert classify_probe_run(exit_code=124).status is ProbeStatus.TIMEOUT
    assert classify_probe_run(exit_code=137).status is ProbeStatus.TIMEOUT
    assert classify_probe_run(exit_code=None, timed_out=True).status is ProbeStatus.TIMEOUT
    assert classify_probe_run(exit_code=-9).status is ProbeStatus.TIMEOUT

    assert classify_probe_run(exit_code=EXIT_RUNNER_MISSING).status is ProbeStatus.SANDBOX_UNAVAILABLE
    assert classify_probe_run(exit_code=125, stderr="statfs").status is ProbeStatus.SANDBOX_UNAVAILABLE
    delivery = classify_probe_run(exit_code=EXIT_DELIVERY_FAILED, stderr="probe delivery mismatch")
    assert delivery.status is ProbeStatus.INPUT_DELIVERY_FAILED
    assert classify_probe_run(exit_code=None).status is ProbeStatus.SANDBOX_UNAVAILABLE


def test_exit_one_without_a_signal_is_a_setup_error_not_a_failure():
    result = classify_probe_run(exit_code=1, stdout="something unexpected happened")
    assert result.status is ProbeStatus.SETUP_ERROR
    assert result.failure_kind == "exit_1"


def test_a_traceback_in_project_code_is_behavioural():
    trace = (
        "def test_x():\n"
        '>       raise ValueError("boom")\n'
        "E       ValueError: boom\n"
        "\n"
        ".probe/test_probe.py:4: ValueError\n"
    )
    assert has_project_traceback(trace)
    assert not has_project_traceback('File "/usr/local/lib/python3.12/site-packages/_pytest/python.py", line 159')
    assert classify_probe_run(exit_code=1, stdout=trace).status is ProbeStatus.FAIL_BEHAVIORAL


def test_assertion_summary_prefers_the_pytest_e_line():
    text = assertion_summary("", "E       assert 1 == 2\n\n1 failed")
    assert text.startswith("E")
    assert "1 == 2" in text
    assert assertion_summary("", "nothing here") == ""


# --------------------------------------------------------------------------- #
# PodmanSandbox: measured constraints, no container needed
# --------------------------------------------------------------------------- #


def test_podman_refuses_a_scratch_root_the_container_cannot_see(tmp_path):
    worktree = tmp_path / "tree"
    worktree.mkdir()
    sandbox = PodmanSandbox(SandboxConfigView(work_dir=tmp_path / "work"))
    spec = ProbeSpec(test_path=".probe/test_probe.py", content=GOOD_PROBE)
    result = sandbox.run_probe(worktree, spec, expected_sha=sha256_hex(GOOD_PROBE.encode()))
    assert result.status is ProbeStatus.SANDBOX_UNAVAILABLE
    assert "outside" in result.stderr


def test_podman_reports_a_missing_runtime(tmp_path):
    sandbox = PodmanSandbox(SandboxConfigView(runtime="podman-does-not-exist", work_dir=Path("/home/x")))
    ok, detail = sandbox.available()
    assert ok is False
    assert "not installed" in detail


def test_podman_marks_an_unsafe_probe_path_malformed(tmp_path):
    sandbox = PodmanSandbox(SandboxConfigView(work_dir=Path("/home/x")))
    spec = ProbeSpec(test_path="../../etc/evil.py", content=GOOD_PROBE)
    result = sandbox.run_probe(tmp_path, spec, expected_sha=sha256_hex(GOOD_PROBE.encode()))
    assert result.status is ProbeStatus.MALFORMED_PROBE


# --------------------------------------------------------------------------- #
# LocalSandbox: a real pytest run on the host, explicitly not a boundary
# --------------------------------------------------------------------------- #


def _local(tmp_path: Path) -> LocalSandbox:
    return LocalSandbox(SandboxConfigView(work_dir=tmp_path / "work"))


def _tree(tmp_path: Path, source: str) -> Path:
    tree = tmp_path / "tree"
    tree.mkdir()
    (tree / "calc.py").write_text(source, encoding="utf-8")
    return tree


def test_local_sandbox_runs_a_passing_probe(tmp_path):
    sandbox = _local(tmp_path)
    assert sandbox.available()[0] is True
    spec = ProbeSpec(test_path=".probe/test_probe.py", content=GOOD_PROBE)
    result = sandbox.run_probe(
        _tree(tmp_path, FIXED), spec, expected_sha=sha256_hex(GOOD_PROBE.encode())
    )
    assert result.status is ProbeStatus.PASS, result.stderr
    assert result.exit_code == 0
    assert result.input_sha256 == sha256_hex(GOOD_PROBE.encode())
    assert result.container_image.startswith("local:")


def test_local_sandbox_reports_a_behavioural_failure(tmp_path):
    sandbox = _local(tmp_path)
    spec = ProbeSpec(test_path=".probe/test_probe.py", content=GOOD_PROBE)
    result = sandbox.run_probe(
        _tree(tmp_path, BUGGY), spec, expected_sha=sha256_hex(GOOD_PROBE.encode())
    )
    assert result.status is ProbeStatus.FAIL_BEHAVIORAL
    assert "assert -1 == 5" in result.assertion_message
    assert "add(2, 3)" in result.stdout


def test_local_sandbox_reports_a_missing_module_as_a_setup_error(tmp_path):
    sandbox = _local(tmp_path)
    spec = ProbeSpec(
        test_path=".probe/test_probe.py",
        content="import definitely_not_installed\n\n\ndef test_x():\n    assert definitely_not_installed\n",
    )
    result = sandbox.run_probe(
        _tree(tmp_path, FIXED), spec, expected_sha=sha256_hex(spec.content.encode())
    )
    assert result.status is ProbeStatus.SETUP_ERROR
    assert "definitely_not_installed" in (result.stdout + result.stderr)


def test_local_sandbox_refuses_a_mismatched_delivery(tmp_path):
    sandbox = _local(tmp_path)
    spec = ProbeSpec(test_path=".probe/test_probe.py", content=GOOD_PROBE)
    result = sandbox.run_probe(_tree(tmp_path, FIXED), spec, expected_sha="0" * 64)
    assert result.status is ProbeStatus.INPUT_DELIVERY_FAILED
    assert result.exit_code is None


def test_local_sandbox_leaves_no_scratch_behind(tmp_path):
    sandbox = _local(tmp_path)
    spec = ProbeSpec(test_path=".probe/test_probe.py", content=GOOD_PROBE)
    sandbox.run_probe(_tree(tmp_path, FIXED), spec, expected_sha=sha256_hex(GOOD_PROBE.encode()))
    leftovers = list((tmp_path / "work").glob("probe-*"))
    assert leftovers == []


# --------------------------------------------------------------------------- #
# build_sandbox: local is opt-in
# --------------------------------------------------------------------------- #


def test_build_sandbox_selects_the_configured_runner(tmp_path):
    settings = Settings().with_paths(tmp_path / "state")
    assert isinstance(build_sandbox(settings), PodmanSandbox)
    local = replace(settings, sandbox=replace(settings.sandbox, runtime="local"))
    assert isinstance(build_sandbox(local), LocalSandbox)
    disabled = build_sandbox(replace(settings, sandbox=replace(settings.sandbox, runtime="none")))
    assert isinstance(disabled, DisabledSandbox)
    assert disabled.available() == (False, "sandbox.runtime is none; Stage 3 is disabled")


def test_local_fallback_is_only_used_when_the_runtime_is_genuinely_unavailable(tmp_path):
    settings = Settings().with_paths(tmp_path / "state")
    broken = replace(settings.sandbox, runtime="podman-not-installed")
    assert isinstance(build_sandbox(replace(settings, sandbox=replace(broken, local_fallback=True))), LocalSandbox)
    assert isinstance(build_sandbox(replace(settings, sandbox=replace(broken, local_fallback=False))), PodmanSandbox)


# --------------------------------------------------------------------------- #
# Real podman: delivery and setup-error behaviour inside the container
# --------------------------------------------------------------------------- #


@pytest.mark.integration
def test_real_podman_refuses_a_tampered_probe_file(tmp_path):
    image = _sandbox_image()
    state = _container_visible_state()
    try:
        scratch = state / "scratch"
        scratch.mkdir()
        (scratch / "tampered.py").write_text(GOOD_PROBE, encoding="utf-8")
        script = probe_script("tampered.py", "0" * 64, image=image)
        argv = container_argv(SandboxConfigView(work_dir=state, image=image), scratch, script)
        run = run_bounded(argv, cwd=state, timeout_s=120)
        assert run.exit_code == EXIT_DELIVERY_FAILED
        assert "probe delivery mismatch" in run.stderr
        assert "passed" not in run.stdout, "pytest must never run against bytes that failed delivery"
        assert classify_probe_run(exit_code=run.exit_code, stderr=run.stderr).status is ProbeStatus.INPUT_DELIVERY_FAILED
    finally:
        shutil.rmtree(state, ignore_errors=True)


@pytest.mark.integration
def test_real_podman_missing_module_is_a_setup_error(tmp_path):
    image = _sandbox_image()
    state = _container_visible_state()
    try:
        tree = state / "tree"
        tree.mkdir()
        (tree / "calc.py").write_text(FIXED, encoding="utf-8")
        settings = _settings(state, image)
        sandbox = PodmanSandbox(SandboxConfigView.from_settings(settings))
        content = "import definitely_not_installed\n\n\ndef test_x():\n    assert definitely_not_installed is not None\n"
        spec = ProbeSpec(test_path=".probe/test_missing.py", content=content)
        result = sandbox.run_probe(tree, spec, expected_sha=sha256_hex(content.encode()))
        assert result.status is ProbeStatus.SETUP_ERROR, result.stderr
        assert result.failure_kind == "import"
        assert "definitely_not_installed" in (result.stdout + result.stderr)
    finally:
        shutil.rmtree(state, ignore_errors=True)


# --------------------------------------------------------------------------- #
# Real podman: a buggy base commit fails the probe, the fixed head passes it
# --------------------------------------------------------------------------- #


def _sandbox_image() -> str:
    """A precise capability probe: an installed podman *and* an image with pytest."""
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
    state = home / ".cache" / f"zero-loop-stage3-sandbox-{uuid.uuid4().hex[:10]}"
    state.mkdir(parents=True, exist_ok=True)
    return state


def _settings(state: Path, image: str) -> Settings:
    settings = Settings().with_paths(state / "state")
    return replace(settings, sandbox=replace(settings.sandbox, image=image, timeout_s=120.0))


@pytest.mark.integration
def test_real_podman_dual_commit(tmp_path):
    image = _sandbox_image()
    state = _container_visible_state()
    repo_fixture = make_repo(tmp_path)
    base_sha = repo_fixture.commit("buggy add", **{"calc.py": BUGGY})
    head_sha = repo_fixture.commit("fix add", **{"calc.py": FIXED})
    repo = GitRepo(repo_fixture.root)
    work = state / "work"
    base_tree = repo.add_worktree(base_sha, work / "base")
    head_tree = repo.add_worktree(head_sha, work / "head")
    try:
        settings = _settings(state, image)
        sandbox = PodmanSandbox(SandboxConfigView.from_settings(settings))
        ok, detail = sandbox.available()
        assert ok is True, detail
        spec = ProbeSpec(test_path=".probe/test_calc.py", content=GOOD_PROBE, target_symbols=["add"])
        expected = sha256_hex(GOOD_PROBE.encode("utf-8"))

        base_result = sandbox.run_probe(base_tree, spec, expected_sha=expected)
        head_result = sandbox.run_probe(head_tree, spec, expected_sha=expected)

        assert base_result.status is ProbeStatus.FAIL_BEHAVIORAL, base_result.stderr
        assert "assert -1 == 5" in base_result.assertion_message
        assert head_result.status is ProbeStatus.PASS, head_result.stderr
        assert base_result.container_image == image
        assert base_result.input_sha256 == expected
        pair = classify_pair(base_result, head_result)
        assert pair.classification.value == "fix_confirmed"
    finally:
        for tree in (head_tree, base_tree):
            repo.remove_worktree(tree)
        shutil.rmtree(state, ignore_errors=True)


@pytest.mark.integration
def test_real_podman_runs_a_probe_on_an_image_without_pytest(tmp_path):
    if shutil.which("podman") is None:
        pytest.skip("podman is not installed")
    try:
        proc = subprocess.run(
            ["podman", "run", "--rm", "--uts=host", DEFAULT_IMAGE, "python3", "-m", "pytest", "--version"],
            capture_output=True,
            text=True,
            timeout=300,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        pytest.skip(f"podman could not run {DEFAULT_IMAGE}: {exc}")
    if proc.returncode == 0:
        pytest.skip(f"{DEFAULT_IMAGE} already contains pytest, so the fallback cannot be observed")
    state = _container_visible_state()
    repo_fixture = make_repo(tmp_path)
    base_sha = repo_fixture.commit("buggy add", **{"calc.py": BUGGY})
    repo = GitRepo(repo_fixture.root)
    tree = repo.add_worktree(base_sha, state / "work" / "base")
    try:
        settings = _settings(state, DEFAULT_IMAGE)
        sandbox = PodmanSandbox(SandboxConfigView.from_settings(settings))
        spec = ProbeSpec(test_path=".probe/test_calc.py", content=GOOD_PROBE)
        result = sandbox.run_probe(tree, spec, expected_sha=sha256_hex(GOOD_PROBE.encode()))
        # The image has no pytest, so the shipped stdlib runner executes the
        # probe: the assertion still runs and still fails on the buggy commit.
        assert result.status is ProbeStatus.FAIL_BEHAVIORAL, (result.status, result.stderr)
        assert result.exit_code == 1
        assert "add" in (result.stderr + result.stdout)
    finally:
        repo.remove_worktree(tree)
        shutil.rmtree(state, ignore_errors=True)


def test_probe_paths_accept_rust_and_typescript() -> None:
    assert probe_relative_path("tests/zl_probe.rs") == "tests/zl_probe.rs"
    assert probe_relative_path("web/src/probe.test.ts") == "web/src/probe.test.ts"
    assert probe_relative_path("src/probe.js") == "src/probe.js"


def test_rust_probe_must_live_under_tests() -> None:
    from zero_loop_reviewer.errors import SandboxError

    try:
        probe_relative_path("crates/foo/src/probe.rs")
    except SandboxError as exc:
        assert "tests/" in str(exc)
    else:
        raise AssertionError("expected a non-tests Rust path to be refused")


def test_cargo_and_node_scripts_use_the_language_runner() -> None:
    cargo = probe_script("tests/zl_probe.rs", "a" * 64, framework="cargo", image="rust:1")
    assert "cargo test --offline --test zl_probe" in cargo
    assert "CARGO_TARGET_DIR" in cargo
    node = probe_script("web/src/probe.test.ts", "b" * 64, framework="node", image="node:22")
    assert "node --experimental-strip-types --test" in node
    js = probe_script("web/src/probe.test.mjs", "c" * 64, framework="node", image="node:22")
    assert "node --test" in js


def test_go_probe_path_and_image() -> None:
    assert probe_relative_path("calc/calc_test.go") == "calc/calc_test.go"
    from zero_loop_reviewer.stage3.sandbox import SandboxConfigView

    view = SandboxConfigView()
    assert view.image_for("go").endswith("golang:1.23-alpine")
    script = probe_script("calc/calc_test.go", "d" * 64, framework="go", image="go:1")
    assert "go test ./calc -run Test" in script
