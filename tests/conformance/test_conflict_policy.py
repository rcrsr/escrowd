"""Phase 2.7: the policy's conflict options and the passthrough ledger line.

`conflict.verdict: return` reopens a conflicting scope with its changes instead of
dropping it, and sends the conflicting paths back as reasons. `conflict.reads: true`
also fails the commit when a file the scope only read changed in the project since its
snapshot. In `passthrough` mode the ledger says once, at start, that IO outside scopes
goes unescrowed.
"""

import grpc
import pytest

import escrow
from escrow.v1 import escrow_pb2 as pb


def client(d):
    return escrow.connect(str(d.socket))


def two_writers(d, c):
    """Scopes a and b both write f.txt; a commits first. Returns b's id and view."""
    (d.project / "f.txt").write_text("base\n")
    a, b = c.open_scope().scope_id, c.open_scope().scope_id
    (d.mount / a / "f.txt").write_text("from a\n")
    (d.mount / b / "f.txt").write_text("from b\n")
    (d.mount / b / "g.txt").write_text("g from b\n")
    c.close_scope(a)
    c.close_scope(b)
    assert c.commit(a).status == pb.OUTCOME_STATUS_COMMITTED
    return b, d.mount / b


def test_conflict_discards_by_default(daemon):
    with client(daemon) as c:
        b, _ = two_writers(daemon, c)
        out = c.commit(b)
        assert (out.status, out.reopened) == (pb.OUTCOME_STATUS_CONFLICT, False)
        with pytest.raises(grpc.RpcError) as err:
            c.close_scope(b)
    assert err.value.code() == grpc.StatusCode.NOT_FOUND


def test_conflict_verdict_return_reopens_the_scope_with_its_changes(start_daemon):
    d = start_daemon(policy="conflict:\n  verdict: return\n")
    with client(d) as c:
        b, view = two_writers(d, c)
        out = c.commit(b)
        assert (out.status, out.reopened) == (pb.OUTCOME_STATUS_CONFLICT, True)
        assert list(out.paths) == ["f.txt"] and "f.txt" in out.reasons[0]
        # Open again, changes kept: the agent can undo the conflicting edit and retry.
        assert (view / "f.txt").read_text() == "from b\n"
        (view / "f.txt").unlink()
        (view / "h.txt").write_text("h\n")
        cs = c.close_scope(b)
        assert sorted(ch.path for ch in cs.changes) == ["f.txt", "g.txt", "h.txt"]
        assert c.commit(b).status == pb.OUTCOME_STATUS_CONFLICT  # deleting f.txt conflicts too
        assert c.discard(b).status == pb.OUTCOME_STATUS_DISCARDED
    assert (d.project / "f.txt").read_text() == "from a\n"
    assert not (d.project / "g.txt").exists()
    decided = [line.split()[-1] for line in d.ledger if f" scope={b} op=decide " in line]
    assert decided == ["decision=conflict", "decision=return"] * 2 + ["decision=discard"]


def reader_and_editor(d, c):
    """Scope r reads r.txt and writes w.txt; scope e changes r.txt and commits first."""
    (d.project / "r.txt").write_text("v1\n")
    (d.project / "skip.txt").write_text("s\n")
    r, e = c.open_scope().scope_id, c.open_scope().scope_id
    assert (d.mount / r / "r.txt").read_text() == "v1\n"
    (d.mount / r / "w.txt").write_text("w\n")
    (d.mount / e / "r.txt").write_text("v2\n")
    (d.project / "skip.txt").write_text("changed, never read\n")
    c.close_scope(e)
    assert c.commit(e).status == pb.OUTCOME_STATUS_COMMITTED
    c.close_scope(r)
    return r


def test_reads_do_not_conflict_by_default(daemon):
    with client(daemon) as c:
        r = reader_and_editor(daemon, c)
        assert c.commit(r).status == pb.OUTCOME_STATUS_COMMITTED
    assert (daemon.project / "w.txt").read_text() == "w\n"


def test_conflict_reads_fails_a_commit_whose_reads_changed(start_daemon):
    d = start_daemon(policy="conflict:\n  reads: true\n")
    with client(d) as c:
        r = reader_and_editor(d, c)
        out = c.commit(r)
    assert out.status == pb.OUTCOME_STATUS_CONFLICT
    assert list(out.paths) == ["r.txt"]  # skip.txt changed too, but the scope never read it
    assert not (d.project / "w.txt").exists()
    assert any(f" scope={r} op=conflict path=r.txt decision=deny" in line for line in d.ledger)


def test_conflict_reads_passes_when_reads_are_unchanged(start_daemon):
    d = start_daemon(policy="conflict:\n  reads: true\n")
    (d.project / "r.txt").write_text("v1\n")
    with client(d) as c:
        s = c.open_scope().scope_id
        assert (d.mount / s / "r.txt").read_text() == "v1\n"
        (d.mount / s / "w.txt").write_text("w\n")
        c.close_scope(s)
        assert c.commit(s).status == pb.OUTCOME_STATUS_COMMITTED


def test_passthrough_mode_is_in_the_ledger(start_daemon):
    d = start_daemon(unscoped="passthrough")
    assert [line.split(" ", 1)[1] for line in d.ledger if "op=passthrough" in line] == [
        "scope=unscoped op=passthrough path= decision=allow"
    ]


@pytest.mark.parametrize("mode", ["implicit", "deny"])
def test_other_modes_log_no_passthrough(start_daemon, mode):
    d = start_daemon(unscoped=mode)
    assert not any("op=passthrough" in line for line in d.ledger)
