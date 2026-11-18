"""Stage 2A: the Semgrep delta against a real repository and real Semgrep."""

from __future__ import annotations

import shutil
import subprocess

import pytest

from tests.support.gitfix import make_repo
from zero_loop_reviewer.config import SastConfig, Settings
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import AnalyzerFinding, AttemptStatus, DiffBundle, FilePatch, Hunk
from zero_loop_reviewer.stage2.sast import apply_suppressions, patch_caused, run_semgrep


def _semgrep_version() -> str:
    binary = shutil.which("semgrep")
    if binary is None:
        return ""
    try:
        proc = subprocess.run([binary, "--version"], capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.SubprocessError):
        return ""
    return proc.stdout.strip() if proc.returncode == 0 else ""


SEMGREP_VERSION = _semgrep_version()
requires_semgrep = pytest.mark.skipif(
    not SEMGREP_VERSION,
    reason="semgrep is not installed in this environment; the real-analyzer test is skipped",
)

BASE_LEGACY = "def old(v):\n    return eval(v)\n"
BASE_KEEP = "def keep(v):\n    return eval(v)\n"
HEAD_KEEP = BASE_KEEP + "\n\ndef added():\n    return 2\n"
HEAD_VULN = (
    "import hashlib\n"
    "import subprocess\n"
    "import yaml\n"
    "\n"
    "\n"
    "def run(cmd):\n"
    "    subprocess.run(cmd, shell=True)\n"
    "    exec(cmd)\n"
    "    return 1\n"
    "\n"
    "\n"
    "def load(blob):\n"
    "    return yaml.load(blob)\n"
    "\n"
    "\n"
    'password = "hunter2hunter2"\n'
    "\n"
    "\n"
    "def digest(data):\n"
    "    return hashlib.md5(data).hexdigest()\n"
    "\n"
    "\n"
    "def find(db, uid):\n"
    '    return db.execute("SELECT * FROM t WHERE id = " + uid)\n'
    "\n"
    "\n"
    'def login(user, password="hunter2hunter2"):\n'
    "    return user\n"
)


HEAD_SAFE = (
    "import yaml\n"
    "\n"
    "\n"
    "def load_safe(blob):\n"
    "    return yaml.load(blob, Loader=yaml.SafeLoader)\n"
    "\n"
    "\n"
    "def find(db, uid):\n"
    '    return db.execute("SELECT * FROM t WHERE id = ?", (uid,))\n'
)


def _line_of(text: str, needle: str) -> int:
    for index, line in enumerate(text.splitlines(), start=1):
        if needle in line:
            return index
    raise AssertionError(f"{needle!r} not found")


@pytest.fixture(scope="module")
def repo_and_diff(tmp_path_factory):
    root = tmp_path_factory.mktemp("sast")
    fixture = make_repo(root, "repo")
    base = fixture.commit("base", **{"pkg__legacy.py": BASE_LEGACY, "pkg__keep.py": BASE_KEEP})
    head = fixture.commit(
        "head", **{"pkg__vuln.py": HEAD_VULN, "pkg__keep.py": HEAD_KEEP, "pkg__safe.py": HEAD_SAFE}
    )
    repo = GitRepo(fixture.root)
    return fixture.root, repo.diff(base, head)


@pytest.fixture(scope="module")
def semgrep_run(repo_and_diff):
    if not SEMGREP_VERSION:
        pytest.skip("semgrep is not installed in this environment")
    root, diff = repo_and_diff
    settings = Settings(sast=SastConfig(timeout_s=40.0))
    outcome = run_semgrep(root, diff, settings)
    return outcome, diff, settings


# --------------------------------------------------------------------------- #
# Real analyzer
# --------------------------------------------------------------------------- #


def test_semgrep_version_is_reported():
    if not SEMGREP_VERSION:
        pytest.skip("semgrep is not installed in this environment")
    assert SEMGREP_VERSION.split(".")[0].isdigit(), SEMGREP_VERSION


