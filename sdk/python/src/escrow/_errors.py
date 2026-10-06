"""Typed errors: one class per status code the daemon returns (docs/protocol.md, "Status
codes")."""

from __future__ import annotations

from typing import cast

import grpc


class EscrowError(Exception):
    pass


class EscrowRpcError(EscrowError, grpc.RpcError):
    """A call to the daemon failed. `code()` and `details()` as on a grpc.RpcError; a
    code without a class of its own (INTERNAL, UNKNOWN, CANCELLED) raises this class."""

    def __init__(self, code: grpc.StatusCode, details: str):
        super().__init__(details)
        self._code, self._details = code, details

    def code(self) -> grpc.StatusCode:
        return self._code

    def details(self) -> str:
        return self._details


class EscrowNotFoundError(EscrowRpcError):
    """NOT_FOUND: no scope with that id (never opened, or decided and dropped)."""


class EscrowPermissionError(EscrowRpcError):
    """PERMISSION_DENIED: a missing or wrong scope token; an opener's commit or return
    of a held scope; a reviewer's looser verdict without a human override."""


class EscrowStateError(EscrowRpcError):
    """FAILED_PRECONDITION: the scope is not in the state the call needs (open, closed,
    held, the tier's turn), the daemon's mode does not serve it, or a session's next
    scope reached the deadline waiting behind a held one."""


class EscrowInvalidArgumentError(EscrowRpcError):
    """INVALID_ARGUMENT: a request field is missing or out of range."""


class EscrowAbortedError(EscrowRpcError):
    """ABORTED: a commit failed and was rolled back; nothing reached the project. The
    scope stays closed: deciding it again retries."""


class EscrowUnavailableError(EscrowRpcError):
    """UNAVAILABLE: no daemon answers on the socket, or it stopped during the call."""


class EscrowTimeoutError(EscrowRpcError):
    """DEADLINE_EXCEEDED: the call's timeout passed. A decision may still have run."""


class EscrowUnsupportedError(EscrowRpcError):
    """UNIMPLEMENTED: the socket does not serve the call (a reviewer call on the main
    socket, or the reverse) or the daemon predates it."""


_BY_CODE: dict[grpc.StatusCode, type[EscrowRpcError]] = {
    grpc.StatusCode.NOT_FOUND: EscrowNotFoundError,
    grpc.StatusCode.PERMISSION_DENIED: EscrowPermissionError,
    grpc.StatusCode.FAILED_PRECONDITION: EscrowStateError,
    grpc.StatusCode.INVALID_ARGUMENT: EscrowInvalidArgumentError,
    grpc.StatusCode.ABORTED: EscrowAbortedError,
    grpc.StatusCode.UNAVAILABLE: EscrowUnavailableError,
    grpc.StatusCode.DEADLINE_EXCEEDED: EscrowTimeoutError,
    grpc.StatusCode.UNIMPLEMENTED: EscrowUnsupportedError,
}


def typed(e: grpc.RpcError) -> EscrowRpcError:
    """The typed error for a failed call (a grpc.Call)."""
    call = cast(grpc.Call, e)
    code = call.code()
    return _BY_CODE.get(code, EscrowRpcError)(code, call.details() or "")
