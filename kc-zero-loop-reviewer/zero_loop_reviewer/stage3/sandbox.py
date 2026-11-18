"""Stage 3 — the probe sandbox.

Two runners implement :class:`Sandbox`:

``PodmanSandbox``
    Runs the probe inside ``podman run --rm --network none --read-only
    --cap-drop=ALL --security-opt no-new-privileges --memory=… --pids-limit=…
    --uts=host`` against a writable bind of a **scratch copy** of the worktree
    (never the review worktree itself).  Before the probe runs, the container
    recomputes ``sha256sum`` over the delivered probe file and refuses to run a
    byte that differs from what the host sent (``exit 97`` →
    :attr:`ProbeStatus.INPUT_DELIVERY_FAILED`).

``LocalSandbox``
    Development only.  **It is not a security boundary**: the probe runs in the
    reviewer's own process tree with the reviewer's own privileges.  It is only
    built when the operator opts in (``sandbox.runtime = local`` or
    ``sandbox.local_fallback = true`` on a host with no container runtime).

Exit-code contract (pytest inside the container)::

    0     pass
    1     a test failed (AssertionError / a traceback in project code)
    2     usage or collection error          -> setup_error
    3, 4  internal / usage error             -> setup_error
    5     no tests collected                 -> setup_error
    96    the image cannot run pytest        -> sandbox_unavailable
    97    the probe file did not survive delivery -> input_delivery_failed
    124   the runner was killed on the deadline -> timeout
    125-127 podman could not start the container -> sandbox_unavailable
    137   SIGKILL (deadline or OOM)          -> timeout

A generated test's **setup error never counts against the PR**: every
non-behavioural outcome is reported as ``setup_error``/``timeout``/
``sandbox_unavailable`` and Stage 3 discards it.
"""

from __future__ import annotations

import re
import shlex
import shutil
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Mapping, Protocol, Sequence, runtime_checkable

from ..adapters import BUILTIN_ADAPTERS, ProbeAdapter, adapter_for, adapters_map
from ..config import Settings
from ..errors import SandboxError
from ..models import AttemptStatus, ProbeResult, ProbeSpec, ProbeStatus
from ..stage1.contract import deliver_file, run_bounded

#: Directory the container sees the scratch copy at.
PROBE_ROOT = "/w"

#: Marks used by the in-container preflight.
EXIT_RUNNER_MISSING = 96
EXIT_DELIVERY_FAILED = 97

#: Default image (matches ``SandboxConfig.SandboxConfig.image``).
DEFAULT_IMAGE = "docker.io/library/python:3.12-alpine"

#: Scratch entries that are never copied: VCS metadata and tool caches.
_IGNORE = shutil.ignore_patterns(
    ".git",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "*.pyc",
)

_SAFE_RELATIVE = re.compile(r"^[A-Za-z0-9_.][A-Za-z0-9_./-]*$")

#: ``(needle, failure_kind)`` for output that means the probe could not run.
_SETUP_MARKERS: tuple[tuple[str, str], ...] = (
    ("modulenotfounderror", "import"),
    ("importerror", "import"),
    ("cannot import name", "import"),
    ("syntaxerror", "syntax"),
    ("indentationerror", "syntax"),
    ("error collecting", "collection"),
    ("errors during collection", "collection"),
    ("error during collection", "collection"),
    ("no tests ran", "no_tests"),
    ("fixture", "fixture"),
    ("usage error", "usage"),
    ("unrecognized arguments", "usage"),
    ("could not compile", "compile"),
    ("failed to compile", "compile"),
    ("error[E", "compile"),
    ("error TS", "typescript"),
    ("cannot find module", "import"),
    ("module not found", "import"),
    ("failed to load manifest", "manifest"),
    ("no test target", "no_tests"),
    ("undefined:", "compile"),
)

#: ``needle`` → failure kind for a genuine behavioural failure.
_BEHAVIOURAL_MARKERS: tuple[tuple[str, str], ...] = (
    ("assertionerror", "assertion"),
    ("e   assert", "assertion"),
    ("failed ", "assertion"),
    ("failed\n", "assertion"),
    ("exception raised", "exception"),
    ("test result: failed", "assertion"),
    ("panicked at", "panic"),
    ("assertion failed", "assertion"),
    ("not ok", "assertion"),
    ("--- fail", "assertion"),
    ("fail\t", "assertion"),
)

