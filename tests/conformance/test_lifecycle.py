"""Phase 1.3: scope lifecycle, gate and ledger.

Close freezes a scope and returns its change set (changes, reads, labels); discard drops
it; return reopens it. Read rules come from the policy file; every operation and every
lifecycle event lands in the ledger with its scope.
"""

import errno
import os
import subprocess

import grpc
import pytest
from scenario import EXPECTED, fingerprint, mutate, seed

import escrow
from escrow.v1 import escrow_pb2 as pb

KIND = {
    pb.CHANGE_KIND_CREATE: "create",
    pb.CHANGE_KIND_MODIFY: "modify",
    pb.CHANGE_KIND_DELETE: "delete",
    pb.CHANGE_KIND_RENAME: "rename",
}


def client(d):
    return escrow.connect(str(d.socket))


def changes(cs) -> list[tuple]:
    return [
        (KIND[c.kind], c.path, c.from_path) if c.from_path else (KIND[c.kind], c.path)
        for c in cs.changes
    ]


def reads(cs) -> list[tuple]:
    return [(r.path, r.decision == pb.READ_DECISION_ALLOW) for r in cs.reads]


def work(root):
    """Two reads (one denied), then the shared scenario's writes."""
    (root / "keep.txt").read_text()
    with pytest.raises(PermissionError):
        (root / ".env").read_text()
    mutate(root)


@pytest.fixture
def scoped(daemon):
    seed(daemon.project)
    with client(daemon) as c:
        s = c.open_scope("work", labels={"call": "c-17", "tool": "bash"})
    return daemon, s.scope_id, daemon.mount / s.scope_id


def test_close_returns_the_net_change_set(scoped):
    d, sid, root = scoped
    work(root)
    with client(d) as c:
        cs = c.close_scope(sid)
    assert changes(cs) == EXPECTED


def test_change_set_carries_reads_and_labels(scoped):
    d, sid, root = scoped
    work(root)
    with client(d) as c:
        cs = c.close_scope(sid)
    assert reads(cs) == [(".env", False), ("keep.txt", True)]
    assert dict(cs.labels) == {"call": "c-17", "tool": "bash"}


def test_close_is_idempotent(scoped):
    d, sid, root = scoped
    work(root)
    with client(d) as c:
        assert changes(c.close_scope(sid)) == changes(c.close_scope(sid)) == EXPECTED


def test_base_is_unchanged_through_close_and_discard(scoped):
    d, sid, root = scoped
    before = fingerprint(d.project)
    work(root)
    with client(d) as c:
        c.close_scope(sid)
        assert fingerprint(d.project) == before
        c.discard(sid)
    assert fingerprint(d.project) == before


def test_closed_scope_refuses_new_io_with_erofs(scoped):
    d, sid, root = scoped
    with client(d) as c:
        c.close_scope(sid)
    attempts = [
        lambda: open(root / "keep.txt").read(),
        lambda: open(root / "keep.txt", "w"),
        lambda: open(root / "fresh.txt", "w"),
        lambda: (root / "newdir").mkdir(),
        lambda: (root / "edit.txt").unlink(),
        lambda: os.rename(root / "edit.txt", root / "e2.txt"),
        lambda: os.chmod(root / "edit.txt", 0o600),
    ]
    for attempt in attempts:
        with pytest.raises(OSError) as err:
            attempt()
        assert err.value.errno == errno.EROFS


def test_handle_opened_before_close_cannot_write_after(scoped):
    d, sid, root = scoped
    f = open(root / "edit.txt", "w")
    f.write("before close\n")
    f.flush()
    os.fsync(f.fileno())
    with client(d) as c:
        c.close_scope(sid)
    f.write("after close\n")
    f.flush()  # lands in the kernel's writeback cache
    with pytest.raises(OSError) as err:
        os.fsync(f.fileno())  # reaching the daemon fails
    assert err.value.errno == errno.EBADF
    with pytest.raises(OSError):
        f.close()
    assert (d.upper(sid) / "edit.txt").read_text() == "before close\n"


def test_return_reopens_the_scope_and_keeps_its_changes(scoped):
    d, sid, root = scoped
    (root / "edit.txt").write_text("first\n")
    with client(d) as c:
        assert changes(c.close_scope(sid)) == [("modify", "edit.txt")]
        out = c.decide(sid, pb.VERDICT_RETURN, reasons=["edit.txt: missing newline at EOF"])
        assert out.status == pb.OUTCOME_STATUS_RETURNED
        assert list(out.reasons) == ["edit.txt: missing newline at EOF"]
        (root / "extra.txt").write_text("fix\n")
        assert changes(c.close_scope(sid)) == [("modify", "edit.txt"), ("create", "extra.txt")]


