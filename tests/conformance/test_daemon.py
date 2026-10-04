"""Phase 1.1: the daemon serves the protocol over its Unix socket."""

import grpc
import pytest

import escrow
from escrow.v1 import escrow_pb2


def test_ping_round_trip(daemon):
    with escrow.connect(str(daemon.socket)) as client:
        reply = client.ping()
    assert reply.protocol_version == 1
    assert reply.daemon_version == "0.1.0"


def test_connect_reads_escrow_socket(daemon, monkeypatch):
    monkeypatch.setenv("ESCROW_SOCKET", str(daemon.socket))
    with escrow.connect() as client:
        assert client.ping().protocol_version == 1


@pytest.mark.parametrize(
    "call, request_type",
    [
        ("OpenScope", escrow_pb2.OpenScopeRequest),
        ("CloseScope", escrow_pb2.CloseScopeRequest),
        ("Decide", escrow_pb2.DecideRequest),
        ("Spawn", escrow_pb2.SpawnRequest),
        ("SettleUnscoped", escrow_pb2.SettleUnscopedRequest),
    ],
)
def test_unbuilt_calls_are_unimplemented(daemon, call, request_type):
    with escrow.connect(str(daemon.socket)) as client:
        with pytest.raises(grpc.RpcError) as err:
            getattr(client._stub, call)(request_type(), timeout=5)
    assert err.value.code() == grpc.StatusCode.UNIMPLEMENTED


def test_sigterm_removes_socket(daemon):
    assert daemon.stop() == 0
    assert not daemon.socket.exists()


def test_stale_socket_is_replaced(escrow_bin, runtime_dir):
    from conftest import Daemon

    sock = runtime_dir / "escrow.sock"
    first = Daemon(escrow_bin, sock)
    first.proc.kill()  # SIGKILL leaves the socket file behind
    first.proc.wait()
    assert sock.exists()
    # Returns at once (the stale file already exists), so poll with ping.
    second = Daemon(escrow_bin, sock)
    try:
        for _ in range(100):
            try:
                with escrow.connect(str(sock)) as client:
                    assert client.ping(timeout=0.5).protocol_version == 1
                break
            except grpc.RpcError:
                continue
        else:
            pytest.fail("second daemon never answered on the stale socket path")
    finally:
        second.stop()
