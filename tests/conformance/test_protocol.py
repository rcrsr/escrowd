"""Phase 3.6: the frozen protocol (docs/protocol.md).

Each status code the daemon returns raises its typed error in the SDK (still a
grpc.RpcError); an outcome status or tier the SDK does not know is "not decided yet"
and "unknown", so a later daemon can add review states; the spec names every call.
"""

import re
from pathlib import Path

import grpc
import pytest

import escrow
from escrow import _sdk
from escrow.v1 import escrow_pb2 as pb

ROOT = Path(__file__).resolve().parents[2]


def raised(f) -> escrow.EscrowRpcError:
    with pytest.raises(escrow.EscrowRpcError) as err:
        f()
    assert isinstance(err.value, grpc.RpcError)
    return err.value


def test_each_status_code_raises_its_typed_error(daemon, runtime_dir):
    with escrow.connect(str(daemon.socket)) as c:
        sid = c.open_scope("t").scope_id
        cases = [
            (lambda: c.close_scope("nope"), escrow.EscrowNotFoundError, "NOT_FOUND"),
            (
                lambda: c.close_scope(sid, token="wrong"),
                escrow.EscrowPermissionError,
                "PERMISSION_DENIED",
            ),
            (lambda: c.get_change_set(sid), escrow.EscrowStateError, "FAILED_PRECONDITION"),
            (lambda: c.settle_unscoped(), escrow.EscrowStateError, "FAILED_PRECONDITION"),
            (
                lambda: c.decide(sid, pb.VERDICT_UNSPECIFIED),
                escrow.EscrowInvalidArgumentError,
                "INVALID_ARGUMENT",
            ),
        ]
        for call, cls, code in cases:
            e = raised(call)
            assert type(e) is cls, (code, e)
            assert e.code() == getattr(grpc.StatusCode, code)
            assert e.details()
        c.discard(sid)
    # The review socket serves ReviewerService only, and the main socket EscrowService.
    with escrow.Reviewer(str(daemon.socket)) as rv:
        assert type(raised(rv.list_held)) is escrow.EscrowUnsupportedError
    with escrow.connect(str(runtime_dir / "absent.sock")) as c:
        assert type(raised(lambda: c.ping(timeout=2))) is escrow.EscrowUnavailableError


def test_a_stream_raises_typed_errors_too(daemon):
    with escrow.connect(str(daemon.socket)) as c:
        e = raised(lambda: list(c.await_decision("nope", timeout=5)))
    assert type(e) is escrow.EscrowNotFoundError


def test_unknown_values_are_not_decided_yet():
    cs = _sdk.ChangeSet.from_proto(
        pb.ChangeSet(
            scope_id="s",
            changes=[pb.Change(kind=99, path="a")],  # ty: ignore[invalid-argument-type]
            review=pb.Review(tiers=[pb.TIER_LLM, 9]),  # ty: ignore[invalid-argument-type]
        )
    )
    assert cs.changes[0].kind == "unknown"
    assert cs.review.tiers == ["llm", "unknown"]
    out = _sdk._outcome(pb.Outcome(status=42, tiers=[7]), cs)  # ty: ignore[invalid-argument-type]
    assert (out.status, out.tiers) == ("held", ["unknown"])
    assert _sdk._outcome(pb.Outcome(status=pb.OUTCOME_STATUS_RETURNED), cs).status == "returned"


def test_the_spec_names_every_call_and_the_frozen_version():
    proto = (ROOT / "proto/escrow/v1/escrow.proto").read_text()
    spec = (ROOT / "docs/protocol.md").read_text()
    calls = re.findall(r"^\s*rpc (\w+)\(", proto, re.M)
    assert len(calls) == 10
    assert [c for c in calls if f"`{c}`" not in spec] == []
    typed = [escrow.EscrowRpcError, *escrow.EscrowRpcError.__subclasses__()]
    assert len(typed) == 9
    assert [t.__name__ for t in typed if f"`{t.__name__}`" not in spec] == []
    assert "protocol_version` is 7" in spec
