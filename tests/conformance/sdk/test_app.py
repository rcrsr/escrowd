"""Phase 1.7: the seven exit tests and check 8, driven through `examples/test-app`.

Each check seeds a project, runs one app check (its own process, re-executed under
`escrow run` by `escrow.init`), and checks the project, the outcomes the app printed and
the ledger. After every run the suite checks attribution mechanically: each ledger entry
must name a scope the app opened, and each path in it must sit under a prefix that
scope's IO used (the app tags every scope's paths and content). One misattributed entry
fails the check.
"""

import importlib.util
import os
import shutil
import subprocess
from pathlib import Path

import pytest

import escrow
from escrow_testkit import fingerprint, scenario, tree, write_policy
from escrow_testkit.apps import APPS, PYTHON_APP, App, misattributed

ops = scenario.load("app")


def seed(project: Path, d: str) -> None:
    scenario.apply(ops["seed"], project, d)


def expected(project: Path, work: Path, d: str) -> list:
    """The project's tree after the scenario's `mutate` ops run natively on a copy."""
    copy = work / "expected"
    shutil.copytree(project, copy, symlinks=True)
    scenario.apply(ops["mutate"], copy, d)
    return tree(copy)


@pytest.fixture(params=APPS)
def app(request, escrow_bin, runtime_dir):
    a = App(escrow_bin, runtime_dir, APPS[request.param])
    yield a
    for m in (runtime_dir / "run" / "escrowd").glob("*/view"):
        subprocess.run(["fusermount3", "-u", "-z", m], capture_output=True)


def test_misattribution_check_flags_foreign_entries():
    scopes = {"s1": ["a"], "s2": ["b/f.txt"]}
    ok = "1 scope=s1 op=create path=a/x decision=allow"
    assert misattributed([ok, "1 scope=s2 op=close path= decision=allow"], scopes) == []
    foreign = [
        "1 scope=s1 op=create path=b/f.txt decision=allow",
        "1 scope=s2 op=rename path=b/f.txt from=a/x decision=allow",
        "1 scope=s3 op=open path= decision=allow",
        "1 scope=s1 op=create path=ab decision=allow",
    ]
    assert misattributed([ok, *foreign], scopes) == foreign


def test_unknown_op_is_rejected_by_both_interpreters(tmp_path):
    bad = [{"op": "teleport", "path": "x"}]
    with pytest.raises(ValueError, match="teleport"):
        scenario.apply(bad, tmp_path, "d")
    spec = importlib.util.spec_from_file_location("test_app_module", PYTHON_APP)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.P = tmp_path
    with pytest.raises(ValueError, match="teleport"):
        module.apply(bad, "d")


def test_1_project_is_unchanged_until_commit(app):
    seed(app.project, "w")
    base = sorted(os.listdir(app.project / "w"))
    want = expected(app.project, app.work, "w")
    out = app.run("escrowed")
    assert out["own_read"] == "w:w/edit.txt\n"
    assert out["other_scope_sees"] == out["project_at_decide"] == base
    assert out["writes"]["status"] == "committed"
    assert out["writes"]["paths"] == [
        "w/edit.txt",
        "w/gone.txt",
        "w/link",
        "w/mode.txt",
        "w/new.txt",
        "w/renamed.txt",
        "w/sub",
        "w/sub/deep",
        "w/sub/deep/f.txt",
    ]
    assert tree(app.project) == want


def test_2_subprocess_writes_land_in_their_scope(app):
    (app.project / "c").mkdir()
    out = app.run("child")
    assert out["read_back"] == "c:c/f.txt\n"
    assert out["child"]["paths"] == ["c/d", "c/d/g.txt", "c/f.txt"]
    (sid,) = (s for s in out["scopes"] if s != "unscoped")
    assert app.entries(sid, "create") == ["c/d/g.txt", "c/f.txt"]
    assert (app.project / "c" / "d" / "g.txt").read_text() == "c:c/d/g.txt\n"


def test_3_concurrent_async_scopes_on_one_thread(app):
    for d in ("a", "b"):
        (app.project / d).mkdir()
    out = app.run("concurrent")
    assert out["threads"] == 1
    assert (out["a_reads"], out["b_reads"]) == ("a:a/a-sub.txt\n", "b:b/b-sub.txt\n")
    ids = {prefixes[0]: sid for sid, prefixes in out["scopes"].items() if prefixes}
    for name in ("a", "b"):
        files = [f"{name}/{name}-sub.txt", f"{name}/{name}.txt"]
        assert out[name]["paths"] == files
        assert app.entries(ids[name], "create") == files
        for f in files:
            assert (app.project / f).read_text() == f"{name}:{f}\n"


def test_4_discard_leaves_the_project_byte_identical(app):
    seed(app.project, "d")
    before = fingerprint(app.project)
    out = app.run("discarded")
    assert out["discard"]["status"] == "discarded"
    assert out["discard"]["reasons"] == ["not wanted"]
    assert fingerprint(app.project) == before


@pytest.mark.parametrize("fault", [None, "journal:0", "preimage:2", "apply:1", "apply:6", "done:0"])
def test_4_commit_applies_every_change_or_none(app, fault):
    seed(app.project, "d")
    before = fingerprint(app.project)
    want = expected(app.project, app.work, "d")
    out = app.run("atomic", env={"ESCROWD_FAULT": fault} if fault else None)
    if fault is None:
        assert out["atomic"]["status"] == "committed"
        assert tree(app.project) == want
    else:
        assert out["atomic"]["status"] == "error"
        assert fingerprint(app.project) == before


