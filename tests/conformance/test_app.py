"""Phase 1.7: the seven exit tests and check 8, driven through `examples/test-app`.

Each check seeds a project, runs one app check (its own process, re-executed under
`escrow run` by `escrow.init`), and checks the project, the outcomes the app printed and
the ledger. After every run the suite checks attribution mechanically: each ledger entry
must name a scope the app opened, and each path in it must sit under a prefix that
scope's IO used (the app tags every scope's paths and content). One misattributed entry
fails the check.
"""

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

import grpc
import pytest
from conftest import ROOT, write_policy
from scenario import fingerprint, tree

import escrow
from escrow.v1 import escrow_pb2 as pb

APP = ROOT / "examples" / "test-app" / "app.py"
sys.path.insert(0, str(APP.parent))
import app as test_app  # noqa: E402  (replays `mutate` natively for the expected tree)

BASE = {"edit.txt": "edit\n", "old.txt": "old\n", "gone.txt": "gone\n", "mode.txt": "mode\n"}


def seed(project: Path, d: str) -> None:
    (project / d).mkdir(parents=True, exist_ok=True)
    for name, text in BASE.items():
        (project / d / name).write_text(text)
    os.chmod(project / d / "mode.txt", 0o644)
    os.symlink("old.txt", project / d / "link")


def expected(project: Path, work: Path, d: str) -> list:
    """The project's tree after `mutate(d)` runs natively on a copy."""
    copy = work / "expected"
    shutil.copytree(project, copy, symlinks=True)
    test_app.P = copy
    test_app.mutate(d)
    return tree(copy)


def misattributed(ledger: list[str], scopes: dict[str, list[str]]) -> list[str]:
    """Ledger entries naming a scope the app never opened, or a path outside the
    prefixes that scope's IO used."""

    def owned(path: str, prefixes: list[str]) -> bool:
        return any(path == p or path.startswith(p + "/") for p in prefixes)

    bad = []
    for line in ledger:
        fields = dict(f.split("=", 1) for f in line.split()[1:])
        if "scope" not in fields and "proc" in fields:
            continue  # a process, named by the scope lines that follow
        prefixes = scopes.get(fields["scope"])
        paths = [p for p in (fields.get("path"), fields.get("from")) if p]
        if prefixes is None or not all(owned(p, prefixes) for p in paths):
            bad.append(line)
    return bad


class App:
    def __init__(self, escrow_bin: Path, work: Path):
        self.bin, self.work = escrow_bin, work
        self.project = work / "proj"
        self.project.mkdir()
        (work / "run").mkdir()
        self.policy = write_policy(work, deny_read=(".env",))

    def run(self, check: str, mode: str = "deny", env: dict | None = None, reviewer=None) -> dict:
        """`reviewer(rv)`, if given, is called on the review socket until the app exits."""
        clean = {k: v for k, v in os.environ.items() if not k.startswith("ESCROW")}
        clean |= {
            "ESCROW_EXE": str(self.bin),
            "XDG_STATE_HOME": str(self.work / "state-home"),
            "XDG_RUNTIME_DIR": str(self.work / "run"),
        }
        argv = [sys.executable, APP, self.project, mode, self.policy, check]
        env = clean | (env or {})
        if reviewer is None:
            r = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=60)
            stdout, stderr, code = r.stdout, r.stderr, r.returncode
        else:
            stdout, stderr, code = self._reviewed(argv, env, reviewer)
        assert code == 0, stderr
        out = json.loads(stdout.strip().splitlines()[-1])
        assert misattributed(self.ledger, out["scopes"]) == [], "misattributed ledger entries"
        return out

    def _reviewed(self, argv, env, reviewer):
        p = subprocess.Popen(
            argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        deadline = time.monotonic() + 60
        rv = None
        try:
            while p.poll() is None and time.monotonic() < deadline:
                if rv is None:
                    socks = list((self.work / "run" / "escrowd").glob("*/escrow.sock.review"))
                    if socks:
                        rv = escrow.Reviewer(str(socks[0]))
                if rv is not None:
                    try:
                        reviewer(rv)
                    except grpc.RpcError as e:
                        if e.code() != grpc.StatusCode.UNAVAILABLE:  # the app exited
                            raise
                time.sleep(0.05)
        finally:
            if rv is not None:
                rv.close()
            if p.poll() is None:
                p.kill()
        stdout, stderr = p.communicate()
        return stdout, stderr, p.returncode

    @property
    def ledger(self) -> list[str]:
        (log,) = (self.work / "state-home" / "escrowd").glob("*/ledger.log")
        return log.read_text().splitlines()

    def entries(self, scope_id: str, op: str) -> list[str]:
        return sorted(
            line.split(" path=")[1].split()[0]
            for line in self.ledger
            if f" scope={scope_id} op={op} " in line
        )


@pytest.fixture
def app(escrow_bin, runtime_dir):
    a = App(escrow_bin, runtime_dir)
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
            (pb.TIER_LLM, pb.VERDICT_DISCARD, ["checks nothing"], False),
            (pb.TIER_HUMAN, pb.VERDICT_COMMIT, ["a fixture"], True),
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
    commit = (pb.TIER_LLM, pb.VERDICT_COMMIT, [], False)
    out = app.run("held-continue", reviewer=scripted({"turn1": [commit], "turn2": [commit]}, seen))
    assert out["held"] == ["held", "held"]
    assert out["turn2_sees"] == "base\n"  # its snapshot lacks turn 1
    assert out["turn1"]["status"] == "committed"
    assert (out["turn2"]["status"], out["turn2"]["paths"]) == ("conflict", ["n/guide.md"])  # 1.4
    assert (app.project / "n" / "guide.md").read_text() == "turn1:n/guide.md\n"
    # 1.5: turn 2's reviewer saw turn 1's change set and verdict.
    (h,) = seen["turn2"].history
    assert h.name == "turn1" and h.outcome.status == pb.OUTCOME_STATUS_COMMITTED
    assert "+turn1:n/guide.md" in h.change_set.diff
