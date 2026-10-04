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
