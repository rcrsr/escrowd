"""gRPC client for the escrowd Unix socket."""

from __future__ import annotations

import os

import grpc

from escrow.v1 import escrow_pb2, escrow_pb2_grpc


class Client:
    def __init__(self, socket: str):
        self.socket = socket
        # grpcio sends the socket path as :authority, which tonic's HTTP/2 stack rejects
        # (RST_STREAM PROTOCOL_ERROR); any valid host name works. A change set's diff
        # can pass grpcio's 4 MiB receive limit (policy diff.max_bytes): no limit.
        self._channel = grpc.insecure_channel(
            f"unix:{socket}",
            options=[
                ("grpc.default_authority", "localhost"),
                ("grpc.max_receive_message_length", -1),
            ],
        )
        self._stub = escrow_pb2_grpc.EscrowStub(self._channel)

    def ping(self, timeout: float = 5.0) -> escrow_pb2.PingResponse:
        return self._stub.Ping(escrow_pb2.PingRequest(), timeout=timeout)

    def open_scope(
        self, name: str = "", labels: dict[str, str] | None = None, timeout: float = 5.0
    ) -> escrow_pb2.OpenScopeResponse:
        req = escrow_pb2.OpenScopeRequest(name=name, labels=labels or {})
        return self._stub.OpenScope(req, timeout=timeout)

    def close_scope(self, scope_id: str, timeout: float = 30.0) -> escrow_pb2.ChangeSet:
        """Freeze the scope and return its change set. Fsync the scope's open files first."""
        return self._stub.CloseScope(
            escrow_pb2.CloseScopeRequest(scope_id=scope_id), timeout=timeout
        )

    def decide(
        self,
        scope_id: str,
        verdict: escrow_pb2.Verdict,
        reasons: list[str] | None = None,
        timeout: float = 30.0,
    ) -> escrow_pb2.Outcome:
        req = escrow_pb2.DecideRequest(scope_id=scope_id, verdict=verdict, reasons=reasons or [])
        return self._stub.Decide(req, timeout=timeout)

    def commit(self, scope_id: str, timeout: float = 60.0) -> escrow_pb2.Outcome:
        """Apply a closed scope's change set, all or nothing; a conflict drops the scope."""
        return self.decide(scope_id, escrow_pb2.VERDICT_COMMIT, timeout=timeout)

    def settle_unscoped(self, timeout: float = 30.0) -> escrow_pb2.ChangeSet:
        """Close the implicit default scope and return its change set (scope id "unscoped")."""
        return self._stub.SettleUnscoped(escrow_pb2.SettleUnscopedRequest(), timeout=timeout)

    def get_change_set(self, scope_id: str, timeout: float = 30.0) -> escrow_pb2.ChangeSet:
        """A closed, undecided scope's change set and diff; an open scope fails."""
        return self._stub.GetChangeSet(
            escrow_pb2.GetChangeSetRequest(scope_id=scope_id), timeout=timeout
        )

    def discard(self, scope_id: str, timeout: float = 30.0) -> escrow_pb2.Outcome:
        return self.decide(scope_id, escrow_pb2.VERDICT_DISCARD, timeout=timeout)

    def close(self) -> None:
        self._channel.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def connect(socket: str | None = None) -> Client:
    """Connect to the daemon at `socket`, or at $ESCROW_SOCKET."""
    path = socket or os.environ.get("ESCROW_SOCKET")
    if not path:
        raise RuntimeError("no escrowd socket: pass one or set ESCROW_SOCKET")
    return Client(path)
