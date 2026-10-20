"""Launching from a relative script path, and the launcher's private hand-off."""

import builtins
import subprocess
from concurrent import futures

import grpc
import pytest

import escrow
from escrow.v1 import escrow_pb2, escrow_pb2_grpc
from escrow_testkit.apps import Sdk


@pytest.fixture
def sdk(escrow_bin, runtime_dir):
    s = Sdk(escrow_bin, runtime_dir)
    yield s
    for m in (runtime_dir / "run" / "escrowd").glob("*/view"):
        subprocess.run(["fusermount3", "-u", "-z", m], capture_output=True)


def test_relative_script_and_project_commit_to_the_project(sdk):
    sdk.run(
        """
        with escrow.scope("work"):
            (P / "made.txt").write_text("x")
        """,
        cwd=sdk.work,
        relative=True,
    )
    assert (sdk.work / "proj" / "made.txt").read_text() == "x"


def test_second_pass_returns_to_the_callers_directory(sdk):
    out = sdk.run(
        'out["cwd"] = str(CWD.resolve())',
        cwd=sdk.work / "app",
        relative=True,
    )
    assert out["cwd"] == str((sdk.work / "app").resolve())


def test_children_do_not_see_the_launcher_hand_off(sdk):
    out = sdk.run("""
        r = subprocess.run(
            ["sh", "-c", "echo ${ESCROW_INIT_CWD-unset}"], capture_output=True, text=True
        )
        out["child"] = r.stdout.strip()
        out["self"] = "ESCROW_INIT_CWD" in os.environ
    """)
    assert out == {"child": "unset", "self": False}


class _FakeDaemon(escrow_pb2_grpc.EscrowServiceServicer):
    def __init__(self, protocol_version):
        self.protocol_version = protocol_version

    def Ping(self, request, context):
        return escrow_pb2.PingResponse(
            daemon_version="fake", protocol_version=self.protocol_version
        )


def test_init_refuses_a_daemon_older_than_protocol_7_and_installs_no_wrappers(
    runtime_dir, tmp_path, monkeypatch
):
    sock = runtime_dir / "fake.sock"
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=2))
    escrow_pb2_grpc.add_EscrowServiceServicer_to_server(_FakeDaemon(6), server)
    server.add_insecure_port(f"unix:{sock}")
    server.start()
    for var in ("ESCROW_PROJECT", "ESCROW_UNSCOPED", "ESCROW_INIT_CWD"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("ESCROW_SOCKET", str(sock))
    before = builtins.open
    try:
        with pytest.raises(escrow.EscrowError, match="protocol 6") as raised:
            escrow.init(tmp_path, "passthrough")
    finally:
        server.stop(None)
    assert not isinstance(raised.value, escrow.EscrowRpcError)
    assert builtins.open is before


def test_init_accepts_the_real_daemon(sdk):
    out = sdk.run('out["ready"] = True')
    assert out == {"ready": True}
