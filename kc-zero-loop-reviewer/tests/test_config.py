"""Configuration and git-plumbing regressions found while wiring the reviewer
into a real repository (karucode): a token *budget* is not a credential, and a
binary file in the tree must not crash a symbol scan.
"""

from __future__ import annotations

import pytest

from tests.support.gitfix import make_repo
from zero_loop_reviewer.config import SettingError, _holds_credential, load_settings
from zero_loop_reviewer.gitops import GitRepo


def test_a_token_budget_is_not_a_credential() -> None:
    assert _holds_credential("llm.api_key") is True
    assert _holds_credential("ci.token") is True
    assert _holds_credential("password") is True
    assert _holds_credential("provider.api_key") is True
    assert _holds_credential("llm.api_key_env") is False
    assert _holds_credential("review.max_model_tokens") is False
    assert _holds_credential("sast.timeout_tokens") is False
    assert _holds_credential("gate.require_checks") is False


def test_the_review_token_budget_can_be_configured() -> None:
    settings = load_settings({"review.max_model_tokens": 64000}, {})
    assert settings.max_model_tokens == 64000


def test_a_stored_credential_setting_is_refused() -> None:
    with pytest.raises(SettingError):
        load_settings({"llm.api_key": "sk-not-in-a-file"}, {})


def test_a_binary_blob_does_not_crash_a_symbol_scan(tmp_path) -> None:
    fixture = make_repo(tmp_path, "binary")
    binary = bytes(range(256))
    fixture.write_bytes("assets/logo.png", binary)
    sha = fixture.commit(
        "a source file and a binary",
        **{
            "src/app.tsx": "export function add(a: number, b: number) {\n  return a + b;\n}\n",
        },
    )
    repo = GitRepo(fixture.root)
    text = repo.show(sha, "assets/logo.png")
    assert isinstance(text, str)  # replacement characters, never a UnicodeDecodeError
    files = repo.list_files(sha)
    assert "assets/logo.png" in files


def test_reasoning_effort_defaults_high_for_review_and_low_for_triage() -> None:
    settings = load_settings({}, {})
    assert settings.llm.primary.reasoning_effort == "high"
    assert settings.llm.security.reasoning_effort == "high"
    assert settings.llm.reviewer.reasoning_effort == "high"
    assert settings.llm.monitor.reasoning_effort == "medium"
    assert settings.llm.triage.reasoning_effort == "low"
    assert settings.llm.probe.reasoning_effort == "medium"
    lowered = load_settings({"model.reviewer_reasoning_effort": "low"}, {})
    assert lowered.llm.reviewer.reasoning_effort == "low"


def test_context_and_output_defaults_are_500k_and_64k() -> None:
    settings = load_settings({}, {})
    assert settings.max_model_tokens == 500_000
    assert settings.llm.primary.max_tokens == 65_536
    assert settings.llm.reviewer.max_tokens == 65_536
    assert settings.llm.monitor.max_tokens == 65_536


def test_output_tokens_can_be_configured_without_a_host_setting() -> None:
    settings = load_settings({"model.max_tokens": 32_000}, {})
    assert settings.llm.security.max_tokens == 32_000


def test_decomposition_and_exhaustive_are_real_booleans() -> None:
    from zero_loop_reviewer.config import load_settings

    settings = load_settings({"review.decomposition": False, "review.exhaustive": "false"}, {})
    assert settings.decomposition is False
    assert settings.exhaustive is False


def test_per_language_probe_images_are_configurable() -> None:
    from zero_loop_reviewer.config import load_settings

    settings = load_settings(
        {
            "sandbox.rust_image": "rust:test",
            "sandbox.node_image": "node:test",
            "sandbox.go_image": "go:test",
        },
        {},
    )
    assert settings.sandbox.rust_image == "rust:test"
    assert settings.sandbox.node_image == "node:test"
    assert settings.sandbox.go_image == "go:test"
