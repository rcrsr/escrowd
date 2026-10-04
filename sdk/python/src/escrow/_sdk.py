"""Scopes for Python programs.

`init()` puts the program under `escrow run` (re-executing it if needed) and wraps the
file API: inside a scope, `open`, `io.open`, the `os` path functions (and so pathlib,
shutil and os.walk) rewrite `<project>/x` to the scope's view, and `subprocess.Popen`
(and so `subprocess.run` and asyncio subprocesses) runs the child through `escrow exec`
in the scope's sandbox. The current scope lives in a ContextVar, so concurrent asyncio
tasks on one thread keep separate scopes.

Not covered (falls to the unscoped mode): `os.system`, `os.posix_spawn`, `os.chdir` into
the project (the working directory is per process, not per scope), native code doing
its own IO, and file descriptors beyond stdin, stdout and stderr passed to children.
"""

from __future__ import annotations

import builtins
import contextvars
import functools
import inspect
import io
import os
import pathlib
import shutil
import subprocess
import sys
import weakref
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field

from escrow._client import connect
from escrow.v1 import escrow_pb2 as pb


class EscrowError(Exception):
    pass


# ---- decisions and outcomes ----


@dataclass(frozen=True)
class Decision:
    verdict: int
    reasons: tuple[str, ...] = ()


def commit() -> Decision:
    """Apply the scope's change set to the project, all or nothing."""
    return Decision(pb.VERDICT_COMMIT)


def discard(*reasons: str) -> Decision:
    """Drop the scope's changes."""
    return Decision(pb.VERDICT_DISCARD, reasons)


def send_back(*reasons: str) -> Decision:
    """Return to agent: the reasons go back, the scope stays open for a fix (`resume=`)."""
    return Decision(pb.VERDICT_RETURN, reasons)


_KINDS = {
    pb.CHANGE_KIND_CREATE: "create",
    pb.CHANGE_KIND_MODIFY: "modify",
    pb.CHANGE_KIND_DELETE: "delete",
    pb.CHANGE_KIND_RENAME: "rename",
}
_STATUS = {
    pb.OUTCOME_STATUS_COMMITTED: "committed",
    pb.OUTCOME_STATUS_DISCARDED: "discarded",
    pb.OUTCOME_STATUS_RETURNED: "returned",
    pb.OUTCOME_STATUS_CONFLICT: "conflict",
}


@dataclass(frozen=True)
class Change:
    kind: str  # create, modify, delete, rename
    path: str  # project-relative; for a rename, the destination
    from_path: str | None = None


@dataclass(frozen=True)
class Read:
    path: str
    allowed: bool


@dataclass(frozen=True)
class ChangeSet:
    """What a closed scope would do to the project, and what it read."""

    scope_id: str
    changes: list[Change]
    reads: list[Read]
    labels: dict[str, str]

    @property
    def paths(self) -> list[str]:
        return [c.path for c in self.changes]

    @classmethod
    def from_proto(cls, cs: pb.ChangeSet) -> ChangeSet:
        return cls(
            scope_id=cs.scope_id,
            changes=[Change(_KINDS[c.kind], c.path, c.from_path or None) for c in cs.changes],
            reads=[Read(r.path, r.decision == pb.READ_DECISION_ALLOW) for r in cs.reads],
            labels=dict(cs.labels),
        )


@dataclass(frozen=True)
class Outcome:
    status: str  # committed, discarded, returned, conflict
    paths: list[str]  # changed paths, or the conflicting ones
    reasons: list[str]
    changes: ChangeSet

    @property
    def reads(self) -> list[Read]:
        return self.changes.reads

    @property
    def labels(self) -> dict[str, str]:
        return self.changes.labels


Decide = Callable[[ChangeSet], "Decision | None | Awaitable[Decision | None]"]


# ---- configuration ----


@dataclass
class _Config:
    project: str = ""
    socket: str = ""
    exe: str = ""
    views: str | None = None  # where scope views appear (/escrow under escrow run)
    unscoped: str = ""
    orig: dict = field(default_factory=dict)


