"""Stage 4C: DesignLore suppresses what the repo's own rules and precedents forbid."""

from __future__ import annotations

import json

import pytest

from tests.support.gitfix import make_repo
from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.models import (
    Candidate,
    Category,
    CitedSite,
    Evidence,
    Fix,
    StageId,
)
from zero_loop_reviewer.stage4.designlore import filter_candidate, load_directives, load_precedents

AGENTS = """# House rules

- never: use `eval` in `src/**`
- must: keep `logging` calls in `src/**`
- prefer: small functions in `src/**`

Everything else is a matter of taste.
"""

DESIGNLORE = {
    "precedents": [
        {
            "id": "hashing-integrity",
            "problem": "integrity hashing of stored blobs",
            "trigger": ["integrity", "hash"],
            "decision": "use `sha256` for integrity checks",
            "rationale": "never use `md5` for integrity checks",
            "applies_to": ["src/**"],
            "preconditions": ["src/store.py"],
        },
        {
            "id": "retry-backoff",
            "trigger": ["retry"],
            "decision": "use `backoff` for every retry",
            "rationale": "never use a bare loop for retries",
            "applies_to": ["src/**"],
            "preconditions": ["src/missing_client.py"],
        },
        {
            "id": "config-before-use",
            "trigger": ["config"],
            "decision": "load `config` once before use",
            "rationale": "never read the environment lazily",
            "applies_to": ["src/**"],
            "preconditions": ["store"],
        },
    ]
}

APP = "def main():\n    return 1\n"
STORE = "def store(data):\n    return data\n"


@pytest.fixture()
def checkout(tmp_path):
    fixture = make_repo(tmp_path / "checkout")
    fixture.commit(
        "house rules",
        **{
            "AGENTS.md": AGENTS,
            ".zero_loop/designlore.json": json.dumps(DESIGNLORE, indent=2),
            "src/app.py": APP,
            "src/store.py": STORE,
        },
    )
    return fixture


def _load(checkout):
    repo = GitRepo(checkout.root)
    head = checkout.head()
    settings = Settings()
    return load_precedents(repo, head, settings), load_directives(repo, head, settings)


def _candidate(title: str, root_cause: str, path: str, patch: str) -> Candidate:
    return Candidate(
        id="cand-designlore",
        stage=StageId.FALSIFY,
        category=Category.LOGIC,
        title=title,
        root_cause=root_cause,
        severity=2,
        cited_sites=[CitedSite(path=path, line=1)],
        evidence=Evidence(kind="analyzer", analyzer="stage2:rale", rule_id="logic"),
        verified_fix=Fix(patch=patch, files_changed=[path], applies_to_sha="0" * 40, verified=True),
    )


def _adding(path: str, line: int, context: str, added: str) -> str:
    return f"--- a/{path}\n+++ b/{path}\n@@ -{line},1 +{line},2 @@\n {context}\n+{added}\n"


def _removing(path: str, line: int, removed: str, context: str) -> str:
    return f"--- a/{path}\n+++ b/{path}\n@@ -{line},2 +{line},1 @@\n-{removed}\n {context}\n"


# --------------------------------------------------------------------------- #
# Loaders
# --------------------------------------------------------------------------- #


def test_load_directives_reads_kinds_and_path_globs(checkout):
    _precedents, directives = _load(checkout)

    by_kind = {directive.kind: directive for directive in directives}
    assert set(by_kind) == {"never", "must", "prefer"}
    assert by_kind["never"].source == "AGENTS.md"
    assert by_kind["never"].applies_to == ["src/**"]
    assert "eval" in by_kind["never"].text
    assert by_kind["must"].applies_to == ["src/**"]
    assert by_kind["must"].line == 4


def test_loaders_read_the_head_commit_not_the_working_tree(checkout):
    _precedents, before = _load(checkout)
    assert not any("pickle" in directive.text for directive in before)

    (checkout.root / "AGENTS.md").write_text(AGENTS + "- never: use `pickle` in `src/**`\n", encoding="utf-8")
    _precedents, uncommitted = _load(checkout)
    assert not any("pickle" in directive.text for directive in uncommitted)

    checkout.commit("add a rule", **{"AGENTS.md": AGENTS + "- never: use `pickle` in `src/**`\n"})
    _precedents, committed = _load(checkout)
    assert any("pickle" in directive.text for directive in committed)


def test_load_precedents_reads_ids_triggers_and_preconditions(checkout):
    precedents, _directives = _load(checkout)

    ids = [precedent.id for precedent in precedents]
    assert ids == ["hashing-integrity", "retry-backoff", "config-before-use"]
    hashing = precedents[0]
    assert hashing.source == ".zero_loop/designlore.json"
    assert hashing.trigger == ["integrity", "hash"]
    assert hashing.applies_to == ["src/**"]
    assert hashing.preconditions == ["src/store.py"]


