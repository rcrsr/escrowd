"""Phase 1.1: the daemon serves the protocol over its Unix socket."""

import time

import grpc
import pytest

import escrow


def test_ping_round_trip(daemon):
    with escrow.connect(str(daemon.socket)) as client:
        reply = client.ping()
    assert reply.protocol_version == 7
    assert reply.daemon_version == "0.1.0"


def test_connect_reads_escrow_socket(daemon, monkeypatch):
    monkeypatch.setenv("ESCROW_SOCKET", str(daemon.socket))
    with escrow.connect() as client:
        assert client.ping().protocol_version == 7


def test_settle_unscoped_needs_the_implicit_mode(daemon):
    with escrow.connect(str(daemon.socket)) as client:
        with pytest.raises(escrow.EscrowStateError) as err:
            client.settle_unscoped(timeout=5)
    assert err.value.code() == grpc.StatusCode.FAILED_PRECONDITION


def test_sigterm_removes_socket(daemon):
    assert daemon.stop() == 0
    assert not daemon.socket.exists()


def test_stale_socket_is_replaced(start_daemon):
    first = start_daemon()
    first.proc.kill()  # SIGKILL leaves the socket file and a stale mount behind
    first.proc.wait()
    first.stop()
    assert first.socket.exists()
    second = start_daemon()  # ready once the socket path accepts connections again
    for _ in range(100):
        try:
            with escrow.connect(str(second.socket)) as client:
                assert client.ping(timeout=0.5).protocol_version == 7
            return
        except grpc.RpcError:
            time.sleep(0.05)
    pytest.fail("second daemon never answered on the stale socket path")
