"""Phase 1.4: commit through the journal, conflicts, snapshots and rollback.

Commit applies a closed scope's whole change set or none of it (exit test 4); a path
changed in the project since the scope saw it is a conflict, and nothing is written
(exit test 5); a scope keeps reading the base as it was when it opened, through the
pre-images of later commits (check 8); a fault at any commit step, in place or as a
daemon crash, leaves the project byte-identical.
"""

import os
import shutil
import subprocess

import grpc
import pytest
from conftest import Daemon, make_runtime_dir
from scenario import EXPECTED, fingerprint, mutate, seed, tree

import escrow
from escrow.v1 import escrow_pb2 as pb


def client(d):
    return escrow.connect(str(d.socket))


def leftovers(root):
    """Temporary files of a commit left in the project."""
    return [
        os.path.join(dp, n)
        for dp, dns, fns in os.walk(root)
        for n in dns + fns
        if n.startswith(".escrow-")
    ]


def reference(work, seed_fn, mutate_fn):
    """The tree the commit must produce: the same IO on a plain directory."""
    ref = work / "ref"
    shutil.rmtree(ref, ignore_errors=True)
    ref.mkdir()
    seed_fn(ref)
    mutate_fn(ref)
    return tree(ref)


def open_root(d, c):
    sid = c.open_scope().scope_id
    return sid, d.mount / sid


def test_commit_applies_the_change_set(daemon):
    d = daemon
    seed(d.project)
    want = reference(d.work, seed, mutate)
    with client(d) as c:
        sid, root = open_root(d, c)
        mutate(root)
        c.close_scope(sid)
        out = c.commit(sid)
    assert out.status == pb.OUTCOME_STATUS_COMMITTED
    assert sorted(out.paths) == sorted(e[1] for e in EXPECTED)
    assert tree(d.project) == want
    assert not leftovers(d.project)
    assert not (d.state / "scopes" / sid).exists() and d.generations() == []
    assert any(f" scope={sid} op=decide path= decision=commit" in line for line in d.ledger)


def test_commit_needs_a_closed_scope(daemon):
    with client(daemon) as c:
        sid = c.open_scope().scope_id
        with pytest.raises(grpc.RpcError) as err:
            c.commit(sid)
    assert err.value.code() == grpc.StatusCode.FAILED_PRECONDITION


def test_empty_commit_drops_the_scope(daemon):
    (daemon.project / "f.txt").write_text("f\n")
    before = fingerprint(daemon.project)
    with client(daemon) as c:
        sid, root = open_root(daemon, c)
        (root / "f.txt").read_text()
        c.close_scope(sid)
        out = c.commit(sid)
        assert out.status == pb.OUTCOME_STATUS_COMMITTED and list(out.paths) == []
        with pytest.raises(grpc.RpcError) as err:
            c.close_scope(sid)
    assert err.value.code() == grpc.StatusCode.NOT_FOUND
    assert fingerprint(daemon.project) == before


def test_second_writer_of_a_path_hits_the_conflict_policy(daemon):
    d = daemon
    (d.project / "f.txt").write_text("base\n")
    (d.project / "g.txt").write_text("g\n")
    with client(d) as c:
        a, ra = open_root(d, c)
        b, rb = open_root(d, c)
        (ra / "f.txt").write_text("from a\n")
        (rb / "f.txt").write_text("from b\n")
        (rb / "g.txt").write_text("from b\n")
        c.close_scope(a)
        c.close_scope(b)
        assert c.commit(a).status == pb.OUTCOME_STATUS_COMMITTED
        out = c.commit(b)
        assert out.status == pb.OUTCOME_STATUS_CONFLICT
        assert list(out.paths) == ["f.txt"]
        assert "f.txt" in out.reasons[0]
        with pytest.raises(grpc.RpcError) as err:
            c.close_scope(b)  # the default conflict policy discards the scope
    assert err.value.code() == grpc.StatusCode.NOT_FOUND
    assert (d.project / "f.txt").read_text() == "from a\n"
    assert (d.project / "g.txt").read_text() == "g\n"  # nothing of b was written
    assert any(f" scope={b} op=conflict path=f.txt decision=deny" in line for line in d.ledger)


def test_two_scopes_creating_one_path_conflict(daemon):
    d = daemon
    with client(d) as c:
        a, ra = open_root(d, c)
        b, rb = open_root(d, c)
        (ra / "new.txt").write_text("a\n")
        (rb / "new.txt").write_text("b\n")
        c.close_scope(a)
        c.close_scope(b)
        assert c.commit(a).status == pb.OUTCOME_STATUS_COMMITTED
        assert c.commit(b).status == pb.OUTCOME_STATUS_CONFLICT
    assert (d.project / "new.txt").read_text() == "a\n"