_cfg = _Config()
_current: contextvars.ContextVar[Scope | None] = contextvars.ContextVar(
    "escrow_scope", default=None
)


def _escrow_exe() -> str:
    exe = os.environ.get("ESCROW_EXE") or shutil.which("escrow")
    if not exe:
        raise EscrowError("escrow binary not found: put it on PATH or set ESCROW_EXE")
    return exe


def _symlinks_on_the_way(path: str) -> list[str]:
    """Every symlink met while resolving `path` (itself or an ancestor, hop by hop): the
    sandbox must show those paths too, not only the resolved target."""
    out: list[str] = []
    p = os.path.abspath(path)
    for _ in range(40):
        for a in (p, *map(str, pathlib.Path(p).parents)):
            if os.path.islink(a) and a not in out:
                out.append(a)
        if not os.path.islink(p):
            break
        p = os.path.normpath(os.path.join(os.path.dirname(p), os.readlink(p)))
    return out


def _read_paths(extra) -> list[str]:
    """What the sandbox must show of the host for this interpreter to run."""
    paths = [sys.prefix, sys.base_prefix, sys.exec_prefix]
    paths += [os.path.dirname(os.path.realpath(sys.executable))]
    paths += [p for p in sys.path if p and os.path.isabs(p)]
    paths += [os.fspath(p) for p in extra]
    out: list[str] = []
    for p in map(os.path.realpath, paths):
        if os.path.exists(p) and p != "/" and p not in out:
            out.append(p)
    # A symlink inside a bound path needs nothing more (bwrap cannot mount over it).
    for link in _symlinks_on_the_way(sys.executable):
        if not any(link.startswith(p + os.sep) for p in out):
            out.append(link)
    return out


def init(
    project: str | os.PathLike,
    unscoped: str,
    policy: str | os.PathLike | None = None,
    *,
    on_exit: str | None = None,
    read: tuple = (),
) -> None:
    """Run this program under escrowd. Without `ESCROW_SOCKET`, re-execute it under
    `escrow run` (this call does not return); with it, check that the launcher serves
    `project` in the `unscoped` mode (passthrough, implicit or deny) and install the
    wrappers. `on_exit` (commit or discard) settles implicit unscoped IO; `read` adds
    host paths the sandbox may read (the interpreter and sys.path are added for you)."""
    project = os.path.realpath(project)
    socket = os.environ.get("ESCROW_SOCKET")
    if not socket:
        exe = _escrow_exe()
        args = [exe, "run", "--project", project, "--unscoped", unscoped]
        if policy:
            args += ["--policy", os.path.realpath(policy)]
        if on_exit:
            args += ["--on-exit", on_exit]
        for p in _read_paths(read):
            args += ["--read", p]
        sys.stdout.flush()
        sys.stderr.flush()
        os.execv(exe, [*args, "--", sys.executable, *sys.orig_argv[1:]])
    for var, want in (("ESCROW_PROJECT", project), ("ESCROW_UNSCOPED", unscoped)):
        got = os.environ.get(var)
        if got and got != want:
            raise EscrowError(f"escrow run serves {var}={got}, init() asked for {want}")
    _install(project, socket, os.environ.get("ESCROW_VIEWS"), unscoped)


def settle_unscoped(decide: Decide | None = None) -> Outcome:
    """Close the implicit default scope and apply `decide` (default: commit) to it."""
    with connect(_cfg.socket) as c:
        cs = ChangeSet.from_proto(c.settle_unscoped())
        d = decide(cs) if decide else None
        if inspect.isawaitable(d):
            raise EscrowError("settle_unscoped: decide must be synchronous")
        return _apply(c, cs, d)


def _apply(c, cs: ChangeSet, d: Decision | None) -> Outcome:
    d = d or commit()
    out = c.decide(cs.scope_id, d.verdict, reasons=list(d.reasons))
    reasons = list(out.reasons) or list(d.reasons)
    return Outcome(_STATUS[out.status], list(out.paths), reasons, cs)


# ---- scopes ----


