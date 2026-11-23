"""Scopes: the `Scope` context manager, deciding and awaiting a change set, and the
path mapping between the project and a scope's views."""

from __future__ import annotations

import asyncio
import contextvars
import inspect
import os
import pathlib
import warnings
import weakref

from escrow._client import Client
from escrow._config import _cfg, _shared_client
from escrow._errors import EscrowError, EscrowUnscopedWarning
from escrow._model import (
    ChangeSet,
    Decide,
    Decision,
    Outcome,
    commit,
    discard,
)

_current: contextvars.ContextVar[Scope | None] = contextvars.ContextVar(
    "escrow_scope", default=None
)


def settle_unscoped(decide: Decide | None = None, *, wait: bool = True) -> Outcome:
    """Close the implicit default scope and apply `decide` (default: commit) to it; a
    held change set is waited for unless `wait` is False."""
    c = _shared_client()
    cs = c.settle_unscoped()
    d = decide(cs) if decide else None
    if inspect.isawaitable(d):
        raise EscrowError("settle_unscoped: decide must be synchronous")
    out = _apply(c, cs, d, "", wait)
    return _await(out, "") if wait and out.status == "held" else out


def _apply(c: Client, cs: ChangeSet, d: Decision | None, token: str, wait: bool) -> Outcome:
    d = d or commit()
    out = c.decide(cs.scope_id, d.verdict, reasons=list(d.reasons), token=token, wait=wait)
    return Outcome.from_result(out, cs, d.reasons)


def _await(held: Outcome, token: str, timeout: float | None = None) -> Outcome:
    """Follow a held scope to its verdict (the daemon's AwaitDecision stream)."""
    last = None
    for last in _shared_client().await_decision(
        held.changes.scope_id, token=token, timeout=timeout
    ):
        if last.status != "held":
            break
    if last is None:
        raise EscrowError(f"scope {held.changes.scope_id}: no verdict")
    return Outcome.from_result(last, held.changes)


# ---- scopes ----


class Scope:
    """`with escrow.scope(...) as s:` or `async with …`: IO in the body is escrowed; at
    exit the scope's open files are fsynced, the scope closes, `decide` (default: commit)
    gets the change set and `s.outcome` holds the result. An exception in the body
    discards the scope and propagates.

    `decide`'s verdict is a proposal: the policy's rules only tighten it, and a commit
    of a change set that needs reviewers is held. With `wait` (the default) the exit
    returns after their verdict; without, `s.outcome.status` is "held" and
    `s.wait_decided()` or `await s.decided()` gives the verdict. `session` orders an
    agent's scopes: a scope of the session does not open while another is held with a
    wait (`s.outcome.wait`)."""

    def __init__(
        self,
        name: str = "",
        *,
        decide: Decide | None = None,
        labels: dict[str, str] | None = None,
        resume: Scope | None = None,
        session: str | None = None,
        wait: bool = True,
    ):
        if not _cfg.socket:
            raise EscrowError("call escrow.init() first")
        self.name, self.labels, self.decide = name, labels or {}, decide
        self.session = session if session is not None else resume.session if resume else ""
        self.wait = wait
        self.id = resume.id if resume else ""
        # The scope's capability; never put in a path, a label or a child's environment.
        self._scope_token = resume._scope_token if resume else ""
        self.root = resume.root if resume else ""
        # (host path, view, direct paths) of each served root outside the project.
        self.roots: list[tuple[str, str, tuple[str, ...]]] = resume.roots if resume else []
        self.outcome: Outcome | None = None
        self.closed = False
        self._files: weakref.WeakSet = weakref.WeakSet()
        self._ctx: contextvars.Token | None = None

    def _enter(self) -> Scope:
        if not self.id:
            s = _shared_client().open_scope(
                self.name, self.labels, timeout=None, session=self.session
            )
            self.id = s.scope_id
            self._scope_token = s.token
            self.root = _view(s.root)
            self.roots = [(r.path, _view(r.view), r.direct) for r in s.roots]
        self.closed = False
        self._ctx = _current.set(self)
        return self

    def _close(self) -> ChangeSet:
        if self._ctx is not None:
            _current.reset(self._ctx)
            self._ctx = None
        self.flush()
        cs = _shared_client().close_scope(self.id, self._scope_token)
        self.closed = True
        if cs.unscoped:
            warnings.warn(
                f"escrow: {cs.unscoped} change(s) reached the unscoped mode "
                f"({_cfg.unscoped}) while scope {self.id} was open",
                EscrowUnscopedWarning,
                stacklevel=3,
            )
        return cs

    def _propose(self, cs: ChangeSet, d: Decision | None) -> None:
        # The working directory is per process: once decided (or held: closed), move it
        # out of the scope's views to the same path in the project.
        cwd = _cfg.orig[(os, "getcwd")]()
        back = _unmap_from(self, cwd)
        self.outcome = _apply(_shared_client(), cs, d, self._scope_token, self.wait)
        if back != cwd:
            _chdir_nearest(back)

    def wait_decided(self, timeout: float | None = None) -> Outcome:
        """The scope's verdict: at once unless it is held, else after its reviewers."""
        if self.outcome is None:
            raise EscrowError(f"scope {self.id} is not decided: it is still open")
        if self.outcome.status == "held":
            self.outcome = _await(self.outcome, self._scope_token, timeout)
        return self.outcome

    async def decided(self, timeout: float | None = None) -> Outcome:
        """`wait_decided` without blocking the event loop."""
        return await asyncio.to_thread(self.wait_decided, timeout)

    def __enter__(self) -> Scope:
        return self._enter()

    def __exit__(self, exc_type, exc, tb) -> None:
        cs = self._close()
        if exc is not None:
            return self._propose(cs, discard(f"{exc_type.__name__}: {exc}"))
        d = self.decide(cs) if self.decide else None
        if inspect.isawaitable(d):
            raise EscrowError("a sync scope needs a sync decide; use `async with`")
        self._propose(cs, d)
        if self.wait:
            self.wait_decided()

    async def __aenter__(self) -> Scope:
        return self._enter()

    async def __aexit__(self, exc_type, exc, tb) -> None:
        cs = self._close()
        if exc is not None:
            return self._propose(cs, discard(f"{exc_type.__name__}: {exc}"))
        d = self.decide(cs) if self.decide else None
        if inspect.isawaitable(d):
            d = await d
        self._propose(cs, d)
        if self.wait:
            await self.decided()

    def flush(self) -> None:
        """Push every dirty page of the scope to the daemon: fsync its open files
        (syncfs is not a barrier on plain FUSE)."""
        for f in list(self._files):
            if not f.closed and f.writable():
                f.flush()
                os.fsync(f.fileno())

    def path(self, p: str | os.PathLike) -> str:
        """Where a project path (or a path in a served root) lives in this scope's views."""
        return _rewrite_to(self, os.fspath(p))