_TRACEBACK = re.compile(r'(?:File "(?P<file>[^"]+\.py)", line (?P<line>\d+))|(?m:^(?P<short>[\w./-]+\.py):(?P<shortline>\d+):)', re.MULTILINE)
_E_LINE = re.compile(r"^E\s{2,}\S")


# --------------------------------------------------------------------------- #
# Configuration view
# --------------------------------------------------------------------------- #


@dataclass(frozen=True)
class SandboxConfigView:
    """The slice of ``Settings`` a sandbox needs, with the measured constraints.

    ``visible_roots`` encodes a measured property of this deployment: podman
    rootless here can only bind-mount paths under ``/home`` (``statfs /tmp/…``
    fails), so a scratch directory outside those roots is refused with
    ``sandbox_unavailable`` instead of producing a confusing mount error.
    Pass ``visible_roots=()`` on a host without that restriction.
    """

    runtime: str = "podman"
    image: str = DEFAULT_IMAGE
    rust_image: str = "docker.io/library/rust:1.89-slim"
    node_image: str = "docker.io/library/node:22-alpine"
    go_image: str = "docker.io/library/golang:1.23-alpine"
    helper_image: str = "docker.io/library/alpine:latest"
    timeout_s: float = 120.0
    memory_mb: int = 1024
    cpus: float = 1.0
    pids_limit: int = 256
    network: str = "none"
    extra_args: tuple[str, ...] = ("--uts=host",)
    local_fallback: bool = False
    work_dir: Path = Path.home() / ".local/state/zero-loop-reviewer/work"
    visible_roots: tuple[str, ...] = ("/home",)
    adapters: Mapping[str, ProbeAdapter] = field(default_factory=lambda: adapters_map(BUILTIN_ADAPTERS))

    @classmethod
    def from_settings(cls, settings: Settings) -> "SandboxConfigView":
        sandbox = settings.sandbox
        return cls(
            runtime=str(sandbox.runtime or "none"),
            image=str(sandbox.image or DEFAULT_IMAGE),
            rust_image=str(sandbox.rust_image or "docker.io/library/rust:1.89-slim"),
            node_image=str(sandbox.node_image or "docker.io/library/node:22-alpine"),
            go_image=str(sandbox.go_image or "docker.io/library/golang:1.23-alpine"),
            helper_image=str(sandbox.helper_image or "docker.io/library/alpine:latest"),
            timeout_s=float(sandbox.timeout_s),
            memory_mb=int(sandbox.memory_mb),
            cpus=float(sandbox.cpus),
            pids_limit=int(sandbox.pids_limit),
            network=str(sandbox.network or "none"),
            extra_args=tuple(sandbox.extra_args),
            local_fallback=bool(sandbox.local_fallback),
            work_dir=Path(settings.paths.work_dir),
            adapters=adapters_map(settings.probe_adapters),
        )

    def scratch_parent(self) -> Path:
        return Path(self.work_dir)

    def image_for(self, framework: str) -> str:
        adapter = self.adapters.get(framework)
        if adapter is not None and adapter.image:
            return adapter.image
        if framework == "cargo":
            return self.rust_image
        if framework in ("node", "tsx"):
            return self.node_image
        if framework == "go":
            return self.go_image
        return self.image

    def visibility_problem(self) -> str:
        """Return "" when the scratch root can be bind-mounted, else the reason."""
        root = Path(self.work_dir)
        if not root.is_absolute():
            return f"the scratch root {root} is not an absolute path"
        if not self.visible_roots:
            return ""
        text = root.as_posix().rstrip("/")
        for allowed in self.visible_roots:
            prefix = allowed.rstrip("/")
            if text == prefix or text.startswith(prefix + "/"):
                return ""
        return (
            f"the scratch root {root} is outside {list(self.visible_roots)}; this podman cannot "
            "bind-mount it (only these roots are visible to the container), so a probe cannot be delivered"
        )