def test_5_second_writer_hits_the_conflict_policy(app):
    (app.project / "x").mkdir()
    out = app.run("conflict")
    assert out["first"]["status"] == "committed"
    assert out["second"]["status"] == "conflict"
    assert out["second"]["reasons"] == [
        "conflict: x/shared.txt changed in the project since the scope read it"
    ]
    assert (app.project / "x" / "shared.txt").read_text() == "first:x/shared.txt\n"


def test_6_denied_read_fails_and_is_reported(app):
    (app.project / ".env").write_text("SECRET=1\n")
    out = app.run("denied-read")
    assert out["errno"] == 13
    assert out["leaky"]["reads"] == [[".env", False]]
    (sid,) = (s for s in out["scopes"] if s != "unscoped")
    assert any(f" scope={sid} op=read path=.env decision=deny" in x for x in app.ledger)


@pytest.mark.parametrize(
    "mode, write, lands",
    [("passthrough", "ok", True), ("implicit", "ok", False), ("deny", 30, False)],
)
def test_7_unscoped_modes(app, mode, write, lands):
    (app.project / "u").mkdir()
    (app.project / "u" / "base.txt").write_text("base\n")
    out = app.run("unscoped", mode=mode)
    assert out["base_read"] == "base\n"
    assert out["write"] == write
    if write == "ok":
        assert out["read_back"] == "unscoped:u/out.txt\n"
    assert (app.project / "u" / "out.txt").exists() == lands
    if mode == "implicit":  # discarded at exit (on_exit="discard")
        assert app.entries("unscoped", "create") == ["u/out.txt"]


def test_8_snapshot_at_open(app):
    (app.project / "s").mkdir()
    (app.project / "s" / "f.txt").write_text("base\n")
    out = app.run("snapshot")
    assert out["new"]["status"] == "committed"
    assert out["before"] == out["after"] == "base\n"
    assert out["listing"] == ["f.txt"]
    assert out["old"] == {"status": "committed", "paths": [], "reasons": []}
    assert (app.project / "s" / "f.txt").read_text() == "new:s/f.txt\n"


REVIEW = """review:
  - {paths: ['h/auth/**'], tier: human}
  - {paths: ['h/**'], tier: llm}
  - {paths: ['n/**'], tier: llm, wait: optional}
"""


def scripted(plan: dict[str, list[tuple]], seen: dict):
    """A reviewer that gives each held scope, by name, its planned verdicts in order
    (tier, verdict, reasons, override), oldest hold first; `seen[name]` keeps what
    GetHeld showed before the first."""

    def review(rv: escrow.Reviewer) -> None:
        for h in rv.list_held():
            steps = plan.get(h.name)
            if not steps:
                continue
            seen.setdefault(h.name, rv.get_held(h.scope_id))
            tier, verdict, reasons, override = steps.pop(0)
            rv.review(h.scope_id, tier, verdict, reasons=reasons, override=override)

    return review


def test_held_1_1_to_1_3_wait_independence_monotonic(app):
    """Exit criteria 1.1, 1.2 and 1.3 through the SDK."""
    app.policy = write_policy(app.work, deny_read=(".env",), extra=REVIEW)
    seen: dict = {}
    plan = {
        "guarded": [
            ("llm", "discard", ["checks nothing"], False),
            ("human", "commit", ["a fixture"], True),
        ]
    }
    out = app.run("held", reviewer=scripted(plan, seen))
    assert out["held"]["status"] == "held"
    assert (out["held"]["tiers"], out["held"]["wait"]) == (["llm", "human"], True)
    assert out["self_commit"] == "PERMISSION_DENIED"  # 1.2
    assert out["next_reads"] == "guarded:h/auth/login.py\n"  # 1.1: opened after the verdict
    assert out["guarded"] == {
        "status": "committed",
        "paths": ["h", "h/auth", "h/auth/login.py"],
        "reasons": ["llm: checks nothing", "human: a fixture"],
    }
    assert out["next"]["status"] == "committed"
    guarded = seen["guarded"].held.scope_id
    ops = [x.split()[2] for x in app.ledger if f" scope={guarded} " in x]
    assert ops[-4:] == ["op=review-llm", "op=override", "op=review-human", "op=decide"]  # 1.3


def test_held_1_4_and_1_5_continue_conflicts_history(app):
    """Exit criteria 1.4 and 1.5 through the SDK."""
    app.policy = write_policy(app.work, deny_read=(".env",), extra=REVIEW)
    (app.project / "n").mkdir()
    (app.project / "n" / "guide.md").write_text("base\n")
    seen: dict = {}
    commit = ("llm", "commit", [], False)
    out = app.run("held-continue", reviewer=scripted({"turn1": [commit], "turn2": [commit]}, seen))
    assert out["held"] == ["held", "held"]
    assert out["turn2_sees"] == "base\n"  # its snapshot lacks turn 1
    assert out["turn1"]["status"] == "committed"
    assert (out["turn2"]["status"], out["turn2"]["paths"]) == ("conflict", ["n/guide.md"])  # 1.4
    assert (app.project / "n" / "guide.md").read_text() == "turn1:n/guide.md\n"
    # 1.5: turn 2's reviewer saw turn 1's change set and verdict.
    (h,) = seen["turn2"].history
    assert h.name == "turn1" and h.outcome.status == "committed"
    assert "+turn1:n/guide.md" in h.outcome.diff
