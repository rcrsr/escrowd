"""The low-level client's connection helpers."""

import asyncio
import os

import grpc
import pytest

import escrow
from escrow import _patch, lowlevel


def test_connect_without_socket_raises_escrow_error(monkeypatch):
    monkeypatch.delenv("ESCROW_SOCKET", raising=False)
    for connect in (lowlevel.connect, lowlevel.connect_reviewer):
        with pytest.raises(escrow.EscrowError, match="no escrowd socket"):
            connect()


@pytest.fixture
def initialized(start_daemon, escrow_bin, monkeypatch):
    """`escrow.init()` against an implicit-mode daemon in this process, undone at teardown."""
    daemon = start_daemon(unscoped="implicit")
    monkeypatch.setenv("ESCROW_SOCKET", str(daemon.socket))
    monkeypatch.setenv("ESCROW_EXE", str(escrow_bin))
    monkeypatch.chdir(daemon.project)
    escrow.init(daemon.project, unscoped="implicit")
    yield daemon
    _patch._uninstall()


def test_internal_calls_share_one_channel_across_tasks(initialized, monkeypatch):
    channels = []
    real = grpc.insecure_channel

    def counting(*args, **kwargs):
        channels.append(args)
        return real(*args, **kwargs)

    monkeypatch.setattr(grpc, "insecure_channel", counting)

    async def work(name):
        async with escrow.scope(name) as s:
            (initialized.project / f"{name}.txt").write_text(name)
        await s.decided()

    async def main():
        await asyncio.gather(work("a"), work("b"))

    asyncio.run(main())
    escrow.settle_unscoped()

    assert len(channels) == 1


def test_fork_child_drops_the_shared_client_without_a_call(initialized):
    escrow.settle_unscoped()  # creates the shared client
    assert escrow._config._cfg.client is not None

    pid = os.fork()
    if pid == 0:
        os._exit(0 if escrow._config._cfg.client is None else 1)
    _, status = os.waitpid(pid, 0)

    assert os.waitstatus_to_exitcode(status) == 0
