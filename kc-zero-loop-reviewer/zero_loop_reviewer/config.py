"""Configuration: operator settings, environment, and an optional TOML file.

Precedence (highest first):

1. explicit environment override ``ZERO_LOOP_<KEY>`` (dots → underscores,
   upper-case), e.g. ``ZERO_LOOP_FUSION_C_FR=25``;
2. host settings (``KARUCODE_PLUGIN_SETTINGS`` or ``plugin/settings/get``);
3. a TOML file passed with ``--config`` (CI mode);
4. the defaults below.

Secrets are never read from settings: only the *name* of an environment
variable is configured, and values are read at call time.  A settings key that
looks like a credential is refused.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import tomllib
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Mapping

# --------------------------------------------------------------------------- #
# Declarations shown in Settings → Plugins & MCP (max 32, host limit)
# --------------------------------------------------------------------------- #

#: key, label, kind, default, help, min, max, options
SETTING_DECLARATIONS: list[dict[str, Any]] = [
    {
        "key": "model.primary",
        "label": "Primary model (all stages)",
        "type": "text",
        "default": "od/deepseek-v4.1-flash",
        "help": "DeepSeek-V4.1 Flash by default. Per-stage overrides live below.",
    },
    {
        "key": "model.triage",
        "label": "Stage 1 triage model",
        "type": "text",
        "default": "",
        "help": "Empty = primary.",
    },
    {
        "key": "model.security",
        "label": "Stage 2B secSemScan model",
        "type": "text",
        "default": "",
        "help": "Empty = primary.",
    },
    {
        "key": "model.probe",
        "label": "Stage 3 probe synthesis model",
        "type": "text",
        "default": "",
        "help": "Empty = primary.",
    },
    {
        "key": "model.reviewer",
        "label": "Stage 3/4/5 grounded reviewer model",
        "type": "text",
        "default": "",
        "help": "Empty = primary.",
    },
    {
        "key": "model.monitor",
        "label": "Stage 5 independent monitor model",
        "type": "text",
        "default": "",
        "help": "Empty = primary. Use a second provider's model when monitor independence is critical.",
    },
    {
        "key": "model.monitor_alt",
        "label": "Second-provider monitor model",
        "type": "text",
        "default": "",
        "help": "Optional: a model on another provider, used for monitor independence.",
    },
    {
        "key": "llm.base_url",
        "label": "OpenAI-compatible base URL",
        "type": "text",
        "default": "",
        "help": "e.g. http://127.0.0.1:4000/v1 (LiteLLM) or a hosted endpoint. Empty = read from the environment.",
    },
    {
        "key": "llm.api_key_env",
        "label": "API-key environment variable",
        "type": "text",
        "default": "ZERO_LOOP_LLM_API_KEY",
        "help": "Only the variable name is stored; the key itself never enters plugin settings.",
    },
    {
        "key": "llm.timeout_secs",
        "label": "Per-call timeout (s)",
        "type": "number",
        "default": 120,
        "help": "Hard deadline for one model call.",
        "min": 5,
        "max": 600,
    },
    {
        "key": "review.deadline_secs",
        "label": "Review deadline (s)",
        "type": "number",
        "default": 240,
        "help": "1–4 minute CI budget. Later stages degrade to human review rather than overrun silently.",
        "min": 30,
        "max": 1800,
    },
    {
        "key": "review.stage2_timeout_secs",
        "label": "Stage 2 track timeout (s)",
        "type": "number",
        "default": 60,
        "help": "Each parallel static/ensemble track is bounded by this.",
        "min": 10,
        "max": 300,
    },
    {
        "key": "review.max_model_tokens",
        "label": "Context token budget",
        "type": "number",
        "default": 128000,
        "help": "Hard ceiling for the compressed context slice.",
        "min": 4000,
        "max": 1048576,
    },
    {
        "key": "review.probe_attempts",
        "label": "Probe synthesis attempts",
        "type": "number",
        "default": 3,
        "help": "Reflexion iterations per candidate (1–3).",
        "min": 1,
        "max": 3,
    },
    {
        "key": "fusion.c_fr",
        "label": "Cost of a false rejection",
        "type": "number",
        "default": 10,
        "help": "C_FR in τ = C_FR / (C_FA + C_FR). Host-side only; never sent to a model.",
        "min": 1,
        "max": 1000,
    },
    {
        "key": "fusion.c_fa",
        "label": "Cost of a false acceptance",
        "type": "number",
        "default": 1,
        "help": "C_FA in τ = C_FR / (C_FA + C_FR). Host-side only; never sent to a model.",
        "min": 1,
        "max": 1000,
    },
    {
        "key": "fusion.monitor_votes",
        "label": "Monitor votes (k)",
        "type": "number",
        "default": 5,
        "help": "Independent monitor samples fused by median.",
        "min": 1,
        "max": 9,
    },
    {
        "key": "fusion.monitor_temperature",
        "label": "Monitor temperature",
        "type": "number",
        "default": 0.8,
        "help": "Deliberately different from the reviewer's deterministic setting.",
        "min": 0,
        "max": 2,
    },
    {
        "key": "fusion.disagreement_threshold",
        "label": "Reviewer/monitor disagreement band",
        "type": "number",
        "default": 0.55,
        "help": "A gap wider than this routes to human review even below τ.",
        "min": 0,
        "max": 1,
    },
    {
        "key": "sandbox.runtime",
        "label": "Sandbox runtime",
        "type": "select",
        "default": "podman",
        "help": "podman/docker enforce the isolation boundary; local is for environments without one and is not a security boundary; none disables Stage 3.",
        "options": [
            {"value": "podman", "label": "podman (recommended)"},
            {"value": "docker", "label": "docker"},
            {"value": "local", "label": "local (no isolation)"},
            {"value": "none", "label": "disabled"},
        ],
    },
    {
        "key": "sandbox.image",
        "label": "Sandbox image",
        "type": "text",
        "default": "docker.io/library/python:3.12-alpine",
        "help": "Must contain the project's runtime; probes are executed inside it.",
    },
    {
        "key": "sandbox.timeout_secs",
        "label": "Probe timeout (s)",
        "type": "number",
        "default": 120,
        "help": "One base-or-head execution.",
        "min": 10,
        "max": 900,
    },
    {
        "key": "sast.semgrep_config",
        "label": "Semgrep config",
        "type": "text",
        "default": "",
        "help": "Empty = the rules shipped with this plugin. A path, registry id or 'auto'.",
    },
    {
        "key": "gate.bot_login",
        "label": "Review bot identity",
        "type": "text",
        "default": "zero-loop-reviewer[bot]",
        "help": "Reviews are bound to this identity; only its own review is updated.",
    },
    {
        "key": "gate.require_checks",
        "label": "Required check runs (comma-separated)",
        "type": "text",
        "default": "",
        "help": "Names that must pass on the exact HEAD SHA.",
    },
    {
        "key": "gate.require_approval",
        "label": "Require a human approval at HEAD",
        "type": "bool",
        "default": False,
        "help": "Off for a review trigger (a fresh PR has no approval yet); on when this is a merge gate.",
    },
    {
        "key": "gate.cross_provider_audit",
        "label": "Require a disjoint reviewer key pool",
        "type": "bool",
        "default": True,
        "help": "Refuse to launch when the reviewer key is the authoring agent's key.",
    },
    {
        "key": "redaction.enabled",
        "label": "Redact exploitable security detail",
        "type": "bool",
        "default": True,
        "help": "Public PR comments get a gate status; the diagnostic and patch go to the private log.",
    },
    {
        "key": "ci.provider",
        "label": "Git provider",
        "type": "select",
        "default": "none",
        "help": "Used by `zero-loop-reviewer ci` / `webhook`.",
        "options": [
            {"value": "none", "label": "none"},
            {"value": "github", "label": "GitHub"},
            {"value": "gitlab", "label": "GitLab"},
            {"value": "bitbucket", "label": "Bitbucket"},
        ],
    },
    {
        "key": "ci.token_env",
        "label": "Git token environment variable",
        "type": "text",
        "default": "GITHUB_TOKEN",
        "help": "Only the variable name is stored.",
    },
    {
        "key": "state.dir",
        "label": "State directory",
        "type": "text",
        "default": "",
        "help": "Receipts, worktrees and the private security log. Empty = ~/.local/state/zero-loop-reviewer.",
    },
    {
        "key": "review.max_findings",
        "label": "Public findings cap",
        "type": "number",
        "default": 3,
        "help": "The zero-loop contract caps output at three. The host enforces 1–3.",
        "min": 1,
        "max": 3,
    },
]

SECRET_WORDS = frozenset({"key", "token", "secret", "password", "credential", "passphrase"})
#: Keys that name a variable rather than hold a credential.
SECRET_NAME_SUFFIXES = ("_env", "_env_name", ".env")


def _holds_credential(key: str) -> bool:
    """Whether a settings key would *store* a secret value.

    A bare substring test is wrong here: ``review.max_model_tokens`` and
    ``sast.timeout_tokens`` contain "token" but hold budgets, and refusing them
    breaks routine configuration.  A credential key ends in one of the secret
    words (``llm.api_key``, ``ci.token``) or *is* one (``password``); a key that
    only names an environment variable is passed by ``_merged`` first.
    """
    parts = [part for part in re.split(r"[._-]+", key.lower()) if part]
    if not parts:
        return False
    if parts[-1] in SECRET_WORDS:
        return True
    return len(parts) >= 2 and parts[-2:] == ["api", "key"]

DEFAULT_MODEL = "od/deepseek-v4.1-flash"


# --------------------------------------------------------------------------- #
# Typed slices
# --------------------------------------------------------------------------- #


@dataclass(frozen=True)
class StageModel:
    model: str
    temperature: float | None = None
    max_tokens: int = 4096
    votes: int = 1
    reasoning_effort: str | None = None


@dataclass(frozen=True)
class LLMConfig:
    base_url: str = ""
    api_key_env: str = "ZERO_LOOP_LLM_API_KEY"
    timeout_s: float = 120.0
    max_retries: int = 2
    response_format: bool = True
    extra_headers: Mapping[str, str] = field(default_factory=dict)
    primary: StageModel = StageModel(model=DEFAULT_MODEL)
    triage: StageModel = StageModel(model=DEFAULT_MODEL, temperature=0.0)
    security: StageModel = StageModel(model=DEFAULT_MODEL, temperature=0.0)
    probe: StageModel = StageModel(model=DEFAULT_MODEL, temperature=0.2)
    reviewer: StageModel = StageModel(model=DEFAULT_MODEL, temperature=0.0)
    monitor: StageModel = StageModel(model=DEFAULT_MODEL, temperature=0.8, votes=5)
    monitor_alt: StageModel | None = None


@dataclass(frozen=True)
class FusionConfig:
    c_fr: float = 10.0
    c_fa: float = 1.0
    reviewer_weight: float = 1.0
    monitor_weight: float = 0.75
    prior_log_odds: float = -0.85
    disagreement_threshold: float = 0.55
    clip: float = 12.0

    @property
    def tau(self) -> float:
        return self.c_fr / (self.c_fa + self.c_fr)


@dataclass(frozen=True)
class SandboxConfig:
    runtime: str = "podman"  # podman | docker | local | none
    image: str = "docker.io/library/python:3.12-alpine"
    helper_image: str = "docker.io/library/alpine:latest"
    timeout_s: float = 120.0
    memory_mb: int = 1024
    cpus: float = 1.0
    pids_limit: int = 256
    network: str = "none"
    extra_args: tuple[str, ...] = ("--uts=host",)
    local_fallback: bool = False


@dataclass(frozen=True)
class SastConfig:
    semgrep_config: str = ""
    semgrep_bin: str = "semgrep"
    timeout_s: float = 55.0
    enabled: bool = True
    max_findings: int = 200
    trusted_sanitizers: tuple[str, ...] = ("shlex.quote", "html.escape", "escape", "sanitize")
    suppressed_paths: tuple[str, ...] = ("tests/", "test/", "vendor/", "third_party/")


@dataclass(frozen=True)
class GateConfig:
    bot_login: str = "zero-loop-reviewer[bot]"
    required_checks: tuple[str, ...] = ()
    cross_provider_audit: bool = True
    authoring_pool_env: str = ""
    reviewer_pool_env: str = "ZERO_LOOP_LLM_API_KEY"
    allow_stacked_branches: bool = True
    #: A reviewer bot normally runs *before* a human approval exists; requiring
    #: one by default would refuse every fresh pull request.  Turn this on when
    #: the gate is used as a merge gate rather than a review trigger.
    require_approval: bool = False


@dataclass(frozen=True)
class RedactionConfig:
    enabled: bool = True
    private_log_name: str = "security-private.jsonl"
    exploitable_cwes: tuple[str, ...] = (
        "CWE-78",
        "CWE-79",
        "CWE-89",
        "CWE-94",
        "CWE-119",
        "CWE-120",
        "CWE-125",
        "CWE-200",
        "CWE-287",
        "CWE-306",
        "CWE-352",
        "CWE-416",
        "CWE-502",
        "CWE-611",
        "CWE-798",
        "CWE-862",
        "CWE-863",
        "CWE-918",
    )


@dataclass(frozen=True)
class PathConfig:
    state_dir: Path
    receipts_dir: Path
    work_dir: Path
    private_log: Path


@dataclass(frozen=True)
class CiConfig:
    provider: str = "none"
    token_env: str = "GITHUB_TOKEN"
    api_base: str = ""
    webhook_secret_env: str = "ZERO_LOOP_WEBHOOK_SECRET"
    webhook_port: int = 8791


@dataclass(frozen=True)
class Settings:
    llm: LLMConfig = field(default_factory=LLMConfig)
    fusion: FusionConfig = field(default_factory=FusionConfig)
    sandbox: SandboxConfig = field(default_factory=SandboxConfig)
    sast: SastConfig = field(default_factory=SastConfig)
    gate: GateConfig = field(default_factory=GateConfig)
    redaction: RedactionConfig = field(default_factory=RedactionConfig)
    ci: CiConfig = field(default_factory=CiConfig)
    paths: PathConfig = field(
        default_factory=lambda: default_paths(Path.home() / ".local/state/zero-loop-reviewer")
    )
    max_model_tokens: int = 128_000
    deadline_s: float = 240.0
    stage2_timeout_s: float = 60.0
    probe_attempts: int = 3
    max_findings: int = 3
    designlore_files: tuple[str, ...] = (
        "AGENTS.md",
        ".cursorrules",
        ".pr_agent.toml",
        ".zero_loop/designlore.json",
    )
    important_files: tuple[str, ...] = ("AGENTS.md", "README.md", "pyproject.toml")

    # ---- endpoint resolution ------------------------------------------------
    def llm_base_url(self, environ: Mapping[str, str] | None = None) -> str:
        env = environ if environ is not None else os.environ
        if self.llm.base_url:
            return self.llm.base_url.rstrip("/")
        for name in ("ZERO_LOOP_LLM_BASE_URL", "LITELLM_BASE_URL", "OPENAI_BASE_URL", "KARUCODE_BASE_URL"):
            value = env.get(name, "").strip()
            if value:
                return value.rstrip("/")
        return ""

    def api_key(self, environ: Mapping[str, str] | None = None) -> str:
        env = environ if environ is not None else os.environ
        value = env.get(self.llm.api_key_env, "").strip()
        if value:
            return value
        for name in ("ZERO_LOOP_LLM_API_KEY", "LITELLM_API_KEY", "OPENAI_API_KEY", "DEEPSEEK_API_KEY", "KARUCODE_API_KEY"):
            value = env.get(name, "").strip()
            if value:
                return value
        return ""

    def pool_fingerprint(self, env_name: str, environ: Mapping[str, str] | None = None) -> str:
        """SHA-256 of a key's *value*, for equality checks only.

        The digest is never logged and never written to a receipt; the
        pipeline only compares two digests.
        """
        env = environ if environ is not None else os.environ
        value = env.get(env_name, "")
        if not value:
            return ""
        return hashlib.sha256(value.encode("utf-8")).hexdigest()

    def with_paths(self, state_dir: Path) -> "Settings":
        return replace(self, paths=default_paths(state_dir))

    def for_workspace(self, workspace: Path) -> "Settings":
        if self.paths.state_dir != default_paths(Path.home() / ".local/state/zero-loop-reviewer").state_dir:
            return self
        return self.with_paths(Path.home() / ".local/state/zero-loop-reviewer")


def default_paths(state_dir: Path) -> PathConfig:
    state_dir = Path(state_dir)
    return PathConfig(
        state_dir=state_dir,
        receipts_dir=state_dir / "receipts",
        work_dir=state_dir / "work",
        private_log=state_dir / "security-private.jsonl",
    )


# --------------------------------------------------------------------------- #
# Loading
# --------------------------------------------------------------------------- #


class SettingError(ValueError):
    pass


def _env_key(key: str) -> str:
    return "ZERO_LOOP_" + key.replace(".", "_").replace("-", "_").upper()


def _coerce(key: str, raw: Any, declaration: dict[str, Any] | None) -> Any:
    kind = (declaration or {}).get("type", "text")
    if kind == "bool":
        if isinstance(raw, bool):
            return raw
        text = str(raw).strip().lower()
        if text in ("1", "true", "yes", "on"):
            return True
        if text in ("0", "false", "no", "off", ""):
            return False
        raise SettingError(f"{key}: {raw!r} is not a boolean")
    if kind == "number":
        try:
            value = float(raw)
        except (TypeError, ValueError):
            raise SettingError(f"{key}: {raw!r} is not a number") from None
        lo, hi = (declaration or {}).get("min"), (declaration or {}).get("max")
        if lo is not None and value < lo:
            raise SettingError(f"{key}: {value} is below the minimum {lo}")
        if hi is not None and value > hi:
            raise SettingError(f"{key}: {value} is above the maximum {hi}")
        return int(value) if float(value).is_integer() else value
    if kind == "select":
        value = str(raw)
        options = [o["value"] for o in (declaration or {}).get("options", [])]
        if options and value not in options:
            raise SettingError(f"{key}: {value!r} is not one of {options}")
        return value
    return str(raw)


def load_toml(path: Path | None) -> dict[str, Any]:
    if path is None:
        return {}
    if not path.exists():
        raise SettingError(f"config file not found: {path}")
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    flat: dict[str, Any] = {}
    for section, value in data.items():
        if isinstance(value, dict):
            for sub, inner in value.items():
                if isinstance(inner, dict):
                    for leaf, final in inner.items():
                        flat[f"{section}.{sub}.{leaf}"] = final
                else:
                    flat[f"{section}.{sub}"] = inner
        else:
            flat[section] = value
    return flat


def _merged(settings: Mapping[str, Any], environ: Mapping[str, str], file_values: Mapping[str, Any]) -> dict[str, Any]:
    declared = {d["key"]: d for d in SETTING_DECLARATIONS}
    out: dict[str, Any] = {}
    keys = set(declared) | set(file_values) | {
        "fusion.reviewer_weight",
        "fusion.monitor_weight",
        "fusion.prior_log_odds",
        "fusion.clip",
        "llm.max_retries",
        "llm.response_format",
        "sast.enabled",
        "sast.semgrep_bin",
        "sast.timeout_secs",
        "gate.authoring_pool_env",
        "gate.reviewer_pool_env",
        "sandbox.helper_image",
        "sandbox.memory_mb",
        "sandbox.cpus",
        "sandbox.pids_limit",
        "sandbox.local_fallback",
        "redaction.private_log_name",
        "ci.api_base",
        "ci.webhook_secret_env",
        "ci.webhook_port",
    }
    for key in keys:
        if key in file_values:
            out[key] = file_values[key]
        if key in settings:
            out[key] = settings[key]
        env_name = _env_key(key)
        if env_name in environ and environ[env_name] != "":
            out[key] = environ[env_name]
    for key in settings:
        # `llm.api_key_env` / `ci.token_env` name a variable; they are not the
        # credential. Only a key that would hold a secret value is refused.
        if key.endswith(SECRET_NAME_SUFFIXES):
            continue
        if _holds_credential(key):
            raise SettingError(
                f"setting {key!r} looks like a credential; configure the environment variable name instead"
            )
    return {k: _coerce(k, v, declared.get(k)) for k, v in out.items()}


def load_settings(
    settings: Mapping[str, Any] | None = None,
    environ: Mapping[str, str] | None = None,
    config_file: Path | None = None,
) -> Settings:
    settings = dict(settings or {})
    environ = dict(environ if environ is not None else os.environ)
    raw = _merged(settings, environ, load_toml(config_file))
    defaults = {d["key"]: d["default"] for d in SETTING_DECLARATIONS}

    def get(key: str, fallback: Any = None) -> Any:
        if key in raw:
            return raw[key]
        if key in defaults:
            return defaults[key]
        return fallback

    primary_model = str(get("model.primary") or DEFAULT_MODEL)
    primary = StageModel(model=primary_model, temperature=0.0)
    monitor_votes = int(get("fusion.monitor_votes", 5))
    monitor_temp = float(get("fusion.monitor_temperature", 0.8))

    llm = LLMConfig(
        base_url=str(get("llm.base_url", "") or ""),
        api_key_env=str(get("llm.api_key_env", "ZERO_LOOP_LLM_API_KEY") or "ZERO_LOOP_LLM_API_KEY"),
        timeout_s=float(get("llm.timeout_secs", 120)),
        max_retries=int(get("llm.max_retries", 2)),
        response_format=bool(get("llm.response_format", True)),
        primary=primary,
        triage=StageModel(model=str(get("model.triage") or primary_model), temperature=0.0),
        security=StageModel(model=str(get("model.security") or primary_model), temperature=0.0),
        probe=StageModel(model=str(get("model.probe") or primary_model), temperature=0.2),
        reviewer=StageModel(model=str(get("model.reviewer") or primary_model), temperature=0.0),
        monitor=StageModel(
            model=str(get("model.monitor") or primary_model),
            temperature=monitor_temp,
            votes=monitor_votes,
        ),
        monitor_alt=(
            StageModel(
                model=str(get("model.monitor_alt")),
                temperature=monitor_temp,
                votes=monitor_votes,
            )
            if get("model.monitor_alt")
            else None
        ),
    )

    fusion = FusionConfig(
        c_fr=float(get("fusion.c_fr", 10)),
        c_fa=float(get("fusion.c_fa", 1)),
        reviewer_weight=float(get("fusion.reviewer_weight", 1.0)),
        monitor_weight=float(get("fusion.monitor_weight", 0.75)),
        prior_log_odds=float(get("fusion.prior_log_odds", -0.85)),
        disagreement_threshold=float(get("fusion.disagreement_threshold", 0.55)),
        clip=float(get("fusion.clip", 12.0)),
    )
    if fusion.c_fr <= 0 or fusion.c_fa <= 0:
        raise SettingError("fusion costs must be positive")
    if not 0 < fusion.tau < 1:
        raise SettingError("fusion tau must lie strictly inside (0, 1)")

    sandbox = SandboxConfig(
        runtime=str(get("sandbox.runtime", "podman")),
        image=str(get("sandbox.image", "docker.io/library/python:3.12-alpine")),
        helper_image=str(get("sandbox.helper_image", "docker.io/library/alpine:latest")),
        timeout_s=float(get("sandbox.timeout_secs", 120)),
        memory_mb=int(get("sandbox.memory_mb", 1024)),
        cpus=float(get("sandbox.cpus", 1.0)),
        pids_limit=int(get("sandbox.pids_limit", 256)),
        local_fallback=bool(get("sandbox.local_fallback", False)),
    )

    sast = SastConfig(
        semgrep_config=str(get("sast.semgrep_config", "") or ""),
        semgrep_bin=str(get("sast.semgrep_bin", "semgrep") or "semgrep"),
        timeout_s=float(get("sast.timeout_secs", 55)),
        enabled=bool(get("sast.enabled", True)),
    )

    gate = GateConfig(
        bot_login=str(get("gate.bot_login", "zero-loop-reviewer[bot]")),
        required_checks=tuple(
            part.strip() for part in str(get("gate.require_checks", "") or "").split(",") if part.strip()
        ),
        cross_provider_audit=bool(get("gate.cross_provider_audit", True)),
        authoring_pool_env=str(get("gate.authoring_pool_env", "") or ""),
        reviewer_pool_env=str(get("gate.reviewer_pool_env", "ZERO_LOOP_LLM_API_KEY") or "ZERO_LOOP_LLM_API_KEY"),
        require_approval=bool(get("gate.require_approval", False)),
    )

    redaction = RedactionConfig(
        enabled=bool(get("redaction.enabled", True)),
        private_log_name=str(get("redaction.private_log_name", "security-private.jsonl")),
    )

    ci = CiConfig(
        provider=str(get("ci.provider", "none")),
        token_env=str(get("ci.token_env", "GITHUB_TOKEN")),
        api_base=str(get("ci.api_base", "") or ""),
        webhook_secret_env=str(get("ci.webhook_secret_env", "ZERO_LOOP_WEBHOOK_SECRET")),
        webhook_port=int(get("ci.webhook_port", 8791)),
    )

    state_dir = Path(str(get("state.dir") or "")).expanduser() if get("state.dir") else Path.home() / ".local/state/zero-loop-reviewer"
    paths = default_paths(state_dir)
    paths = replace(paths, private_log=state_dir / redaction.private_log_name)

    return Settings(
        llm=llm,
        fusion=fusion,
        sandbox=sandbox,
        sast=sast,
        gate=gate,
        redaction=redaction,
        ci=ci,
        paths=paths,
        max_model_tokens=int(get("review.max_model_tokens", 128_000)),
        deadline_s=float(get("review.deadline_secs", 240)),
        stage2_timeout_s=float(get("review.stage2_timeout_secs", 60)),
        probe_attempts=max(1, min(3, int(get("review.probe_attempts", 3)))),
        max_findings=max(1, min(3, int(get("review.max_findings", 3)))),
    )


def settings_from_plugin_env(environ: Mapping[str, str] | None = None) -> dict[str, Any]:
    """Read the host-provided settings blob without trusting its shape."""
    env = environ if environ is not None else os.environ
    raw = env.get("KARUCODE_PLUGIN_SETTINGS", "")
    if not raw:
        return {}
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    return value if isinstance(value, dict) else {}


def ensure_state_dirs(settings: Settings) -> None:
    for path in (
        settings.paths.state_dir,
        settings.paths.receipts_dir,
        settings.paths.work_dir,
    ):
        path.mkdir(parents=True, exist_ok=True)
