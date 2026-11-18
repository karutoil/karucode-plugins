"""Change-graph decomposition: units follow real cross-file relationships."""

from __future__ import annotations

from tests.support.gitfix import make_repo
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.pipeline import _behavioral_batches
from zero_loop_reviewer.stage1.changegraph import build_change_plan


def _plan(tmp_path):
    fixture = make_repo(tmp_path, "change_graph")
    base = fixture.commit("base", **{"pkg__a.py": "def a():\n    return 1\n"})
    head = fixture.commit(
        "head",
        **{
            "pkg__a.py": "from pkg.b import b\n\ndef a():\n    return b()\n",
            "pkg__b.py": "def b():\n    return 2\n",
            "web__app.ts": "import { state } from './state';\nexport const app = state;\n",
            "web__state.ts": "export const state = 1;\n",
            "docs__guide.md": "# guide\n",
            "unrelated.py": "def solo():\n    return 3\n",
        },
    )
    repo = GitRepo(fixture.root)
    return build_change_plan(repo, repo.diff(base, head)), repo, repo.diff(base, head)


def test_imports_pair_files_into_the_same_change_unit(tmp_path) -> None:
    plan, _, _ = _plan(tmp_path)
    units = {path: plan.unit_id_for(path) for path in ["pkg/a.py", "pkg/b.py", "web/app.ts", "web/state.ts"]}
    assert units["pkg/a.py"] == units["pkg/b.py"]
    assert units["web/app.ts"] == units["web/state.ts"]
    assert units["pkg/a.py"] != units["web/app.ts"]
    assert plan.unit_id_for("docs/guide.md")
    assert plan.unit_id_for("unrelated.py")


def test_batches_do_not_mix_change_units(tmp_path) -> None:
    plan, _, diff = _plan(tmp_path)
    batches, _, _ = _behavioral_batches(diff, max_batches=100, plan=plan)
    for batch in batches:
        assert len({plan.unit_id_for(patch.path) for patch in batch}) <= 1


def test_a_large_plan_still_covers_every_changed_line(tmp_path) -> None:
    plan, _, diff = _plan(tmp_path)
    batches, _, _ = _behavioral_batches(diff, max_batches=100, plan=plan)
    covered = {(patch.path, line) for batch in batches for patch in batch for line in patch.changed_lines}
    required = {
        (patch.path, line)
        for patch in diff.files
        if not patch.is_deleted and patch.path.endswith((".py", ".ts"))
        for line in patch.changed_lines
    }
    assert required <= covered
