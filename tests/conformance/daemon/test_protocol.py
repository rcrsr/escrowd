"""Phase 3.6: the frozen protocol (docs/protocol.md).

Each status code the daemon returns raises its typed error in the SDK (still a
grpc.RpcError); an outcome status or tier the SDK does not know is "not decided yet"
and "unknown", so a later daemon can add review states; the spec names every call.
"""

import re
from pathlib import Path

import grpc
import pytest

from escrow import lowlevel
from escrow.v1 import escrow_pb2 as pb

ROOT = Path(__file__).resolve().parents[3]


def raised(f) -> lowlevel.EscrowRpcError:
    with pytest.raises(lowlevel.EscrowRpcError) as err:
        f()
    assert isinstance(err.value, grpc.RpcError)
    return err.value


def test_each_status_code_raises_its_typed_error(daemon, runtime_dir):
    with daemon.client() as c:
        sid = c.open_scope("t").scope_id
        cases = [
            (lambda: c.close_scope("nope"), lowlevel.EscrowNotFoundError, "NOT_FOUND"),
            (
                lambda: c.close_scope(sid, token="wrong"),
                lowlevel.EscrowPermissionError,
                "PERMISSION_DENIED",
            ),
            (lambda: c.get_change_set(sid), lowlevel.EscrowStateError, "FAILED_PRECONDITION"),
            (lambda: c.settle_unscoped(), lowlevel.EscrowStateError, "FAILED_PRECONDITION"),
            (
                lambda: c.decide(sid, pb.VERDICT_UNSPECIFIED),
                lowlevel.EscrowInvalidArgumentError,
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
    with lowlevel.Reviewer(str(daemon.socket)) as rv:
        assert type(raised(rv.list_held)) is lowlevel.EscrowUnsupportedError
    with lowlevel.connect(str(runtime_dir / "absent.sock")) as c:
        assert type(raised(lambda: c.ping(timeout=2))) is lowlevel.EscrowUnavailableError


def test_a_stream_raises_typed_errors_too(daemon):
    with daemon.client() as c:
        e = raised(lambda: list(c.await_decision("nope", timeout=5)))
    assert type(e) is lowlevel.EscrowNotFoundError


def test_the_spec_names_every_call_and_the_frozen_version():
    proto = (ROOT / "proto/escrow/v1/escrow.proto").read_text()
    spec = (ROOT / "docs/protocol.md").read_text()
    calls = re.findall(r"^\s*rpc (\w+)\(", proto, re.M)
    assert len(calls) == 10
    assert [c for c in calls if f"`{c}`" not in spec] == []
    typed = [lowlevel.EscrowRpcError, *lowlevel.EscrowRpcError.__subclasses__()]
    assert len(typed) == 9
    assert [t.__name__ for t in typed if f"`{t.__name__}`" not in spec] == []
    assert "protocol_version` is 8" in spec
