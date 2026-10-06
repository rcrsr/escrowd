"""Scopes for Python programs.

`init()` puts the program under `escrow run` (re-executing it if needed) and wraps the
file API: inside a scope, `open`, `io.open`, the `os` path functions (and so pathlib,
shutil and os.walk) rewrite `<project>/x` to the scope's view (and `$HOME/x` or `/tmp/x`
to its views of those roots, when the policy serves them), and `subprocess.Popen`
(and so `subprocess.run` and asyncio subprocesses), `os.system` and `os.posix_spawn*`
run the child through `escrow exec` in the scope's sandbox. `os.chdir` into the project
moves into the scope's view (`os.getcwd` and `os.path.realpath` map it back); the scope's
close moves back to the project path. The current scope lives in a ContextVar, so
concurrent asyncio tasks on one thread keep separate scopes.

Errors: a project write outside any scope in `deny` mode raises `EscrowUnscopedError`,
and a write on a file opened in a scope that has closed raises `EscrowStaleHandleError`;
both are `OSError`s, as before. At close, `s.outcome.unscoped` counts the changes that
reached the unscoped mode while the scope was open (a warning: IO that escaped it).

Each scope holds its token (from the daemon, kept on the `Scope` object only): closing,
deciding and `escrow exec` in the scope send it, so code that learns a scope id from a
path cannot decide that scope. `escrow exec` gets it in `ESCROW_SCOPE_TOKEN`, which the
child does not see.

Not covered (falls to the unscoped mode): `os.spawn*`, `os.exec*` and `os.fork`, native
code doing its own IO, the working directory of other tasks while one scope has moved
it (it is per process, not per scope), and file descriptors beyond stdin, stdout and
stderr passed to children.
"""

from __future__ import annotations

import builtins
import contextvars
import errno
import functools
import inspect
import io
import os
import pathlib
import shutil
import subprocess
import sys
import warnings
import weakref
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field

from escrow._client import connect
from escrow.v1 import escrow_pb2 as pb


class EscrowError(Exception):
    pass


class EscrowUnscopedError(EscrowError, PermissionError):
    """A project write outside any scope in `deny` mode (EROFS): open a scope for it."""


class EscrowStaleHandleError(EscrowError, OSError):
    """A write on a file opened in a scope that has closed (EBADF): reopen the file in a
    new scope."""


class EscrowUnscopedWarning(UserWarning):
    """Changes reached the unscoped mode while a scope was open: IO that escaped it."""


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


_TIERS = {pb.TIER_SOFTWARE: "software", pb.TIER_LLM: "llm", pb.TIER_HUMAN: "human"}


@dataclass(frozen=True)
class Review:
    """The daemon's close-time review (policy `write:` and `review:`)."""

    # The software tier's verdict: "discard" when a write rule broke (a commit or a
    # return then becomes a discard), else "commit".
    verdict: str = "commit"
    reasons: list[str] = field(default_factory=list)
    # Tiers above software the change set needs, cheapest first ("llm", "human"); the
    # daemon refuses to commit while any is listed.
    tiers: list[str] = field(default_factory=list)
    # A review rule requires the agent to wait for those tiers.
    wait_required: bool = False

    @classmethod
    def from_proto(cls, r: pb.Review) -> Review:
        return cls(
            verdict="discard" if r.verdict == pb.VERDICT_DISCARD else "commit",
            reasons=list(r.reasons),
            tiers=[_TIERS[t] for t in r.tiers],
            wait_required=r.wait_required,
        )


@dataclass(frozen=True)
class ChangeSet:
    """What a closed scope would do to the project, and what it read."""

    scope_id: str
    changes: list[Change]
    reads: list[Read]
    labels: dict[str, str]
    # Content diff against the scope's snapshot, git format; binary and large files
    # summarized (size, SHA-256); capped by the policy's `diff:` sizes.
    diff: str = ""
    # Changes that reached the unscoped mode while the scope was open (0 in passthrough).
    unscoped: int = 0
    review: Review = field(default_factory=Review)

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
            diff=cs.diff,
            unscoped=cs.unscoped_ops,
            review=Review.from_proto(cs.review),
        )


@dataclass(frozen=True)
class Outcome:
    status: str  # committed, discarded, returned, conflict
    paths: list[str]  # changed paths, or the conflicting ones
    reasons: list[str]
    changes: ChangeSet
    # The scope is open again with its changes (`resume=` it): status returned, or a
    # conflict under the policy's `conflict.verdict: return`.
    reopened: bool = False

    @property
    def reads(self) -> list[Read]:
        return self.changes.reads

    @property
    def labels(self) -> dict[str, str]:
        return self.changes.labels

    @property
    def diff(self) -> str:
        return self.changes.diff

    @property
    def unscoped(self) -> int:
        return self.changes.unscoped


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
        return _apply(c, cs, d, "")


