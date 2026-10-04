"""gRPC client for the escrowd Unix socket."""

import os

import grpc

from escrow.v1 import escrow_pb2, escrow_pb2_grpc


class Client:
    def __init__(self, socket: str):
        self.socket = socket
        # grpcio sends the socket path as :authority, which tonic's HTTP/2 stack rejects
        # (RST_STREAM PROTOCOL_ERROR); any valid host name works.
        self._channel = grpc.insecure_channel(
            f"unix:{socket}", options=[("grpc.default_authority", "localhost")]
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