# --------------------------------------------------------------------------- #
# Probe paths, scripts and argv (pure functions, unit-tested)
# --------------------------------------------------------------------------- #


def probe_relative_path(test_path: str, *, extensions: tuple[str, ...] | None = None) -> str:
    """Normalise a probe path and refuse anything that could leave the worktree."""
    text = str(test_path or "").strip().replace("\\", "/")
    while text.startswith("./"):
        text = text[2:]
    if not text:
        raise SandboxError("the probe has no test_path")
    if text.startswith("/") or ".." in Path(text).parts:
        raise SandboxError(f"probe path {test_path!r} must stay inside the worktree")
    if not _SAFE_RELATIVE.match(text):
        raise SandboxError(f"probe path {test_path!r} contains characters a shell would interpret")
    allowed = extensions or (".py", ".rs", ".go", ".ts", ".tsx", ".js", ".mjs", ".cjs")
    suffix = Path(text).suffix.lower()
    if suffix not in allowed:
        raise SandboxError(
            f"probe path {test_path!r} does not end in one of {list(allowed)}"
        )
    if text.endswith(".go") and not text.endswith("_test.go"):
        raise SandboxError("a Go probe must be a *_test.go file")
    if text.endswith(".rs") and "/tests/" not in f"/{text}":
        raise SandboxError("a Rust probe must live under a tests/ directory so cargo test can discover it")
    return text


def _expand_command(command: Sequence[str], rel_path: str) -> list[str]:
    values = {
        "{test_path}": rel_path,
        "{test_dir}": str(Path(rel_path).parent) or ".",
        "{test_stem}": Path(rel_path).stem,
        "{name}": Path(rel_path).stem,
    }
    out: list[str] = []
    for part in command:
        for needle, value in values.items():
            part = part.replace(needle, value)
        out.append(part)
    return out


def probe_argv(
    python: str,
    rel_path: str,
    *,
    framework: str = "pytest",
    adapter_command: Sequence[str] | None = None,
) -> list[str]:
    """The command the local runner executes once the probe file is in place."""
    target = f"./{rel_path}"
    if adapter_command:
        return _expand_command(adapter_command, rel_path)
    if adapter_command:
        preflight = ""
        command = " ".join(shlex.quote(part) for part in _expand_command(adapter_command, rel_path))
    elif framework == "cargo":
        stem = Path(rel_path).stem
        crate = rel_path.split("/tests/", 1)[0] if "/tests/" in rel_path else "."
        return [
            "cargo",
            "test",
            "--offline",
            "--manifest-path",
            f"{crate}/Cargo.toml",
            "--test",
            stem,
            "--",
            "--nocapture",
        ]
    if framework == "go":
        directory = str(Path(rel_path).parent) or "."
        return ["go", "test", f"./{directory}", "-run", "Test"]
    if framework == "tsx":
        return ["npx", "--no-install", "tsx", "--test", target]
    if framework == "node":
        if rel_path.endswith(".ts"):
            return ["node", "--experimental-strip-types", "--test", target]
        return ["node", "--test", target]
    if framework == "unittest":
        return [python, "-m", "unittest", "-v", target]
    return [python, "-m", "pytest", target, "-q", "-p", "no:cacheprovider"]


