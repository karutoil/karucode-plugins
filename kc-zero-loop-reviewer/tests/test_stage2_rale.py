"""Stage 2C: RALE's AST view, CFG view, code view, ensemble and classifiers."""

from __future__ import annotations

import importlib.util
import itertools

import pytest

from zero_loop_reviewer.config import Settings
from zero_loop_reviewer.errors import ToolError
from zero_loop_reviewer.models import RaleResult
from zero_loop_reviewer.stage2.classifiers import (
    CODEBERT_MODEL,
    HashingCodeClassifier,
    LocalCodeClassifier,
    TransformersCodeClassifier,
    default_classifier,
)
from zero_loop_reviewer.stage2.rale import (
    CODE_FLAG_THRESHOLD,
    ast_view,
    cfg_view,
    code_view,
    ensemble,
    rale,
)

BENIGN = "def add(a, b):\n    return a + b\n"
BARE_EXCEPT = "def f(v):\n    try:\n        return v\n    except:\n        return None\n"
EXCEPT_PASS = "def f(v):\n    try:\n        return v\n    except ValueError:\n        pass\n    return v\n"
MUTABLE_DEFAULT = "def f(items=[]):\n    items.append(1)\n    return items\n"
UNREACHABLE = "def f():\n    return 1\n    raise SystemExit(2)\n"
ASSERT_TUPLE = "def f(a, b):\n    assert (a, b)\n    return a\n"
MISSING_RETURN = "def f(x):\n    if x:\n        return 1\n"
LOOP_FOREVER = "def f():\n    while True:\n        pass\n"
LOOP_BREAK = "def f(x):\n    while True:\n        if x:\n            break\n    return 1\n"
LEAKY_EXCEPT = "def f(path):\n    try:\n        fh = open(path)\n        return fh.read()\n    except OSError:\n        return None\n"
CLEAN_EXCEPT = (
    "def f(path):\n"
    "    try:\n"
    "        fh = open(path)\n"
    "        return fh.read()\n"
    "    except OSError:\n"
    "        return None\n"
    "    finally:\n"
    "        fh.close()\n"
)


# --------------------------------------------------------------------------- #
# AST view
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    ("source", "needle"),
    [
        (BARE_EXCEPT, "bare_except"),
        (EXCEPT_PASS, "except_pass"),
        (MUTABLE_DEFAULT, "mutable_default"),
        (UNREACHABLE, "unreachable_code"),
        (ASSERT_TUPLE, "assert_on_tuple"),
        (MISSING_RETURN, "missing_return_branch"),
    ],
)
def test_ast_view_flags_structural_defects(source, needle):
    flag, reasons, _functions = ast_view(source, "mod.py")
    assert flag is True
    assert any(needle in reason for reason in reasons), reasons
    assert all("mod.py:" in reason for reason in reasons)


def test_ast_view_accepts_benign_code_and_lists_functions():
    flag, reasons, functions = ast_view(BENIGN, "mod.py")
    assert flag is False and reasons == []
    assert functions == ["add"]


def test_ast_view_returns_no_flags_for_non_python_and_broken_python():
    assert ast_view("function f() { return 1; }", "mod.js") == (False, [], [])
    assert ast_view("def f(:\n", "mod.py") == (False, [], [])


# --------------------------------------------------------------------------- #
# CFG view
# --------------------------------------------------------------------------- #


def test_cfg_view_flags_a_missing_return_on_one_path():
    flag, reasons = cfg_view(MISSING_RETURN, "mod.py")
    assert flag is True
    assert any("missing_return_on_path" in reason for reason in reasons), reasons
    assert cfg_view(BENIGN, "mod.py") == (False, [])
    assert cfg_view("def f(x):\n    if x:\n        return 1\n    return 2\n", "mod.py") == (False, [])


def test_cfg_view_flags_a_loop_with_no_exit_and_clears_it_with_break():
    flag, reasons = cfg_view(LOOP_FOREVER, "mod.py")
    assert flag is True
    assert any("loop_without_exit" in reason for reason in reasons), reasons
    assert cfg_view(LOOP_BREAK, "mod.py") == (False, [])


def test_cfg_view_flags_an_exception_path_that_skips_cleanup():
    flag, reasons = cfg_view(LEAKY_EXCEPT, "mod.py")
    assert flag is True
    assert any("exception_path_skips_cleanup" in reason for reason in reasons), reasons
    assert cfg_view(CLEAN_EXCEPT, "mod.py") == (False, [])