class Scope:
    """`with escrow.scope(...) as s:` or `async with …`: IO in the body is escrowed; at
    exit the scope's open files are fsynced, the scope closes, `decide` (default: commit)
    gets the change set and `s.outcome` holds the result. An exception in the body
    discards the scope and propagates."""

    def __init__(
        self,
        name: str = "",
        *,
        decide: Decide | None = None,
        labels: dict[str, str] | None = None,
        resume: Scope | None = None,
    ):
        if not _cfg.socket:
            raise EscrowError("call escrow.init() first")
        self.name, self.labels, self.decide = name, labels or {}, decide
        self.id = resume.id if resume else ""
        self.root = resume.root if resume else ""
        self.outcome: Outcome | None = None
        self._files: weakref.WeakSet = weakref.WeakSet()
        self._token: contextvars.Token | None = None

    def _enter(self) -> Scope:
        if not self.id:
            with connect(_cfg.socket) as c:
                s = c.open_scope(self.name, self.labels)
            self.id = s.scope_id
            self.root = os.path.join(_cfg.views, s.scope_id) if _cfg.views else s.root
        self._token = _current.set(self)
        return self

    def _close(self) -> ChangeSet:
        if self._token is not None:
            _current.reset(self._token)
            self._token = None
        self.flush()
        with connect(_cfg.socket) as c:
            return ChangeSet.from_proto(c.close_scope(self.id))

    def _finish(self, cs: ChangeSet, d: Decision | None) -> None:
        with connect(_cfg.socket) as c:
            self.outcome = _apply(c, cs, d)

    def __enter__(self) -> Scope:
        return self._enter()

    def __exit__(self, exc_type, exc, tb) -> None:
        cs = self._close()
        if exc is not None:
            return self._finish(cs, discard(f"{exc_type.__name__}: {exc}"))
        d = self.decide(cs) if self.decide else None
        if inspect.isawaitable(d):
            raise EscrowError("a sync scope needs a sync decide; use `async with`")
        self._finish(cs, d)

    async def __aenter__(self) -> Scope:
        return self._enter()

    async def __aexit__(self, exc_type, exc, tb) -> None:
        cs = self._close()
        if exc is not None:
            return self._finish(cs, discard(f"{exc_type.__name__}: {exc}"))
        d = self.decide(cs) if self.decide else None
        if inspect.isawaitable(d):
            d = await d
        self._finish(cs, d)

    def flush(self) -> None:
        """Push every dirty page of the scope to the daemon: fsync its open files
        (syncfs is not a barrier on plain FUSE)."""
        for f in list(self._files):
            if not f.closed and f.writable():
                f.flush()
                os.fsync(f.fileno())

    def path(self, p: str | os.PathLike) -> str:
        """Where a project path lives in this scope's view."""
        return _rewrite_to(self.root, os.fspath(p))


scope = Scope


def current() -> Scope | None:
    return _current.get()


# ---- the wrappers ----


def _rewrite_to(root: str, p):
    if isinstance(p, bytes):
        return os.fsencode(_rewrite_to(root, os.fsdecode(p)))
    a = os.path.abspath(p)
    if a == _cfg.project or a.startswith(_cfg.project + os.sep):
        return os.path.join(root, os.path.relpath(a, _cfg.project))
    return p


def _rewrite(path, dir_fd=None):
    s = _current.get()
    if s is None or dir_fd is not None or isinstance(path, int):
        return path
    return _rewrite_to(s.root, os.fspath(path))


def _unmap(p):
    """A path in the current scope's view, back to the project path the code asked for."""
    s = _current.get()
    if s is None or isinstance(p, bytes):
        return p
    if p == s.root or p.startswith(s.root + os.sep):
        return os.path.normpath(os.path.join(_cfg.project, os.path.relpath(p, s.root)))
    return p


_WRAP1 = (
    "open",
    "stat",
    "lstat",
    "listdir",
    "scandir",
    "mkdir",
    "makedirs",
    "remove",
    "unlink",
    "rmdir",
    "removedirs",
    "chmod",
    "chown",
    "lchown",
    "utime",
    "truncate",
    "access",
    "readlink",
    "mkfifo",
)
_WRAP2 = ("rename", "replace", "link")


