"""Phase 1.1: the daemon serves the protocol over its Unix socket."""

import os
import subprocess
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


def test_second_daemon_on_one_state_is_refused(daemon):
    # Start-up rolls back unfinished commits: a second daemon on the state would undo
    # the first one's commit mid-apply.
    args = [daemon.bin, "daemon", "--socket", daemon.work / "other.sock"]
    args += ["--project", daemon.project, "--state", daemon.state]
    args += ["--mount", daemon.work / "other-mnt"]
    run = subprocess.run(args, capture_output=True, text=True, timeout=10)
    assert run.returncode != 0
    assert "another escrowd" in run.stderr
    assert str(os.getpid()) not in run.stderr  # the holder's pid, not ours
    with escrow.connect(str(daemon.socket)) as client:
        assert client.ping(timeout=5).protocol_version == 7


def test_live_socket_is_not_taken_over(daemon):
    args = [daemon.bin, "daemon", "--socket", daemon.socket, "--project", daemon.project]
    args += ["--state", daemon.work / "other-state", "--mount", daemon.work / "other-mnt"]
    run = subprocess.run(args, capture_output=True, text=True, timeout=10)
    assert run.returncode != 0
    assert "in use" in run.stderr
    with escrow.connect(str(daemon.socket)) as client:
        assert client.ping(timeout=5).protocol_version == 7