def test_cfg_view_ignores_non_python_and_syntax_errors():
    assert cfg_view("int f() { return 1; }", "mod.c") == (False, [])
    assert cfg_view("def f(:\n", "mod.py") == (False, [])


# --------------------------------------------------------------------------- #
# Code view
# --------------------------------------------------------------------------- #


def test_code_view_without_a_classifier_reports_unavailable():
    assert code_view(BENIGN, "mod.py", classifier=None) == (False, 0.0, "unavailable")


def test_code_view_uses_a_deterministic_baseline():
    classifier = HashingCodeClassifier()
    flag, score, detail = code_view("eval(user_input)\n", "mod.py", classifier=classifier)
    assert flag is True
    assert score >= CODE_FLAG_THRESHOLD
    assert detail.startswith("hashing-linear")
    benign_flag, benign_score, _ = code_view(BENIGN, "mod.py", classifier=classifier)
    assert benign_flag is False and benign_score == 0.0
    assert classifier.score("eval(x)\n", "a.py") == classifier.score("eval(x)\n", "b.py")


def test_a_classifier_that_raises_is_reported_unavailable():
    class Broken:
        name = "broken"

        def score(self, source: str, path: str) -> float:
            raise ToolError("model weights are missing")

    assert code_view(BENIGN, "mod.py", classifier=Broken()) == (False, 0.0, "unavailable: model weights are missing")


# --------------------------------------------------------------------------- #
# Ensemble
# --------------------------------------------------------------------------- #


def test_ensemble_is_the_required_boolean_absorption():
    for ast_flag, cfg_flag, code_flag in itertools.product((False, True), repeat=3):
        assert ensemble(ast_flag, cfg_flag, code_flag) == ((ast_flag and cfg_flag) or code_flag)


def test_rale_combines_the_views_and_ignores_non_python():
    missing = rale("mod.py", MISSING_RETURN)
    assert isinstance(missing, RaleResult)
    assert missing.ast_flag and missing.cfg_flag
    assert missing.flagged is True
    assert missing.function == "f"
    assert missing.classifier == ""

    code_only = rale("mod.py", BENIGN, classifier=HashingCodeClassifier())
    assert code_only.ast_flag is False and code_only.cfg_flag is False
    assert code_only.code_flag is False and code_only.flagged is False

    suspicious = rale("mod.py", "def f(v):\n    return eval(v)\n", classifier=HashingCodeClassifier())
    assert suspicious.code_flag is True
    assert suspicious.flagged is True
    assert suspicious.code_score == pytest.approx(0.85, abs=1e-6)

    class Loud:
        name = "loud"

        def score(self, source: str, path: str) -> float:
            raise AssertionError("a non-Python file must not reach the classifier")

    non_python = rale("mod.js", "function f() { return eval(x); }", classifier=Loud())
    assert non_python.flagged is False
    assert non_python.ast_flag is False and non_python.cfg_flag is False and non_python.code_flag is False


# --------------------------------------------------------------------------- #
# Classifiers
# --------------------------------------------------------------------------- #


def test_hashing_classifier_is_deterministic_and_bounded():
    first = HashingCodeClassifier()
    second = HashingCodeClassifier()
    for source in (BENIGN, BARE_EXCEPT, "shell=True\n", "def f():\n    while True:\n        pass\n"):
        score = first.score(source, "mod.py")
        assert 0.0 <= score <= 1.0
        assert score == second.score(source, "mod.py")
    assert first.name == "hashing-linear"
    assert isinstance(first, LocalCodeClassifier)


def test_transformers_classifier_never_fabricates_a_score():
    classifier = TransformersCodeClassifier(CODEBERT_MODEL)
    available, detail = classifier.available()
    if available:
        score = classifier.score(BENIGN, "mod.py")
        assert 0.0 <= score <= 1.0
    else:
        assert "unavailable" in detail
        with pytest.raises(ToolError):
            classifier.score(BENIGN, "mod.py")


def test_default_classifier_reports_unavailable_as_none():
    classifier = default_classifier(Settings())
    if importlib.util.find_spec("torch") is None:
        assert classifier is None
    elif classifier is not None:
        assert isinstance(classifier, LocalCodeClassifier)
