"""The public client: `lowlevel`'s calls returning the SDK's model types, with verdicts
and tiers as strings. A failed call raises the typed error of its status code."""

from __future__ import annotations

from collections.abc import Iterator, MutableMapping

from escrow import lowlevel
from escrow._model import (
    NO_TIER,
    ChangeSet,
    DaemonInfo,
    HeldDetail,
    HeldScope,
    OpenedScope,
    Result,
    Tier,
    Verdict,
    reviewer_name,
    tier_value,
    verdict_value,
)


class Client:
    """One connection. Closing, deciding and spawning in a scope take the scope's token
    (`OpenedScope.token`); a client remembers the tokens of the scopes it opened and
    sends them when no `token` is passed. The unscoped scope needs none. Clients given
    the same `tokens` mapping share their tokens: it is read and recorded into."""

    def __init__(self, socket: str, tokens: MutableMapping[str, str] | None = None):
        self._low = lowlevel.Client(socket, tokens)

    @property
    def socket(self) -> str:
        return self._low.socket

    @property
    def tokens(self) -> MutableMapping[str, str]:
        return self._low.tokens

    def ping(self, timeout: float = 5.0) -> DaemonInfo:
        return DaemonInfo.from_proto(self._low.ping(timeout))

    def open_scope(
        self,
        name: str = "",
        labels: dict[str, str] | None = None,
        timeout: float | None = 5.0,
        session: str = "",
    ) -> OpenedScope:
        """In a `session` with a scope held with a wait, the call waits for its verdict
        up to `timeout` (None: no limit), then fails with `EscrowStateError`."""
        return OpenedScope.from_proto(self._low.open_scope(name, labels, timeout, session))

    def close_scope(
        self, scope_id: str, token: str | None = None, timeout: float = 30.0
    ) -> ChangeSet:
        """Freeze the scope and return its change set. Fsync the scope's open files first."""
        return ChangeSet.from_proto(self._low.close_scope(scope_id, token, timeout))

    def decide(
        self,
        scope_id: str,
        verdict: Verdict,
        reasons: list[str] | None = None,
        token: str | None = None,
        timeout: float = 30.0,
        wait: bool = False,
    ) -> Result:
        """A commit that needs reviewers returns status "held"; `wait`: the session's
        next scope waits for the verdict. An unknown `verdict` raises ValueError."""
        value = verdict_value(verdict)
        return Result.from_proto(
            self._low.decide(scope_id, value, reasons, token, timeout=timeout, wait=wait)
        )

    def commit(
        self, scope_id: str, token: str | None = None, timeout: float = 60.0, wait: bool = False
    ) -> Result:
        """Apply a closed scope's change set, all or nothing. A conflict drops the scope,
        or reopens it under the policy's `conflict.verdict: return`; a change set that
        needs reviewers is held."""
        return Result.from_proto(self._low.commit(scope_id, token, timeout, wait))

    def settle_unscoped(self, timeout: float = 30.0) -> ChangeSet:
        """Close the implicit default scope and return its change set (scope id "unscoped")."""
        return ChangeSet.from_proto(self._low.settle_unscoped(timeout))

    def get_change_set(self, scope_id: str, timeout: float = 30.0) -> ChangeSet:
        """A closed, undecided scope's change set and diff; an open scope fails."""
        return ChangeSet.from_proto(self._low.get_change_set(scope_id, timeout))

    def discard(self, scope_id: str, token: str | None = None, timeout: float = 30.0) -> Result:
        return Result.from_proto(self._low.discard(scope_id, token, timeout))

    def await_decision(
        self, scope_id: str, token: str | None = None, timeout: float | None = None
    ) -> Iterator[Result]:
        """Follow a held scope: status "held" after each tier's review, then the final
        result. A scope decided already yields its last result."""
        for o in self._low.await_decision(scope_id, token, timeout):
            yield Result.from_proto(o)

    def close(self) -> None:
        self._low.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc) -> None:
        self.close()


class Reviewer:
    """A reviewer's connection, on the daemon's review socket (`<socket>.review`)."""

    def __init__(self, socket: str):
        self._low = lowlevel.Reviewer(socket)

    @property
    def socket(self) -> str:
        return self._low.socket

    def list_held(self, timeout: float = 5.0) -> list[HeldScope]:
        return [HeldScope.from_proto(h) for h in self._low.list_held(timeout)]

    def get_held(self, scope_id: str, timeout: float = 30.0) -> HeldDetail:
        return HeldDetail.from_proto(self._low.get_held(scope_id, timeout))

    def review(
        self,
        scope_id: str,
        tier: Tier,
        verdict: Verdict,
        reasons: list[str] | None = None,
        override: bool = False,
        timeout: float = 60.0,
    ) -> Result:
        """`tier`'s verdict; after the last pending tier the scope is decided. An unknown
        `tier` or `verdict` raises ValueError."""
        t, v = tier_value(tier), verdict_value(verdict)
        return Result.from_proto(self._low.review(scope_id, t, v, reasons, override, timeout))

    def review_as(
        self,
        scope_id: str,
        reviewer: str,
        verdict: Verdict,
        *,
        reasons: list[str] | None = None,
        override: bool = False,
        token: str | None = None,
        timeout: float = 60.0,
    ) -> Result:
        """`reviewer`'s verdict by name (a declared reviewer, or "llm" or "human"); `token`
        is a delegated reviewer's one-time token. An invalid `reviewer` or an unknown
        `verdict` raises ValueError."""
        name, v = reviewer_name(reviewer), verdict_value(verdict)
        out = self._low.review(
            scope_id,
            NO_TIER,  # the name decides
            v,
            reasons,
            override,
            timeout,
            reviewer=name,
            token=token or "",
        )
        return Result.from_proto(out)

    def close(self) -> None:
        self._low.close()

    def __enter__(self) -> Reviewer:
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def connect(socket: str | None = None) -> Client:
    """Connect to the daemon at `socket`, or at $ESCROW_SOCKET."""
    return Client(lowlevel.socket_path(socket))


def connect_reviewer(socket: str | None = None) -> Reviewer:
    """Connect to the review socket of the daemon at `socket`, or at $ESCROW_SOCKET."""
    return Reviewer(lowlevel.socket_path(socket) + ".review")
