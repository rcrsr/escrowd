"""The process-wide configuration `init()` sets and the wrappers read."""

from __future__ import annotations

import os
import shutil
import threading
from dataclasses import dataclass, field
from typing import Any

from escrow._client import Client
from escrow._errors import EscrowError


@dataclass
class _Config:
    project: str = ""
    socket: str = ""
    exe: str = ""
    views: str | None = None  # where scope views appear (/escrow under escrow run)
    unscoped: str = ""
    # The functions the wrappers replaced, by (owner, attribute); empty when not installed.
    orig: dict[tuple[Any, str], Any] = field(default_factory=dict)
    # The one client the SDK's own calls share, for `socket`; created on first use.
    client: Client | None = None


_cfg = _Config()
_client_lock = threading.Lock()


def _shared_client() -> Client:
    """The process's one client for `_cfg.socket`, created on first use. Clients from
    `escrow.connect()` are the caller's and never come from here."""
    with _client_lock:
        if _cfg.client is None:
            _cfg.client = Client(_cfg.socket)
        return _cfg.client


def _drop_client() -> None:
    """Close the shared client and forget it."""
    with _client_lock:
        c, _cfg.client = _cfg.client, None
    if c is not None:
        c.close()


def _forget_client_in_child() -> None:
    # Drop without closing: a gRPC channel must not be closed in a forked child. Another
    # thread may have held the lock at fork time, so replace it.
    global _client_lock
    _client_lock = threading.Lock()
    _cfg.client = None


os.register_at_fork(after_in_child=_forget_client_in_child)


def _escrow_exe() -> str:
    exe = os.environ.get("ESCROW_EXE") or shutil.which("escrow")
    if not exe:
        raise EscrowError("escrow binary not found: put it on PATH or set ESCROW_EXE")
    return exe