def probe_script(
    rel_path: str,
    expected_sha: str,
    *,
    framework: str = "pytest",
    image: str = "",
    adapter_command: Sequence[str] | None = None,
) -> str:
    """The POSIX shell script that runs inside the container.

    Order matters: verify the delivered bytes, verify the image can run the
    runner, then run the probe.  A mismatch exits 97 and the probe never runs.
    """
    target = f"{PROBE_ROOT}/{rel_path}"
    # The image name only appears in a human-readable message; keep it inert.
    safe_image = "".join(ch for ch in (image or "(unknown)") if ch not in '"\\$`\n')
    if adapter_command:
        preflight = ""
        command = " ".join(shlex.quote(part) for part in _expand_command(adapter_command, rel_path))
    elif framework == "cargo":
        stem = Path(rel_path).stem
        crate = rel_path.split("/tests/", 1)[0] if "/tests/" in rel_path else "."
        preflight = (
            "command -v cargo >/dev/null 2>&1 || {\n"
            f'  echo "the sandbox image {safe_image} has no cargo" >&2\n'
            f"  exit {EXIT_RUNNER_MISSING}\n"
            "}\n"
        )
        command = (
            "export CARGO_TARGET_DIR=/tmp/zl-target\n"
            f"cd {PROBE_ROOT}/{crate}\n"
            f"cargo test --offline --test {stem} -- --nocapture"
        )
    elif framework == "node":
        preflight = (
            "command -v node >/dev/null 2>&1 || {\n"
            f'  echo "the sandbox image {safe_image} has no node" >&2\n'
            f"  exit {EXIT_RUNNER_MISSING}\n"
            "}\n"
        )
        if rel_path.endswith(".ts"):
            command = f"node --experimental-strip-types --test ./{rel_path}"
        else:
            command = f"node --test ./{rel_path}"
    elif framework == "go":
        directory = str(Path(rel_path).parent) or "."
        preflight = (
            "command -v go >/dev/null 2>&1 || {\n"
            f'  echo "the sandbox image {safe_image} has no go" >&2\n'
            f"  exit {EXIT_RUNNER_MISSING}\n"
            "}\n"
        )
        command = f"go test ./{directory} -run {"Test"}"
    elif framework == "tsx":
        preflight = (
            "command -v npx >/dev/null 2>&1 || {\n"
            f'  echo "the sandbox image {safe_image} has no npx" >&2\n'
            f"  exit {EXIT_RUNNER_MISSING}\n"
            "}\n"
        )
        command = f"npx --no-install tsx --test ./{rel_path}"
    elif framework == "unittest":
        preflight = ""
        command = f"python3 -m unittest -v {target}"
    else:
        # pytest when the image has it; otherwise the runner copied beside the
        # probe (same assertions, no third-party dependency).  Refusing to run
        # at all would turn every probe into "sandbox unavailable" merely
        # because the image is a stock python one.
        preflight = (
            "command -v python3 >/dev/null 2>&1 || {\n"
            f'  echo "the sandbox image {safe_image} has no python3" >&2\n'
            f"  exit {EXIT_RUNNER_MISSING}\n"
            "}\n"
        )
        command = (
            "if python3 -m pytest --version >/dev/null 2>&1; then\n"
            f"  python3 -m pytest ./{rel_path} -q -p no:cacheprovider\n"
            "else\n"
            f"  python3 ./.zl_probe_runner.py ./{rel_path}\n"
            "fi"
        )
    return (
        "set -eu\n"
        f"cd {PROBE_ROOT}\n"
        f'if [ ! -f "{target}" ]; then\n'
        '  echo "the probe file is missing inside the container" >&2\n'
        f"  exit {EXIT_DELIVERY_FAILED}\n"
        "fi\n"
        f'got=$(sha256sum "{target}" | cut -d\' \' -f1)\n'
        f'if [ "$got" != "{expected_sha}" ]; then\n'
        f'  echo "probe delivery mismatch: expected {expected_sha}, the container saw $got" >&2\n'
        f"  exit {EXIT_DELIVERY_FAILED}\n"
        "fi\n"
        f"{preflight}"
        f"{command}\n"
    )


def container_argv(view: SandboxConfigView, scratch: Path, script: str) -> list[str]:
    """The exact ``podman run`` invocation; every isolation flag is mandatory."""
    return [
        view.runtime,
        "run",
        "--rm",
        f"--network={view.network or 'none'}",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt",
        "no-new-privileges",
        f"--memory={int(view.memory_mb)}m",
        f"--pids-limit={int(view.pids_limit)}",
        f"--cpus={view.cpus}",
        # The root filesystem is read-only, so give pytest a writable /tmp that
        # is not part of the copied worktree.
        "--tmpfs=/tmp",
        "--env=HOME=/tmp",
        *view.extra_args,
        f"--volume={Path(scratch)}:{PROBE_ROOT}:rw",
        f"--workdir={PROBE_ROOT}",
        view.image,
        "/bin/sh",
        "-c",
        script,
    ]


# --------------------------------------------------------------------------- #
# Outcome classification
# --------------------------------------------------------------------------- #