scope = Scope


def _chdir_nearest(path: str) -> None:
    """chdir to `path`, or to its nearest existing ancestor (a discard drops new dirs)."""
    for d in (path, *map(str, pathlib.Path(path).parents)):
        try:
            return _cfg.orig[(os, "chdir")](d)
        except OSError:
            continue


def current() -> Scope | None:
    return _current.get()


# ---- the wrappers ----


def _view(path: str) -> str:
    """A view's path as this process sees it (under /escrow in `escrow run`'s sandbox)."""
    return os.path.join(_cfg.views, os.path.basename(path)) if _cfg.views else path


def _under(p: str, root: str) -> bool:
    return p == root or p.startswith(root.rstrip(os.sep) + os.sep)


def _move(p: str, src: str, dst: str) -> str:
    rel = os.path.relpath(p, src)
    return dst if rel == "." else os.path.join(dst, rel)


def _rewrite_to(s: Scope, p):
    if isinstance(p, bytes):
        return os.fsencode(_rewrite_to(s, os.fsdecode(p)))
    a = os.path.abspath(p)
    # The project first: it may lie inside $HOME.
    if _under(a, _cfg.project):
        return _move(a, _cfg.project, s.root)
    for host, view, direct in s.roots:
        if _under(a, host) and not any(_under(a, d) for d in direct):
            return _move(a, host, view)
    return p


def _rewrite(path, dir_fd=None):
    s = _current.get()
    if s is None or dir_fd is not None or isinstance(path, int):
        return path
    return _rewrite_to(s, os.fspath(path))


def _unmap(p):
    """A path in the current scope's views, back to the path the code asked for."""
    s = _current.get()
    return p if s is None else _unmap_from(s, p)


def _unmap_from(s: Scope, p):
    if isinstance(p, bytes):
        return p
    for view, host in ((s.root, _cfg.project), *((v, h) for h, v, _ in s.roots)):
        if _under(p, view):
            return os.path.normpath(_move(p, view, host))
    return p