def _apply(c, cs: ChangeSet, d: Decision | None, token: str) -> Outcome:
    d = d or commit()
    out = c.decide(cs.scope_id, d.verdict, reasons=list(d.reasons), token=token)
    reasons = list(out.reasons) or list(d.reasons)
    return Outcome(_STATUS[out.status], list(out.paths), reasons, cs, out.reopened)


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
            with connect(_cfg.socket) as c:
                s = c.open_scope(self.name, self.labels)
            self.id = s.scope_id
            self._scope_token = s.token
            self.root = _view(s.root)
            self.roots = [(r.path, _view(r.view), tuple(r.direct)) for r in s.roots]
        self.closed = False
        self._ctx = _current.set(self)
        return self

    def _close(self) -> ChangeSet:
        if self._ctx is not None:
            _current.reset(self._ctx)
            self._ctx = None
        self.flush()
        with connect(_cfg.socket) as c:
            cs = ChangeSet.from_proto(c.close_scope(self.id, self._scope_token))
        self.closed = True
        if cs.unscoped:
            warnings.warn(
                f"escrow: {cs.unscoped} change(s) reached the unscoped mode "
                f"({_cfg.unscoped}) while scope {self.id} was open",
                EscrowUnscopedWarning,
                stacklevel=3,
            )
        return cs

    def _finish(self, cs: ChangeSet, d: Decision | None) -> None:
        # The working directory is per process: once decided, move it out of the scope's
        # views (gone after a commit or a discard) to the same path in the project.
        cwd = _cfg.orig["os.getcwd"]()
        back = _unmap_from(self, cwd)
        with connect(_cfg.socket) as c:
            self.outcome = _apply(c, cs, d, self._scope_token)
        if back != cwd:
            _chdir_nearest(back)

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
        """Where a project path (or a path in a served root) lives in this scope's views."""
        return _rewrite_to(self, os.fspath(p))


scope = Scope


def _chdir_nearest(path: str) -> None:
    """chdir to `path`, or to its nearest existing ancestor (a discard drops new dirs)."""
    for d in (path, *map(str, pathlib.Path(path).parents)):
        try:
            return _cfg.orig["os.chdir"](d)
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
    "chdir",
)
_WRAP2 = ("rename", "replace", "link")
_SPAWN = ("system", "posix_spawn", "posix_spawnp")


def _unscoped_error(e: OSError, *paths) -> OSError:
    """EROFS on a project path outside any scope in `deny` mode: say why, keep the errno."""
    if e.errno != errno.EROFS or _cfg.unscoped != "deny" or _current.get() is not None:
        return e
    for p in paths:
        if isinstance(p, int):
            continue
        p = os.fsdecode(os.fspath(p))
        if _under(os.path.abspath(p), _cfg.project):
            msg = "project writes outside a scope are refused (unscoped=deny): open an escrow.scope"
            return EscrowUnscopedError(errno.EROFS, msg, p)
    return e


def _wrap1(fn):
    @functools.wraps(fn)
    def wrapper(path, *args, **kwargs):
        try:
            return fn(_rewrite(path, kwargs.get("dir_fd")), *args, **kwargs)
        except OSError as e:
            if kwargs.get("dir_fd") is not None:
                raise
            raise _unscoped_error(e, path) from None

    return wrapper


def _wrap2(fn):
    @functools.wraps(fn)
    def wrapper(src, dst, *args, **kwargs):
        s2 = _rewrite(src, kwargs.get("src_dir_fd"))
        try:
            return fn(s2, _rewrite(dst, kwargs.get("dst_dir_fd")), *args, **kwargs)
        except OSError as e:
            if kwargs.get("src_dir_fd") is not None or kwargs.get("dst_dir_fd") is not None:
                raise
            raise _unscoped_error(e, src, dst) from None

    return wrapper


def _symlink(target, link, *args, **kwargs):
    # The target is link content, not a path to rewrite: it must read the same after commit.
    try:
        return _cfg.orig["os.symlink"](
            target, _rewrite(link, kwargs.get("dir_fd")), *args, **kwargs
        )
    except OSError as e:
        if kwargs.get("dir_fd") is not None:
            raise
        raise _unscoped_error(e, link) from None


_STALE = ("write", "writelines", "flush", "truncate", "close")
# fd-level calls that reach the daemon after a buffered write (the writeback cache
# takes `flush`; fsync or close report the EBADF).
_STALE_FD = ("write", "fsync", "fdatasync", "ftruncate")
# Files opened in a scope, for the fd-level calls to find theirs.
_scoped_files: weakref.WeakKeyDictionary = weakref.WeakKeyDictionary()


def _stale_error(e: OSError, s: Scope, name) -> OSError:
    if e.errno != errno.EBADF or not s.closed:
        return e
    msg = f"opened in scope {s.id}, which has closed: reopen it in a new scope"
    return EscrowStaleHandleError(errno.EBADF, msg, name)