def test_disjoint_scopes_both_commit(daemon):
    d = daemon
    (d.project / "dir").mkdir()
    with client(d) as c:
        a, ra = open_root(d, c)
        b, rb = open_root(d, c)
        (ra / "dir" / "a.txt").write_text("a\n")
        (rb / "dir" / "b.txt").write_text("b\n")
        c.close_scope(a)
        c.close_scope(b)
        assert c.commit(a).status == pb.OUTCOME_STATUS_COMMITTED
        assert c.commit(b).status == pb.OUTCOME_STATUS_COMMITTED
    assert sorted(os.listdir(d.project / "dir")) == ["a.txt", "b.txt"]


def test_deleting_a_directory_that_gained_entries_conflicts(daemon):
    d = daemon
    (d.project / "dir").mkdir()
    (d.project / "dir" / "old.txt").write_text("old\n")
    with client(d) as c:
        a, ra = open_root(d, c)
        b, rb = open_root(d, c)
        (ra / "dir" / "new.txt").write_text("new\n")
        subprocess.run(["rm", "-r", rb / "dir"], check=True)
        c.close_scope(a)
        c.close_scope(b)
        assert c.commit(a).status == pb.OUTCOME_STATUS_COMMITTED
        assert c.commit(b).status == pb.OUTCOME_STATUS_CONFLICT
    assert sorted(os.listdir(d.project / "dir")) == ["new.txt", "old.txt"]


def snapshot_seed(project):
    (project / "f.txt").write_text("old\n")
    (project / "g.txt").write_text("g\n")
    (project / "dir").mkdir()
    (project / "dir" / "x.txt").write_text("x\n")


def snapshot_change(root):
    (root / "f.txt").write_text("new content\n")
    (root / "g.txt").unlink()
    (root / "n.txt").write_text("n\n")
    subprocess.run(["rm", "-r", root / "dir"], check=True)


def assert_old_base(root):
    assert (root / "f.txt").read_text() == "old\n"
    assert (root / "g.txt").read_text() == "g\n"
    assert not (root / "n.txt").exists()
    assert (root / "dir" / "x.txt").read_text() == "x\n"
    assert sorted(os.listdir(root)) == ["dir", "f.txt", "g.txt"]


def test_scope_keeps_its_snapshot_through_another_commit(daemon):
    d = daemon
    snapshot_seed(d.project)
    with client(d) as c:
        old, rold = open_root(d, c)
        a, ra = open_root(d, c)
        snapshot_change(ra)
        c.close_scope(a)
        c.commit(a)
        assert sorted(os.listdir(d.project)) == ["f.txt", "n.txt"]
        assert_old_base(rold)
        assert os.stat(rold / "f.txt").st_size == 4  # the pre-image's size, not the new file's
        new, rnew = open_root(d, c)
        assert sorted(os.listdir(rnew)) == ["f.txt", "n.txt"]
        assert (rnew / "f.txt").read_text() == "new content\n"
        assert d.generations() != []
        c.discard(old)
        assert d.generations() == []  # no open scope reads through the pre-images


def test_snapshot_scope_writing_a_committed_path_conflicts(daemon):
    d = daemon
    snapshot_seed(d.project)
    with client(d) as c:
        old, rold = open_root(d, c)
        a, ra = open_root(d, c)
        snapshot_change(ra)
        c.close_scope(a)
        c.commit(a)
        (rold / "f.txt").write_text((rold / "f.txt").read_text() + "more\n")
        (rold / "other.txt").write_text("o\n")
        cs = c.close_scope(old)
        assert [(ch.path, ch.kind) for ch in cs.changes] == [
            ("f.txt", pb.CHANGE_KIND_MODIFY),
            ("other.txt", pb.CHANGE_KIND_CREATE),
        ]
        out = c.commit(old)
    assert out.status == pb.OUTCOME_STATUS_CONFLICT and list(out.paths) == ["f.txt"]
    assert (d.project / "f.txt").read_text() == "new content\n"
    assert not (d.project / "other.txt").exists()


def test_snapshot_survives_a_daemon_restart(start_daemon):
    d = start_daemon()
    snapshot_seed(d.project)
    with client(d) as c:
        old, rold = open_root(d, c)
        a, ra = open_root(d, c)
        snapshot_change(ra)
        c.close_scope(a)
        c.commit(a)
    d.stop()
    d2 = start_daemon()
    assert_old_base(d2.mount / old)
    with client(d2) as c:
        c.discard(old)
    assert d2.generations() == []