def _truncate(text: str, limit: int = 20_000) -> str:
    if len(text) <= limit:
        return text
    return text[: limit - 40] + "\n...[truncated by the reviewer]..."


def _setup_marker(text: str) -> tuple[str, str]:
    low = text.lower()
    for needle, kind in _SETUP_MARKERS:
        if needle in low:
            if needle == "fixture" and "not found" not in low and "error" not in low:
                continue
            return needle, kind
    return "", ""


def _behavioural_marker(text: str) -> tuple[str, str]:
    low = text.lower()
    for needle, kind in _BEHAVIOURAL_MARKERS:
        if needle in low:
            return needle, kind
    return "", ""


def has_project_traceback(text: str) -> bool:
    """True when the output shows a traceback frame in project code."""
    for match in _TRACEBACK.finditer(text):
        path = match.group("file") or match.group("short") or ""
        if not path:
            continue
        lowered = path.lower()
        if "site-packages" in lowered or "_pytest" in lowered or lowered.startswith("/usr/local/lib/python"):
            continue
        return True
    return False


def assertion_summary(stdout: str, stderr: str, *, limit: int = 500) -> str:
    """The first pytest ``E   …`` line, else the first exception line."""
    for line in f"{stdout}\n{stderr}".splitlines():
        if _E_LINE.match(line.strip()):
            return line.strip()[:limit]
    for line in f"{stdout}\n{stderr}".splitlines():
        if "AssertionError" in line or "Error:" in line:
            stripped = line.strip()
            if stripped:
                return stripped[:limit]
    return ""


def classify_probe_run(
    *,
    exit_code: int | None,
    stdout: str = "",
    stderr: str = "",
    timed_out: bool = False,
    cancelled: bool = False,
    container_image: str = "",
    input_sha256: str = "",
    duration_ms: int = 0,
) -> ProbeResult:
    """Map one runner exit onto a :class:`ProbeStatus`, never guessing."""
    out = _truncate(stdout or "")
    err = _truncate(stderr or "")
    combined = f"{out}\n{err}"
    common = dict(
        exit_code=exit_code,
        stdout=out,
        stderr=err,
        assertion_message=assertion_summary(out, err),
        duration_ms=int(max(0, duration_ms)),
        container_image=container_image,
        input_sha256=input_sha256,
    )

    if cancelled:
        return ProbeResult(status=ProbeStatus.CANCELLED, failure_kind="cancelled", **common)
    if timed_out or exit_code in (124, 137) or (exit_code is not None and exit_code < 0):
        return ProbeResult(status=ProbeStatus.TIMEOUT, failure_kind="timeout", **common)
    if exit_code is None:
        return ProbeResult(status=ProbeStatus.SANDBOX_UNAVAILABLE, failure_kind="runtime", **common)
    if exit_code == 0:
        return ProbeResult(status=ProbeStatus.PASS, failure_kind="", **common)
    if exit_code == EXIT_DELIVERY_FAILED:
        return ProbeResult(status=ProbeStatus.INPUT_DELIVERY_FAILED, failure_kind="delivery", **common)
    if exit_code == EXIT_RUNNER_MISSING:
        return ProbeResult(status=ProbeStatus.SANDBOX_UNAVAILABLE, failure_kind="runner_missing", **common)
    if exit_code in (125, 126, 127):
        return ProbeResult(status=ProbeStatus.SANDBOX_UNAVAILABLE, failure_kind="runtime", **common)
    if exit_code == 2 or exit_code in (3, 4, 5):
        _, kind = _setup_marker(combined)
        return ProbeResult(status=ProbeStatus.SETUP_ERROR, failure_kind=kind or "collection", **common)

    marker, kind = _setup_marker(combined)
    if marker:
        return ProbeResult(status=ProbeStatus.SETUP_ERROR, failure_kind=kind, **common)
    bmarker, bkind = _behavioural_marker(combined)
    if bmarker or has_project_traceback(combined):
        return ProbeResult(status=ProbeStatus.FAIL_BEHAVIORAL, failure_kind=bkind or "exception", **common)
    # Exit 1 with nothing that names a behavioural failure is a setup problem:
    # a probe that could not run must never count against the pull request.
    return ProbeResult(status=ProbeStatus.SETUP_ERROR, failure_kind=f"exit_{exit_code}", **common)