@requires_semgrep
@pytest.mark.integration
def test_run_semgrep_completes_with_a_raw_digest(semgrep_run):
    outcome, diff, _settings = semgrep_run
    assert outcome.status is AttemptStatus.COMPLETED, outcome.detail
    assert len(outcome.raw_sha256) == 64
    assert all(ch in "0123456789abcdef" for ch in outcome.raw_sha256)
    assert "changed file" in outcome.detail
    assert diff.changed_paths()


@requires_semgrep
@pytest.mark.integration
def test_shipped_rules_detect_every_required_cwe(semgrep_run):
    outcome, _diff, _settings = semgrep_run
    rules = {finding.rule_id for finding in outcome.findings}
    cwes = {finding.cwe for finding in outcome.findings}
    assert {
        "subprocess-shell-true",
        "eval-exec",
        "yaml-load-unsafe",
        "hardcoded-password",
        "weak-hash-md5",
        "sql-string-concat",
    } <= rules, sorted(rules)
    assert {"CWE-78", "CWE-95", "CWE-502", "CWE-798", "CWE-328", "CWE-89"} <= cwes, sorted(cwes)
    assert all(f.analyzer == "semgrep" for f in outcome.findings)


@requires_semgrep
@pytest.mark.integration
def test_safe_loader_and_parameterized_sql_are_not_reported(semgrep_run):
    outcome, _diff, _settings = semgrep_run
    assert not [finding for finding in outcome.findings if finding.path == "pkg/safe.py"]
    assert {finding.path for finding in outcome.findings} >= {"pkg/vuln.py", "pkg/keep.py"}


@requires_semgrep
@pytest.mark.integration
def test_a_hardcoded_password_default_is_reported(semgrep_run):
    outcome, _diff, _settings = semgrep_run
    password_findings = [f for f in outcome.findings if f.rule_id == "hardcoded-password-literal"]
    assert {f.path for f in password_findings} == {"pkg/vuln.py"}
    assert _line_of(HEAD_VULN, "password=") in {f.line for f in password_findings}
    assert all(f.cwe == "CWE-798" for f in password_findings)


@requires_semgrep
@pytest.mark.integration
def test_patch_caused_flags_instead_of_dropping(semgrep_run):
    outcome, diff, _settings = semgrep_run
    flagged = patch_caused(outcome.findings, diff)
    assert len(flagged) == len(outcome.findings), "patch_caused must not drop findings"
    vuln = [f for f in flagged if f.path == "pkg/vuln.py"]
    kept = [f for f in flagged if f.path == "pkg/keep.py"]
    assert vuln and all(f.patch_caused for f in vuln)
    assert kept, "the pre-existing eval() in the touched file must still be reported"
    assert all(not f.patch_caused for f in kept)
    assert _line_of(HEAD_VULN, "shell=True") in {f.line for f in vuln if f.rule_id == "subprocess-shell-true"}


# --------------------------------------------------------------------------- #
# Host-side logic (no analyzer needed)
# --------------------------------------------------------------------------- #


def _hunk_diff() -> DiffBundle:
    patch = FilePatch(
        path="pkg/x.py",
        hunks=[
            Hunk(
                old_start=0,
                old_lines=0,
                new_start=1,
                new_lines=0,
                header="@@ -0,0 +1,4 @@",
                lines=[
                    "+import subprocess",
                    "+subprocess.run(cmd, shell=True)  # nosec",
                    "+run(cmd, shell=True)",
                    "+run(cmd, shlex.quote(cmd))  # shell=True",
                ],
            )
        ],
    )
    return DiffBundle(base_sha="a" * 40, head_sha="b" * 40, files=[patch])


def test_patch_caused_only_marks_added_lines():
    diff = _hunk_diff()
    findings = [
        AnalyzerFinding(analyzer="semgrep", rule_id="r.one", path="pkg/x.py", line=2),
        AnalyzerFinding(analyzer="semgrep", rule_id="r.two", path="pkg/x.py", line=40),
        AnalyzerFinding(analyzer="semgrep", rule_id="r.three", path="pkg/other.py", line=1),
        AnalyzerFinding(analyzer="semgrep", rule_id="r.four", path="pkg/x.py", line=1, end_line=3),
    ]
    flagged = patch_caused(findings, diff)
    assert [f.patch_caused for f in flagged] == [True, False, False, True]
    assert [f.rule_id for f in flagged] == ["r.one", "r.two", "r.three", "r.four"]