# ---- faults: a small scenario, so every step of its commit can be hit ----


def small_seed(project):
    for rel, text in {
        "a.txt": "a\n",
        "b.txt": "b\n",
        "c.txt": "c\n",
        "d/x.txt": "x\n",
        "d/y.txt": "y\n",
        "e/f.txt": "f\n",
        "t/z.txt": "z\n",
    }.items():
        (project / rel).parent.mkdir(parents=True, exist_ok=True)
        (project / rel).write_text(text)
    os.symlink("a.txt", project / "link")


def small_mutate(root):
    """Modify, delete, rename, rm -r, create in a new dir, chmod a dir, a directory
    replaced by a file, a symlink swap."""
    (root / "a.txt").write_text("a2\n")
    (root / "b.txt").unlink()
    os.rename(root / "c.txt", root / "c2.txt")
    subprocess.run(["rm", "-r", root / "d"], check=True)
    (root / "new").mkdir()
    (root / "new" / "n.txt").write_text("n\n")
    os.chmod(root / "e", 0o700)
    subprocess.run(["rm", "-r", root / "t"], check=True)
    (root / "t").write_text("t is a file now\n")
    os.unlink(root / "link")
    os.symlink("c2.txt", root / "link")


def run_with_fault(escrow_bin, spec: str) -> bool:
    """Commit the small scenario with ESCROWD_FAULT=spec. Returns False if the fault was
    never reached (the commit succeeded); otherwise checks the rollback and the retry."""
    work = make_runtime_dir()
    crash = spec.endswith(":abort")
    try:
        d = Daemon(escrow_bin, work, env={"ESCROWD_FAULT": spec})
        small_seed(d.project)
        want = reference(work, small_seed, small_mutate)
        before = fingerprint(d.project)
        with client(d) as c:
            sid, root = open_root(d, c)
            small_mutate(root)
            c.close_scope(sid)
            try:
                c.commit(sid, timeout=10)
            except grpc.RpcError as err:
                code = err.code()
            else:
                d.stop()
                assert tree(d.project) == want
                return False
        if crash:
            assert code == grpc.StatusCode.UNAVAILABLE
            assert d.proc.wait(timeout=10) != 0
        else:
            assert code == grpc.StatusCode.ABORTED, spec
        d.stop()
        d = Daemon(escrow_bin, work)  # the restart rolls back an unfinished commit
        assert fingerprint(d.project) == before, spec
        assert not leftovers(d.project), spec
        assert d.generations() == [], spec
        with client(d) as c:  # the scope is still closed: the retry applies it
            assert c.commit(sid).status == pb.OUTCOME_STATUS_COMMITTED, spec
        assert tree(d.project) == want, spec
        d.stop()
        return True
    finally:
        subprocess.run(["fusermount3", "-u", "-z", work / "mnt"], capture_output=True)
        shutil.rmtree(work, ignore_errors=True)


@pytest.mark.parametrize("mode", ["error", "crash"])
def test_fault_at_every_commit_step_leaves_the_base_byte_identical(escrow_bin, mode):
    suffix = ":abort" if mode == "crash" else ""
    for point in ("journal", "preimage", "apply", "done"):
        n = 0
        while run_with_fault(escrow_bin, f"{point}:{n}{suffix}"):
            n += 1
        assert n > 0, f"fault point {point} never reached"


def test_crash_after_done_finishes_the_commit_on_restart(escrow_bin):
    work = make_runtime_dir()
    try:
        d = Daemon(escrow_bin, work, env={"ESCROWD_FAULT": "committed:0:abort"})
        small_seed(d.project)
        want = reference(work, small_seed, small_mutate)
        with client(d) as c:
            sid, root = open_root(d, c)
            small_mutate(root)
            c.close_scope(sid)
            with pytest.raises(grpc.RpcError):
                c.commit(sid, timeout=10)
        d.stop()
        d = Daemon(escrow_bin, work)
        assert tree(d.project) == want
        assert not (d.state / "scopes" / sid).exists()
        with client(d) as c:
            c.open_scope()  # reuses the scope index; recovery must not drop it on the next start
        d.stop()
        d = Daemon(escrow_bin, work)
        assert os.listdir(d.state / "scopes") == [sid]
        d.stop()
    finally:
        subprocess.run(["fusermount3", "-u", "-z", work / "mnt"], capture_output=True)
        shutil.rmtree(work, ignore_errors=True)
