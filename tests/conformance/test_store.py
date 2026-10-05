"""Phase 1.2: scopes over RPC and the per-scope store.

Scopes are created and dropped over RPC; whiteouts, opaque directories, base versions
and inode numbers persist in each scope's SQLite store and survive a daemon restart.
"""

import os
import sqlite3
import subprocess

import grpc
import pytest

import escrow
from escrow.v1 import escrow_pb2


def client(d):
    return escrow.connect(str(d.socket))


def seed(project):
    (project / "keep.txt").write_text("keep\n")
    (project / "gone.txt").write_text("gone\n")
    (project / "old.txt").write_text("old\n")
    (project / "read.txt").write_text("read\n")
    (project / "tree").mkdir()
    (project / "tree" / "leaf.txt").write_text("leaf\n")


def meta_db(d, scope_id):
    return sqlite3.connect(d.state / "scopes" / scope_id / "meta.sqlite")


def test_open_scope_creates_a_root_in_the_mount(daemon):
    with client(daemon) as c:
        a, b = c.open_scope("first"), c.open_scope("second")
    assert a.scope_id != b.scope_id
    assert sorted(os.listdir(daemon.mount)) == sorted([a.scope_id, b.scope_id])
    assert a.root == str(daemon.mount / a.scope_id)


def test_scopes_are_not_created_by_mkdir(daemon):
    with pytest.raises(PermissionError):
        os.mkdir(daemon.mount / "sneaky")


def test_discard_drops_the_scope_and_its_store(daemon):
    (daemon.project / "f.txt").write_text("base\n")
    with client(daemon) as c:
        s = c.open_scope("doomed")
        (daemon.mount / s.scope_id / "f.txt").write_text("staged\n")
        out = c.discard(s.scope_id)
    assert out.status == escrow_pb2.OUTCOME_STATUS_DISCARDED
    assert not (daemon.state / "scopes" / s.scope_id).exists()
    assert s.scope_id not in os.listdir(daemon.mount)
    assert (daemon.project / "f.txt").read_text() == "base\n"


def test_discard_of_unknown_scope_is_not_found(daemon):
    with client(daemon) as c, pytest.raises(grpc.RpcError) as err:
        c.discard("s999")
    assert err.value.code() == grpc.StatusCode.NOT_FOUND


def test_store_records_whiteouts_opaque_dirs_and_versions(daemon):
    seed(daemon.project)
    with client(daemon) as c:
        s = c.open_scope()
    root = daemon.mount / s.scope_id
    (root / "read.txt").read_text()
    (root / "keep.txt").write_text("changed\n")
    (root / "gone.txt").unlink()
    subprocess.run(["rm", "-r", root / "tree"], check=True)
    (root / "tree").mkdir()
    with client(daemon) as c:
        c.close_scope(s.scope_id)  # first reads are written by close at the latest
    with meta_db(daemon, s.scope_id) as db:
        whiteouts = {r[0] for r in db.execute("SELECT path FROM whiteouts")}
        opaque = {r[0] for r in db.execute("SELECT path FROM opaque")}
        rows = db.execute("SELECT path, read, changed FROM versions")
        versions = {p: (r, c) for p, r, c in rows}
    assert whiteouts == {b"gone.txt"}  # tree/leaf.txt and tree folded into the opaque mark
    assert opaque == {b"tree"}
    assert versions[b"read.txt"] == (1, 0)
    assert versions[b"keep.txt"] == (0, 1)
    assert versions[b"gone.txt"] == (0, 1)


def test_version_is_the_base_version_first_seen(daemon):
    seed(daemon.project)
    st = (daemon.project / "keep.txt").stat()
    with client(daemon) as c:
        s = c.open_scope()
    root = daemon.mount / s.scope_id
    (root / "keep.txt").read_text()
    (root / "keep.txt").write_text("v2\n")
    (root / "keep.txt").write_text("v3\n")
    with meta_db(daemon, s.scope_id) as db:
        row = db.execute(
            "SELECT read, changed, ino, size, mtime_ns FROM versions WHERE path = ?", (b"keep.txt",)
        ).fetchone()
    assert row == (1, 1, st.st_ino, st.st_size, st.st_mtime_ns)


def test_scope_state_survives_a_daemon_restart(start_daemon):
    d = start_daemon()
    seed(d.project)
    with client(d) as c:
        s = c.open_scope("survivor")
    root = d.mount / s.scope_id
    (root / "gone.txt").unlink()
    (root / "new.txt").write_text("new\n")
    os.rename(root / "old.txt", root / "moved.txt")
    subprocess.run(["rm", "-r", root / "tree"], check=True)
    (root / "tree").mkdir()
    inos = {n: os.stat(root / n).st_ino for n in ("new.txt", "moved.txt", "keep.txt")}
    assert d.stop() == 0

    d2 = start_daemon()
    root = d2.mount / s.scope_id
    assert sorted(os.listdir(d2.mount)) == [s.scope_id]
    assert not (root / "gone.txt").exists()
    assert not (root / "old.txt").exists()
    assert (root / "moved.txt").read_text() == "old\n"
    assert (root / "new.txt").read_text() == "new\n"
    assert os.listdir(root / "tree") == []
    assert {n: os.stat(root / n).st_ino for n in inos} == inos
    with client(d2) as c:
        later = c.open_scope()
    assert int(later.scope_id[1:]) > int(s.scope_id[1:])


def test_hard_link_keeps_inode_after_original_unlinked(daemon):
    (daemon.project / "obj").write_text("blob\n")
    with client(daemon) as c:
        s = c.open_scope()
    root = daemon.mount / s.scope_id
    (root / "tmp").write_text("temp object\n")
    ino = os.stat(root / "tmp").st_ino
    os.link(root / "tmp", root / "final")
    os.unlink(root / "tmp")  # git links, then unlinks its temp objects
    assert os.stat(root / "final").st_ino == ino
    assert (root / "final").read_text() == "temp object\n"


def test_directory_renamed_onto_deleted_base_dir_hides_its_contents(daemon):
    (daemon.project / "dir").mkdir()
    (daemon.project / "dir" / "base.txt").write_text("base\n")
    with client(daemon) as c:
        s = c.open_scope()
    root = daemon.mount / s.scope_id
    subprocess.run(["rm", "-r", root / "dir"], check=True)
    (root / "fresh").mkdir()
    (root / "fresh" / "new.txt").write_text("new\n")
    os.rename(root / "fresh", root / "dir")
    assert sorted(os.listdir(root / "dir")) == ["new.txt"]


def test_daemon_refuses_a_mount_inside_the_project(escrow_bin, runtime_dir):
    proj = runtime_dir / "proj"
    proj.mkdir()
    r = subprocess.run(
        [
            escrow_bin,
            "daemon",
            "--socket",
            runtime_dir / "s.sock",
            "--project",
            proj,
            "--state",
            runtime_dir / "state",
            "--mount",
            proj / "view",
        ],
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert r.returncode != 0 and "must not contain one another" in r.stderr