def test_return_needs_a_closed_scope(scoped):
    d, sid, _ = scoped
    with client(d) as c, pytest.raises(grpc.RpcError) as err:
        c.decide(sid, pb.VERDICT_RETURN)
    assert err.value.code() == grpc.StatusCode.FAILED_PRECONDITION


def test_close_of_unknown_scope_is_not_found(daemon):
    with client(daemon) as c, pytest.raises(grpc.RpcError) as err:
        c.close_scope("s999")
    assert err.value.code() == grpc.StatusCode.NOT_FOUND


def test_closed_state_survives_a_daemon_restart(start_daemon):
    d = start_daemon()
    seed(d.project)
    with client(d) as c:
        sid = c.open_scope().scope_id
    (d.mount / sid / "edit.txt").write_text("staged\n")
    with client(d) as c:
        c.close_scope(sid)
    d.stop()
    d2 = start_daemon()
    with pytest.raises(OSError) as err:
        (d2.mount / sid / "edit.txt").write_text("again\n")
    assert err.value.errno == errno.EROFS
    with client(d2) as c:
        assert changes(c.close_scope(sid)) == [("modify", "edit.txt")]
        c.decide(sid, pb.VERDICT_RETURN)
    (d2.mount / sid / "edit.txt").write_text("again\n")


def test_policy_path_globs_deny_nested_reads(start_daemon):
    d = start_daemon(deny_read=(".env", "secrets/**"))
    (d.project / "secrets" / "deep").mkdir(parents=True)
    (d.project / "secrets" / "deep" / "key.pem").write_text("k\n")
    (d.project / "notsecrets").mkdir()
    (d.project / "notsecrets" / "key.pem").write_text("k\n")
    (d.project / "sub").mkdir()
    (d.project / "sub" / ".env").write_text("X=1\n")
    with client(d) as c:
        sid = c.open_scope().scope_id
    root = d.mount / sid
    for denied in ("secrets/deep/key.pem", "sub/.env"):
        with pytest.raises(PermissionError):
            (root / denied).read_text()
    assert (root / "notsecrets" / "key.pem").read_text() == "k\n"


def test_no_policy_denies_nothing(start_daemon):
    d = start_daemon(deny_read=())
    (d.project / ".env").write_text("X=1\n")
    with client(d) as c:
        sid = c.open_scope().scope_id
    assert (d.mount / sid / ".env").read_text() == "X=1\n"


def test_invalid_policy_stops_the_daemon(escrow_bin, runtime_dir):
    (runtime_dir / "proj").mkdir()
    bad = runtime_dir / "policy.yaml"
    bad.write_text("version: 1\nread:\n  allow: ['*']\n")
    r = subprocess.run(
        [
            escrow_bin,
            "daemon",
            "--socket",
            runtime_dir / "s.sock",
            "--project",
            runtime_dir / "proj",
            "--state",
            runtime_dir / "state",
            "--mount",
            runtime_dir / "mnt",
            "--policy",
            bad,
        ],
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert r.returncode != 0 and "policy" in r.stderr


def test_ledger_records_lifecycle_events_and_escapes_paths(scoped):
    d, sid, root = scoped
    (root / "with space=sign%.txt").write_text("x\n")
    os.listdir(root)
    with client(d) as c:
        c.close_scope(sid)
        c.discard(sid)
    ops = [(f.split()[2], f.split()[3], f.split()[-1]) for f in d.ledger if f" scope={sid} " in f]
    assert ops[0] == ("op=open", "path=", "decision=allow")
    assert ("op=create", "path=with%20space%3Dsign%25.txt", "decision=allow") in ops
    assert ("op=list", "path=", "decision=allow") in ops
    assert ops[-2:] == [
        ("op=close", "path=", "decision=allow"),
        ("op=decide", "path=", "decision=discard"),
    ]


def test_escrow_log_filters_by_scope(daemon, escrow_bin):
    with client(daemon) as c:
        a, b = c.open_scope().scope_id, c.open_scope().scope_id
    out = subprocess.run(
        [escrow_bin, "log", "--state", daemon.state, a], capture_output=True, text=True, check=True
    ).stdout.splitlines()
    assert out and all(f" scope={a} " in line for line in out)
    assert not any(f" scope={b} " in line for line in out)