def _guard_stale(f, s: Scope) -> None:
    """EBADF from a file of `s` after `s` closed becomes EscrowStaleHandleError. The
    wrappers reach the file through a weak reference and the class's methods: a bound
    method would make a cycle, and the file would outlive its last reference (its
    descriptor open on the view) until the garbage collector ran."""
    name = getattr(f, "name", None)
    ref = weakref.ref(f)
    _scoped_files[f] = s

    def guard(method):
        @functools.wraps(method)
        def wrapper(*args, **kwargs):
            try:
                return method(ref(), *args, **kwargs)
            except OSError as e:
                raise _stale_error(e, s, name) from None

        return wrapper

    for m in _STALE:
        method = getattr(type(f), m, None)
        if method is not None:
            setattr(f, m, guard(method))


def _guard_fd(fn):
    @functools.wraps(fn)
    def wrapper(fd, *args, **kwargs):
        try:
            return fn(fd, *args, **kwargs)
        except OSError as e:
            if e.errno == errno.EBADF:
                for f, s in list(_scoped_files.items()):
                    if not f.closed and f.fileno() == fd:
                        raise _stale_error(e, s, getattr(f, "name", None)) from None
            raise

    return wrapper


def _open(file, *args, **kwargs):
    try:
        f = _cfg.orig["open"](_rewrite(file), *args, **kwargs)
    except OSError as e:
        raise _unscoped_error(e, file) from None
    s = _current.get()
    if s is not None:
        s._files.add(f)
        _guard_stale(f, s)
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
            kw["cwd"] = _rewrite_to(s, os.path.abspath(os.fspath(cwd) if cwd else os.getcwd()))
            args = _exec_argv(s, [os.fsdecode(x) for x in argv])
            kw["env"] = _exec_env(s, kw.get("env"))
        super().__init__(args, *a, **kw)


def _system(command):
    """In a scope, the shell runs through `escrow exec`; returns a wait status, as os.system."""
    if _current.get() is None:
        return _cfg.orig["os.system"](command)
    code = _Popen(os.fsdecode(command), shell=True).wait()
    return -code if code < 0 else code << 8


def _exec_argv(s: Scope, argv) -> list[str]:
    return [_cfg.exe, "exec", "--scope", s.id, "--socket", _cfg.socket, "--", *argv]


def _exec_env(s: Scope, env) -> dict:
    """The child's environment (default: this process's) plus the scope's token for
    `escrow exec`, which strips it before the child starts. subprocess and posix_spawn
    take str and bytes keys alike."""
    out: dict = dict(os.environ if env is None else env)
    out["ESCROW_SCOPE_TOKEN"] = s._scope_token
    return out


def _spawner(name: str):
    """`os.posix_spawn` and `os.posix_spawnp`: in a scope, the child runs through `escrow
    exec` (file actions apply to it: stdio redirects reach the child, opened paths are
    rewritten). Returns the PID of `escrow exec`, whose exit status is the child's."""
    orig = getattr(os, name)

    @functools.wraps(orig)
    def wrapper(path, argv, env, *args, **kwargs):
        s = _current.get()
        if s is None or os.fsdecode(path) == _cfg.exe:  # subprocess already routed it
            return orig(path, argv, env, *args, **kwargs)
        actions = kwargs.get("file_actions")
        if actions:
            kwargs["file_actions"] = [
                (a[0], a[1], _rewrite_to(s, os.fsdecode(a[2])), *a[3:])
                if a[0] == os.POSIX_SPAWN_OPEN
                else a
                for a in actions
            ]
        argv = [os.fsdecode(path), *(os.fsdecode(x) for x in list(argv)[1:])]
        env = _exec_env(s, env)
        return _cfg.orig["os.posix_spawn"](_cfg.exe, _exec_argv(s, argv), env, *args, **kwargs)

    return wrapper


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
    for name in _STALE_FD:
        o[f"os.{name}"] = getattr(os, name)
        setattr(os, name, _guard_fd(getattr(os, name)))
    for name in _SPAWN:
        o[f"os.{name}"] = getattr(os, name)
    _set(os, "system", _system)
    _set(os, "posix_spawn", _spawner("posix_spawn"))
    _set(os, "posix_spawnp", _spawner("posix_spawnp"))


def _uninstall() -> None:
    """Undo `init()`'s wrappers (tests)."""
    o = _cfg.orig
    if not o:
        return
    _set(builtins, "open", o["open"])
    _set(io, "open", o["open"])
    for name in (*_WRAP1, *_WRAP2, *_SPAWN, *_STALE_FD, "symlink", "getcwd"):
        setattr(os, name, o[f"os.{name}"])
    _set(os.path, "realpath", o["os.path.realpath"])
    _set(subprocess, "Popen", o["subprocess.Popen"])
    o.clear()
    _cfg.socket = ""