def _elapsed_ms(started: float) -> int:
    return int(max(0.0, (time.monotonic() - started) * 1000))


def _unavailable(detail: str, *, image: str, started: float, exit_code: int | None = None) -> ProbeResult:
    return ProbeResult(
        status=ProbeStatus.SANDBOX_UNAVAILABLE,
        exit_code=exit_code,
        stderr=detail,
        failure_kind="runtime",
        duration_ms=_elapsed_ms(started),
        container_image=image,
    )


def _delivery_failure(detail: str, *, image: str, started: float, sha: str = "") -> ProbeResult:
    return ProbeResult(
        status=ProbeStatus.INPUT_DELIVERY_FAILED,
        stderr=detail,
        failure_kind="delivery",
        duration_ms=_elapsed_ms(started),
        container_image=image,
        input_sha256=sha,
    )


# --------------------------------------------------------------------------- #
# Scratch workspace
# --------------------------------------------------------------------------- #


def _prepare_scratch(view: SandboxConfigView, worktree: Path) -> Path:
    """Copy the worktree into a throw-away directory under the work root."""
    source = Path(worktree)
    if not source.is_dir():
        raise SandboxError(f"the worktree {source} does not exist")
    parent = view.scratch_parent()
    parent.mkdir(parents=True, exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix="probe-", dir=str(parent)))
    try:
        shutil.copytree(source, scratch, symlinks=True, ignore=_IGNORE, dirs_exist_ok=True)
    except OSError as exc:
        shutil.rmtree(scratch, ignore_errors=True)
        raise SandboxError(f"could not copy the worktree into a scratch directory: {exc}") from None
    return scratch


def _deliver(scratch: Path, rel_path: str, spec: ProbeSpec, expected_sha: str, image: str, started: float) -> tuple[ProbeResult | None, str]:
    """Write the probe file and verify the bytes that landed.  ``(failure, sha)``."""
    target = scratch / rel_path
    delivery = deliver_file(target, spec.content.encode("utf-8"))
    if not delivery.ok:
        return _delivery_failure(delivery.detail or "the probe file was not delivered intact", image=image, started=started, sha=delivery.actual_sha256), ""
    if expected_sha and delivery.actual_sha256 != expected_sha:
        return _delivery_failure(
            f"the probe file hashes to {delivery.actual_sha256[:12]} but the caller expected {expected_sha[:12]}",
            image=image,
            started=started,
            sha=delivery.actual_sha256,
        ), ""
    # Shipped beside the probe so an image without pytest can still execute the
    # same assertions.  It is copied only into the throw-away scratch tree.
    runner = Path(__file__).with_name("probe_runner.py")
    if runner.is_file():
        shutil.copyfile(runner, scratch / ".zl_probe_runner.py")
    return None, delivery.actual_sha256


# --------------------------------------------------------------------------- #
# Sandbox protocol
# --------------------------------------------------------------------------- #


@runtime_checkable
class Sandbox(Protocol):
    def available(self) -> tuple[bool, str]:
        """``(True, detail)`` when this runner can execute a probe right now."""
        ...

    def run_probe(
        self,
        worktree: Path,
        spec: ProbeSpec,
        *,
        expected_sha: str,
        timeout_s: float | None = None,
    ) -> ProbeResult:
        ...