def test_apply_suppressions_marks_reasons_and_keeps_findings():
    diff = _hunk_diff()
    settings = SastConfig(suppressed_paths=("tests/", "vendor/"), trusted_sanitizers=("shlex.quote",))
    findings = [
        AnalyzerFinding(analyzer="semgrep", rule_id="a", path="tests/test_x.py", line=1, cwe="CWE-78"),
        AnalyzerFinding(analyzer="semgrep", rule_id="b", path="pkg/x.py", line=2, cwe="CWE-78"),
        AnalyzerFinding(analyzer="semgrep", rule_id="c", path="pkg/x.py", line=3, cwe="CWE-78"),
        AnalyzerFinding(analyzer="semgrep", rule_id="d", path="pkg/x.py", line=4, cwe="CWE-78"),
    ]
    marked = apply_suppressions(findings, diff, settings)
    assert len(marked) == 4
    assert [f.suppressed for f in marked] == [True, True, False, True]
    assert "suppression list" in marked[0].suppression_reason
    assert "inline" in marked[1].suppression_reason
    assert "shlex.quote" in marked[3].suppression_reason
    assert marked[2].suppression_reason == ""


def test_disabled_sast_skips_without_running_an_analyzer(tmp_path):
    root = tmp_path
    diff = DiffBundle(base_sha="a" * 40, head_sha="b" * 40, files=[])
    outcome = run_semgrep(root, diff, Settings(sast=SastConfig(enabled=False)))
    assert outcome.status is AttemptStatus.SKIPPED
    assert outcome.findings == []


def test_missing_worktree_root_is_a_typed_failure(tmp_path):
    outcome = run_semgrep(tmp_path / "does-not-exist", _hunk_diff(), Settings())
    assert outcome.status is AttemptStatus.INPUT_DELIVERY_FAILED
    assert "does not exist" in outcome.detail


def test_missing_semgrep_binary_is_a_typed_failure(tmp_path):
    (tmp_path / "pkg").mkdir()
    (tmp_path / "pkg" / "a.py").write_text("eval(x)\n", encoding="utf-8")
    diff = _hunk_diff()
    diff.files[0].path = "pkg/a.py"
    settings = Settings(sast=SastConfig(semgrep_bin="definitely-not-a-semgrep-binary"))
    outcome = run_semgrep(tmp_path, diff, settings)
    assert outcome.status is AttemptStatus.TOOL_UNAVAILABLE
    assert "not installed" in outcome.detail


def test_missing_configured_rules_is_a_typed_failure(tmp_path):
    (tmp_path / "pkg").mkdir()
    (tmp_path / "pkg" / "a.py").write_text("eval(x)\n", encoding="utf-8")
    diff = _hunk_diff()
    diff.files[0].path = "pkg/a.py"
    settings = Settings(sast=SastConfig(semgrep_config="/nonexistent/rules.yml"))
    outcome = run_semgrep(tmp_path, diff, settings)
    assert outcome.status is AttemptStatus.TOOL_UNAVAILABLE
    assert "does not exist" in outcome.detail


def test_inline_rust_test_module_is_test_context(tmp_path) -> None:
    from zero_loop_reviewer.stage2.sast import _in_test_context

    source = (
        "pub fn parse(raw: Option<i32>) -> i32 {\n"
        "    raw.unwrap()\n"
        "}\n"
        "\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    #[test]\n"
        "    fn parses() {\n"
        "        assert_eq!(super::parse(Some(1)), 1);\n"
        "    }\n"
        "}\n"
    )
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "lib.rs").write_text(source, encoding="utf-8")
    assert _in_test_context(tmp_path, "src/lib.rs", 2) is False
    assert _in_test_context(tmp_path, "src/lib.rs", 9) is True
    assert _in_test_context(tmp_path, "tests/helper.rs", 2) is True