def test_a_malformed_lore_file_yields_nothing_instead_of_raising(tmp_path):
    fixture = make_repo(tmp_path / "broken")
    fixture.write(".zero_loop/designlore.json", "{not json")
    fixture.commit("broken lore", **{"AGENTS.md": "- never: use `eval` in `src/**`\n"})
    repo = GitRepo(fixture.root)

    assert load_precedents(repo, fixture.head(), Settings()) == []
    assert len(load_directives(repo, fixture.head(), Settings())) == 1


# --------------------------------------------------------------------------- #
# Directives
# --------------------------------------------------------------------------- #


def test_a_never_directive_suppresses_a_suggestion_that_adds_the_construct(checkout):
    precedents, directives = _load(checkout)
    patch = _adding("src/app.py", 2, "    return 1", "    return eval(payload)")

    keep, reason = filter_candidate(
        _candidate("evaluate caller input", "return the evaluated payload", "src/app.py", patch),
        precedents,
        directives,
        repo_root=checkout.root,
    )

    assert keep is False
    assert "never" in reason
    assert "AGENTS.md:3" in reason
    assert "eval" in reason


def test_a_must_directive_suppresses_a_suggestion_that_removes_the_construct(checkout):
    precedents, directives = _load(checkout)
    patch = _removing("src/app.py", 2, '    logging.info("start")', "def main():")

    keep, reason = filter_candidate(
        _candidate("drop the log line", "the log line is noisy", "src/app.py", patch),
        precedents,
        directives,
        repo_root=checkout.root,
    )

    assert keep is False
    assert "must" in reason
    assert "logging" in reason


def test_a_never_directive_does_not_reach_a_file_outside_its_glob(checkout):
    precedents, directives = _load(checkout)
    patch = _adding("docs/notes.md", 2, "intro", "    return eval(payload)")

    keep, reason = filter_candidate(
        _candidate("evaluate caller input", "return the evaluated payload", "docs/notes.md", patch),
        precedents,
        directives,
        repo_root=checkout.root,
    )

    assert keep is True
    assert "no matched directive" in reason


# --------------------------------------------------------------------------- #
# Precedents
# --------------------------------------------------------------------------- #


def test_a_precedent_with_held_preconditions_suppresses_the_contradiction(checkout):
    precedents, directives = _load(checkout)
    patch = _adding("src/store.py", 2, "    return data", "    digest = hashlib.md5(data)")

    keep, reason = filter_candidate(
        _candidate("compute a content hash for integrity checking", "hash the stored blob", "src/store.py", patch),
        precedents,
        directives,
        repo_root=checkout.root,
    )

    assert keep is False
    assert "hashing-integrity" in reason
    assert "md5" in reason


def test_a_precedent_whose_precondition_path_is_missing_does_not_suppress(checkout):
    precedents, directives = _load(checkout)
    patch = _adding("src/store.py", 2, "    return data", "    time.sleep(0.1)")

    keep, reason = filter_candidate(
        _candidate("retry the upload immediately", "retry without a client", "src/store.py", patch),
        precedents,
        directives,
        repo_root=checkout.root,
    )

    assert keep is True
    assert "retry-backoff" in reason
    assert "preconditions do not hold" in reason
    assert "src/missing_client.py" in reason


def test_a_precedent_precondition_naming_a_symbol_is_checked_against_the_tree(checkout):
    precedents, directives = _load(checkout)
    patch = _removing("src/store.py", 2, "    config = load()", "def store(data):")

    keep, reason = filter_candidate(
        _candidate("load the config lazily", "config is looked up per call", "src/store.py", patch),
        precedents,
        directives,
        repo_root=checkout.root,
    )

    assert keep is False
    assert "config-before-use" in reason
    assert "config" in reason

    missing = [
        precedent.model_copy(update={"preconditions": ["no_such_symbol_anywhere"]})
        if precedent.id == "config-before-use"
        else precedent
        for precedent in precedents
    ]
    keep, reason = filter_candidate(
        _candidate("load the config lazily", "config is looked up per call", "src/store.py", patch),
        missing,
        directives,
        repo_root=checkout.root,
    )
    assert keep is True
    assert "no_such_symbol_anywhere" in reason


def test_a_candidate_no_lore_constrains_is_kept(checkout):
    precedents, directives = _load(checkout)
    patch = _adding("src/app.py", 2, "    return 1", "    return 2")

    keep, reason = filter_candidate(
        _candidate("return the sum instead", "the constant is wrong", "src/app.py", patch),
        precedents,
        directives,
        repo_root=checkout.root,
    )

    assert keep is True
    assert reason