class PodmanSandbox:
    """Runs probes in a locked-down container (podman or docker)."""

    def __init__(self, view: SandboxConfigView):
        self.view = view

    # -- capability -------------------------------------------------------
    def available(self) -> tuple[bool, str]:
        binary = shutil.which(self.view.runtime)
        if binary is None:
            return False, f"the {self.view.runtime} binary is not installed"
        cwd = self.view.scratch_parent()
        if not cwd.is_dir():
            cwd = Path.home()
        images = []
        candidates = [self.view.image, self.view.rust_image, self.view.node_image, self.view.go_image]
        candidates.extend(adapter.image for adapter in self.view.adapters.values() if adapter.image)
        for image in candidates:
            if image and image not in images:
                images.append(image)
        errors: list[str] = []
        for image in images:
            try:
                if self.view.runtime == "docker":
                    argv = [self.view.runtime, "image", "inspect", image]
                else:
                    argv = [self.view.runtime, "image", "exists", image]
                run = run_bounded(argv, cwd=cwd, timeout_s=60.0, guard_writes=False)
            except FileNotFoundError:
                return False, f"the {self.view.runtime} binary is not installed"
            except OSError as exc:
                return False, f"could not query {self.view.runtime}: {exc}"
            if run.exit_code == 0:
                return True, f"{self.view.runtime} {image}"
            errors.append(image)
        return False, (
            "none of the sandbox images are available locally: "
            + ", ".join(errors)
            + "; pull the image for the language you review"
        )

    # -- execution --------------------------------------------------------
    def _adapter_for(self, spec: ProbeSpec) -> ProbeAdapter | None:
        unique = {id(item): item for item in self.view.adapters.values()}
        by_path = adapter_for(spec.test_path, list(unique.values()))
        return by_path or self.view.adapters.get(spec.framework)

    def run_probe(
        self,
        worktree: Path,
        spec: ProbeSpec,
        *,
        expected_sha: str,
        timeout_s: float | None = None,
    ) -> ProbeResult:
        started = time.monotonic()
        image = self.view.image_for(spec.framework)
        adapter = self._adapter_for(spec)
        try:
            rel_path = probe_relative_path(
                spec.test_path, extensions=adapter.extensions if adapter is not None else None
            )
        except SandboxError as exc:
            return ProbeResult(
                status=ProbeStatus.MALFORMED_PROBE,
                stderr=str(exc),
                failure_kind="malformed_path",
                duration_ms=_elapsed_ms(started),
                container_image=image,
            )
        problem = self.view.visibility_problem()
        if problem:
            return _unavailable(problem, image=image, started=started)
        ok, detail = self.available()
        if not ok:
            return _unavailable(detail, image=image, started=started)
        scratch: Path | None = None
        verified_sha = expected_sha
        try:
            scratch = _prepare_scratch(self.view, Path(worktree))
            failure, delivered_sha = _deliver(scratch, rel_path, spec, expected_sha, image, started)
            if failure is not None:
                return failure
            verified_sha = delivered_sha
            script = probe_script(
                rel_path,
                delivered_sha,
                framework=spec.framework,
                image=image,
                adapter_command=adapter.command if adapter is not None and not adapter.builtin else None,
            )
            argv = container_argv(self.view, scratch, script)
            run = run_bounded(
                argv,
                cwd=self.view.scratch_parent(),
                timeout_s=float(timeout_s if timeout_s is not None else self.view.timeout_s),
            )
        except SandboxError as exc:
            return _unavailable(str(exc), image=image, started=started)
        except FileNotFoundError:
            return _unavailable(f"the {self.view.runtime} binary is not installed", image=image, started=started)
        except OSError as exc:
            return _unavailable(f"could not start {self.view.runtime}: {exc}", image=image, started=started)
        finally:
            if scratch is not None:
                shutil.rmtree(scratch, ignore_errors=True)
        return classify_probe_run(
            exit_code=run.exit_code,
            stdout=run.stdout,
            stderr=run.stderr,
            timed_out=run.timed_out or run.status is AttemptStatus.TIMEOUT,
            cancelled=run.status is AttemptStatus.CANCELLED,
            container_image=image,
            input_sha256=verified_sha,
            duration_ms=run.duration_ms or _elapsed_ms(started),
        )