def _wrap1(fn):
    @functools.wraps(fn)
    def wrapper(path, *args, **kwargs):
        return fn(_rewrite(path, kwargs.get("dir_fd")), *args, **kwargs)

    return wrapper


def _wrap2(fn):
    @functools.wraps(fn)
    def wrapper(src, dst, *args, **kwargs):
        src = _rewrite(src, kwargs.get("src_dir_fd"))
        return fn(src, _rewrite(dst, kwargs.get("dst_dir_fd")), *args, **kwargs)

    return wrapper


def _symlink(target, link, *args, **kwargs):
    # The target is link content, not a path to rewrite: it must read the same after commit.
    return _cfg.orig["os.symlink"](target, _rewrite(link, kwargs.get("dir_fd")), *args, **kwargs)


def _open(file, *args, **kwargs):
    f = _cfg.orig["open"](_rewrite(file), *args, **kwargs)
    s = _current.get()
    if s is not None:
        s._files.add(f)
    return f


def _realpath(path, *args, **kwargs):
    return _unmap(_cfg.orig["os.path.realpath"](path, *args, **kwargs))


def _getcwd():
    return _unmap(_cfg.orig["os.getcwd"]())


class _Popen(subprocess.Popen):
    """In a scope, the child runs through `escrow exec` in the scope's sandbox."""

    def __init__(self, args, *a, **kw):
        s = _current.get()
        if s is not None:
            shell = kw.pop("shell", False)
            kw.pop("executable", None)
            if isinstance(args, (str, bytes, os.PathLike)):
                argv = ["/bin/sh", "-c", os.fspath(args)] if shell else [os.fspath(args)]
            else:
                argv = [os.fspath(x) for x in args]
                if shell:
                    argv = ["/bin/sh", "-c", *argv]
            cwd = kw.pop("cwd", None)
            kw["cwd"] = _rewrite_to(s.root, os.path.abspath(os.fspath(cwd) if cwd else os.getcwd()))
            exec_ = [_cfg.exe, "exec", "--scope", s.id, "--socket", _cfg.socket, "--"]
            args = [*exec_, *(os.fsdecode(x) for x in argv)]
        super().__init__(args, *a, **kw)


def _set(obj, name: str, value) -> None:
    """Patch an attribute; the wrappers take the same arguments, not the stubs' exact overloads."""
    setattr(obj, name, value)


def _install(project: str, socket: str, views: str | None, unscoped: str) -> None:
    if _cfg.orig:
        _uninstall()
    _cfg.project, _cfg.socket, _cfg.views, _cfg.unscoped = project, socket, views, unscoped
    _cfg.exe = _escrow_exe()
    o = _cfg.orig
    o["open"], o["subprocess.Popen"] = builtins.open, subprocess.Popen
    o["os.path.realpath"], o["os.getcwd"], o["os.symlink"] = os.path.realpath, os.getcwd, os.symlink
    _set(builtins, "open", _open)
    _set(io, "open", _open)
    for name in _WRAP1:
        o[f"os.{name}"] = getattr(os, name)
        setattr(os, name, _wrap1(getattr(os, name)))
    for name in _WRAP2:
        o[f"os.{name}"] = getattr(os, name)
        setattr(os, name, _wrap2(getattr(os, name)))
    _set(os, "symlink", _symlink)
    _set(os.path, "realpath", _realpath)
    _set(os, "getcwd", _getcwd)
    _set(subprocess, "Popen", _Popen)


def _uninstall() -> None:
    """Undo `init()`'s wrappers (tests)."""
    o = _cfg.orig
    if not o:
        return
    _set(builtins, "open", o["open"])
    _set(io, "open", o["open"])
    for name in (*_WRAP1, *_WRAP2, "symlink", "getcwd"):
        setattr(os, name, o[f"os.{name}"])
    _set(os.path, "realpath", o["os.path.realpath"])
    _set(subprocess, "Popen", o["subprocess.Popen"])
    o.clear()
    _cfg.socket = ""
