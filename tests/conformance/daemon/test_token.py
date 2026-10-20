"""Phase 2.7: the scope token (check 7).

OpenScope returns a random token with the scope id; CloseScope, Decide and the exec
socket need it. Knowing a scope id (it is in every view path) is not enough to close,
decide or start children in that scope. The unscoped scope has no token.
"""

import sqlite3

import grpc
import pytest

from escrow import lowlevel
from escrow.v1 import escrow_pb2 as pb
from escrow_testkit import Daemon


def denied(call) -> str:
    with pytest.raises(grpc.RpcError) as err:
        call()
    assert err.value.code() == grpc.StatusCode.PERMISSION_DENIED
    return err.value.details()


def test_open_returns_a_fresh_random_token(daemon):
    with daemon.client() as c:
        a, b = c.open_scope(), c.open_scope()
    assert len(a.token) == 64 and int(a.token, 16) >= 0  # 32 random bytes, hex
    assert a.token != b.token
    store = daemon.state / "scopes" / a.scope_id
    stored = b"".join(p.read_bytes() for p in store.glob("meta.sqlite*"))
    assert a.token.encode() not in stored  # only its SHA-256


def test_close_and_decide_need_the_token(daemon):
    d = daemon
    with d.client() as c:
        s = c.open_scope()
        (d.mount / s.scope_id / "f.txt").write_text("x\n")
        assert "missing or wrong token" in denied(lambda: c.close_scope(s.scope_id, token=""))
        denied(lambda: c.close_scope(s.scope_id, token="0" * 64))
        denied(lambda: c.discard(s.scope_id, token=""))  # an open scope cannot be dropped either
        c.close_scope(s.scope_id, token=s.token)
        for verdict in (pb.VERDICT_COMMIT, pb.VERDICT_DISCARD, pb.VERDICT_RETURN):
            denied(lambda v=verdict: c.decide(s.scope_id, v, token=""))
        assert not (d.project / "f.txt").exists()
        assert c.commit(s.scope_id, token=s.token).status == pb.OUTCOME_STATUS_COMMITTED
    assert (d.project / "f.txt").read_text() == "x\n"
    assert any(f" scope={s.scope_id} op=token path= decision=deny" in line for line in d.ledger)


def test_another_client_with_the_token_can_decide(daemon):
    with daemon.client() as opener:
        s = opener.open_scope()
    with lowlevel.connect(str(daemon.socket)) as other:  # shares no tokens with the opener
        denied(lambda: other.close_scope(s.scope_id, token=""))
        other.close_scope(s.scope_id, token=s.token)
        assert other.discard(s.scope_id, token=s.token).status == pb.OUTCOME_STATUS_DISCARDED


def test_exec_needs_the_token_and_the_child_never_sees_it(daemon):
    d = daemon
    with d.client() as c:
        s = c.open_scope()
    r = d.exec(s.scope_id, "true", env={"ESCROW_SCOPE_TOKEN": "0" * 64})
    assert r.returncode == 125 and "missing or wrong token" in r.stderr, r.stderr
    r = d.exec(s.scope_id, "true", env={"ESCROW_SCOPE_TOKEN": None})
    assert r.returncode == 125, r.stderr
    r = d.exec(s.scope_id, "sh", "-c", 'echo "${ESCROW_SCOPE_TOKEN:-none}"; echo x > f.txt')
    assert (r.returncode, r.stdout) == (0, "none\n"), r.stderr
    assert (d.upper(s.scope_id) / "f.txt").exists()


def test_the_token_survives_a_daemon_restart(escrow_bin, runtime_dir):
    d = Daemon(escrow_bin, runtime_dir)
    with d.client() as c:
        s = c.open_scope()
    d.restart()
    try:
        with d.client() as c:
            denied(lambda: c.close_scope(s.scope_id, token=""))
            c.close_scope(s.scope_id, token=s.token)
            assert c.discard(s.scope_id, token=s.token).status == pb.OUTCOME_STATUS_DISCARDED
    finally:
        d.stop()


def test_the_unscoped_scope_needs_no_token(start_daemon):
    d = start_daemon(unscoped="implicit")
    (d.mount / "unscoped" / "u.txt").write_text("u\n")
    with d.client() as c:
        cs = c.settle_unscoped()
        assert cs.scope_id == "unscoped"
        assert c.commit("unscoped", token="").status == pb.OUTCOME_STATUS_COMMITTED
    assert (d.project / "u.txt").read_text() == "u\n"


def test_a_scope_stored_without_a_token_is_discarded_at_start(escrow_bin, runtime_dir):
    home = runtime_dir / "home"
    home.mkdir()
    config = {
        "unscoped": "implicit",
        "roots": "  home:\n    default: capture\n",
        "env": {"HOME": str(home)},
    }
    d = Daemon(escrow_bin, runtime_dir, **config)
    with d.client() as c:
        a, b = c.open_scope(), c.open_scope()
    (d.mount / a.scope_id / "f.txt").write_text("a\n")
    (d.mount / f"{a.scope_id}.home" / "x").write_text("x\n")
    (d.mount / "unscoped" / "u.txt").write_text("u\n")
    d.stop()
    # A scope an older daemon stored has no token hash.
    meta = d.state / "scopes" / a.scope_id / "meta.sqlite"
    db = sqlite3.connect(meta)
    db.execute("DELETE FROM meta WHERE key = 'token_sha256'")
    db.commit()
    db.close()
    d = Daemon(escrow_bin, runtime_dir, **config)
    try:
        with d.client() as c:
            with pytest.raises(grpc.RpcError) as err:
                c.close_scope(a.scope_id, token="")
            assert err.value.code() == grpc.StatusCode.NOT_FOUND
            scopes = d.state / "scopes"
            assert not (scopes / a.scope_id).exists()
            assert not (scopes / f"{a.scope_id}.home").exists()
            assert not (d.project / "f.txt").exists()
            line = f" scope={a.scope_id} op=decide path= decision=discard"
            assert any(line in entry for entry in d.ledger)
            denied(lambda: c.close_scope(b.scope_id, token=""))
            denied(lambda: c.close_scope(b.scope_id, token="0" * 64))
            c.close_scope(b.scope_id, token=b.token)
            assert c.discard(b.scope_id, token=b.token).status == pb.OUTCOME_STATUS_DISCARDED
            assert [ch.path for ch in c.settle_unscoped().changes] == ["u.txt"]
    finally:
        d.stop()


def test_a_restart_keeps_the_testkit_tokens(daemon):
    sid = daemon.client().open_scope().scope_id
    daemon.restart()
    daemon.client().close_scope(sid)