class LocalSandbox:
    """Development runner.  **Not a security boundary.**

    The probe runs with the reviewer's own privileges, in the reviewer's own
    process tree, reading the same filesystem.  It exists so the pipeline still
    produces a *classified* degradation on a developer laptop with no container
    runtime; the operator has to opt in with ``sandbox.runtime = local`` or
    ``sandbox.local_fallback = true``.  Never enable it in CI.
    """

    def __init__(self, view: SandboxConfigView, *, python: str | None = None):
        self.view = view
        self.python = python or sys.executable

    def available(self) -> tuple[bool, str]:
        cwd = self.view.scratch_parent()
        if not cwd.is_dir():
            cwd = Path.home()
        for binary in (self.python, "cargo", "node", "npx"):
            if shutil.which(binary) is not None:
                return True, f"local {binary} (no isolation)"
        return False, "no Python, cargo, node or npx runner is available"

    def _adapter_for(self, spec: ProbeSpec) -> ProbeAdapter | None:
        unique = {id(item): item for item in self.view.adapters.values()}
        by_path = adapter_for(spec.test_path, list(unique.values()))
        return by_path or self.view.adapters.get(spec.framework)

    def run_probe(
        self,
        worktree: Path,
        spec: ProbeSpec,
        *,
        expected_sha: str,
        timeout_s: float | None = None,
    ) -> ProbeResult:
        started = time.monotonic()
        image = f"local:{self.python}"
        adapter = self._adapter_for(spec)
        try:
            rel_path = probe_relative_path(
                spec.test_path, extensions=adapter.extensions if adapter is not None else None
            )
        except SandboxError as exc:
            return ProbeResult(
                status=ProbeStatus.MALFORMED_PROBE,
                stderr=str(exc),
                failure_kind="malformed_path",
                duration_ms=_elapsed_ms(started),
                container_image=image,
            )
        scratch: Path | None = None
        verified_sha = expected_sha
        try:
            scratch = _prepare_scratch(self.view, Path(worktree))
            failure, delivered_sha = _deliver(scratch, rel_path, spec, expected_sha, image, started)
            if failure is not None:
                return failure
            verified_sha = delivered_sha
            argv = probe_argv(
                self.python,
                rel_path,
                framework=spec.framework,
                adapter_command=adapter.command if adapter is not None and not adapter.builtin else None,
            )
            run = run_bounded(
                argv,
                cwd=scratch,
                timeout_s=float(timeout_s if timeout_s is not None else self.view.timeout_s),
                env={"PYTHONPATH": str(scratch)},
            )
        except SandboxError as exc:
            return _unavailable(str(exc), image=image, started=started)
        except FileNotFoundError:
            return _unavailable(f"the interpreter {self.python} is not available", image=image, started=started)
        except OSError as exc:
            return _unavailable(f"could not start {self.python}: {exc}", image=image, started=started)
        finally:
            if scratch is not None:
                shutil.rmtree(scratch, ignore_errors=True)
        return classify_probe_run(
            exit_code=run.exit_code,
            stdout=run.stdout,
            stderr=run.stderr,
            timed_out=run.timed_out or run.status is AttemptStatus.TIMEOUT,
            cancelled=run.status is AttemptStatus.CANCELLED,
            container_image=image,
            input_sha256=verified_sha,
            duration_ms=run.duration_ms or _elapsed_ms(started),
        )


class DisabledSandbox:
    """``sandbox.runtime = none``: Stage 3 has no mechanical signal."""

    def available(self) -> tuple[bool, str]:
        return False, "sandbox.runtime is none; Stage 3 is disabled"

    def run_probe(
        self,
        worktree: Path,
        spec: ProbeSpec,
        *,
        expected_sha: str,
        timeout_s: float | None = None,
    ) -> ProbeResult:
        return ProbeResult(
            status=ProbeStatus.SANDBOX_UNAVAILABLE,
            stderr="sandbox.runtime is none; Stage 3 is disabled",
            failure_kind="runtime",
        )


def build_sandbox(settings: Settings) -> Sandbox:
    """Pick the runner the operator configured.

    ``local`` is only ever built on an explicit opt-in: either
    ``sandbox.runtime = local``, or ``sandbox.local_fallback = true`` **and** the
    container runtime genuinely reports unavailable.
    """
    view = SandboxConfigView.from_settings(settings)
    runtime = (view.runtime or "none").strip().lower()
    if runtime == "none":
        return DisabledSandbox()
    if runtime == "local":
        return LocalSandbox(view)
    container = PodmanSandbox(view)
    if view.local_fallback:
        ok, detail = container.available()
        if not ok:
            return LocalSandbox(view)
        del detail
    return container
