"""gRPC client for the escrowd Unix socket."""

from __future__ import annotations

import os
from collections.abc import Iterator

import grpc

from escrow.v1 import escrow_pb2, escrow_pb2_grpc


def _channel(socket: str) -> grpc.Channel:
    # grpcio sends the socket path as :authority, which tonic's HTTP/2 stack rejects
    # (RST_STREAM PROTOCOL_ERROR); any valid host name works. A change set's diff
    # can pass grpcio's 4 MiB receive limit (policy diff.max_bytes): no limit.
    return grpc.insecure_channel(
        f"unix:{socket}",
        options=[
            ("grpc.default_authority", "localhost"),
            ("grpc.max_receive_message_length", -1),
        ],
    )


class Client:
    """One connection. Closing, deciding and spawning in a scope take the scope's token
    (`OpenScopeResponse.token`); a client remembers the tokens of the scopes it opened
    and sends them when no `token` is passed. The unscoped scope needs none."""

    def __init__(self, socket: str):
        self.socket = socket
        self.tokens: dict[str, str] = {}
        self._channel = _channel(socket)
        self._stub = escrow_pb2_grpc.EscrowStub(self._channel)

    def ping(self, timeout: float = 5.0) -> escrow_pb2.PingResponse:
        return self._stub.Ping(escrow_pb2.PingRequest(), timeout=timeout)

    def open_scope(
        self,
        name: str = "",
        labels: dict[str, str] | None = None,
        timeout: float | None = 5.0,
        session: str = "",
    ) -> escrow_pb2.OpenScopeResponse:
        """In a `session` with a scope held with a wait, the call waits for its verdict
        up to `timeout` (None: no limit), then fails with FAILED_PRECONDITION."""
        req = escrow_pb2.OpenScopeRequest(name=name, labels=labels or {}, session=session)
        resp = self._stub.OpenScope(req, timeout=timeout)
        self.tokens[resp.scope_id] = resp.token
        return resp

    def _token(self, scope_id: str, token: str | None) -> str:
        return self.tokens.get(scope_id, "") if token is None else token

    def close_scope(
        self, scope_id: str, token: str | None = None, timeout: float = 30.0
    ) -> escrow_pb2.ChangeSet:
        """Freeze the scope and return its change set. Fsync the scope's open files first."""
        req = escrow_pb2.CloseScopeRequest(scope_id=scope_id, token=self._token(scope_id, token))
        return self._stub.CloseScope(req, timeout=timeout)

    def decide(
        self,
        scope_id: str,
        verdict: escrow_pb2.Verdict,
        reasons: list[str] | None = None,
        token: str | None = None,
        timeout: float = 30.0,
        wait: bool = False,
    ) -> escrow_pb2.Outcome:
        """A commit that needs reviewers returns OUTCOME_STATUS_HELD; `wait`: the
        session's next scope waits for the verdict."""
        req = escrow_pb2.DecideRequest(
            scope_id=scope_id,
            verdict=verdict,
            reasons=reasons or [],
            token=self._token(scope_id, token),
            wait=wait,
        )
        return self._stub.Decide(req, timeout=timeout)

    def commit(
        self, scope_id: str, token: str | None = None, timeout: float = 60.0, wait: bool = False
    ) -> escrow_pb2.Outcome:
        """Apply a closed scope's change set, all or nothing. A conflict drops the scope,
        or reopens it under the policy's `conflict.verdict: return`; a change set that
        needs reviewers is held."""
        return self.decide(
            scope_id, escrow_pb2.VERDICT_COMMIT, token=token, timeout=timeout, wait=wait
        )

    def settle_unscoped(self, timeout: float = 30.0) -> escrow_pb2.ChangeSet:
        """Close the implicit default scope and return its change set (scope id "unscoped")."""
        return self._stub.SettleUnscoped(escrow_pb2.SettleUnscopedRequest(), timeout=timeout)

    def get_change_set(self, scope_id: str, timeout: float = 30.0) -> escrow_pb2.ChangeSet:
        """A closed, undecided scope's change set and diff; an open scope fails."""
        return self._stub.GetChangeSet(
            escrow_pb2.GetChangeSetRequest(scope_id=scope_id), timeout=timeout
        )

    def discard(
        self, scope_id: str, token: str | None = None, timeout: float = 30.0
    ) -> escrow_pb2.Outcome:
        return self.decide(scope_id, escrow_pb2.VERDICT_DISCARD, token=token, timeout=timeout)

    def await_decision(
        self, scope_id: str, token: str | None = None, timeout: float | None = None
    ) -> Iterator[escrow_pb2.Outcome]:
        """Follow a held scope: OUTCOME_STATUS_HELD after each tier's review, then the
        final outcome. A scope decided already yields its last outcome."""
        req = escrow_pb2.AwaitDecisionRequest(scope_id=scope_id, token=self._token(scope_id, token))
        return self._stub.AwaitDecision(req, timeout=timeout)

    def close(self) -> None:
        self._channel.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc) -> None:
        self.close()


class Reviewer:
    """A reviewer's connection, on the daemon's review socket (`<socket>.review`)."""

    def __init__(self, socket: str):
        self.socket = socket
        self._channel = _channel(socket)
        self._stub = escrow_pb2_grpc.ReviewerStub(self._channel)

    def list_held(self, timeout: float = 5.0) -> list[escrow_pb2.HeldScope]:
        return list(self._stub.ListHeld(escrow_pb2.ListHeldRequest(), timeout=timeout).scopes)

    def get_held(self, scope_id: str, timeout: float = 30.0) -> escrow_pb2.GetHeldResponse:
        return self._stub.GetHeld(escrow_pb2.GetHeldRequest(scope_id=scope_id), timeout=timeout)

    def review(
        self,
        scope_id: str,
        tier: escrow_pb2.Tier,
        verdict: escrow_pb2.Verdict,
        reasons: list[str] | None = None,
        override: bool = False,
        timeout: float = 60.0,
    ) -> escrow_pb2.Outcome:
        """`tier`'s verdict; after the last pending tier the scope is decided."""
        req = escrow_pb2.ReviewRequest(
            scope_id=scope_id,
            tier=tier,
            verdict=verdict,
            reasons=reasons or [],
            override=override,
        )
        return self._stub.Review(req, timeout=timeout)

    def close(self) -> None:
        self._channel.close()

    def __enter__(self) -> Reviewer:
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def connect_reviewer(socket: str | None = None) -> Reviewer:
    """Connect to the review socket of the daemon at `socket`, or at $ESCROW_SOCKET."""
    path = socket or os.environ.get("ESCROW_SOCKET")
    if not path:
        raise RuntimeError("no escrowd socket: pass one or set ESCROW_SOCKET")
    return Reviewer(path + ".review")


def connect(socket: str | None = None) -> Client:
    """Connect to the daemon at `socket`, or at $ESCROW_SOCKET."""
    path = socket or os.environ.get("ESCROW_SOCKET")
    if not path:
        raise RuntimeError("no escrowd socket: pass one or set ESCROW_SOCKET")
    return Client(path)
