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

Held decisions: a commit of a change set the policy's `review:` rules send to reviewers
is held. `escrow.scope(..., wait=True)` (the default) then returns after the reviewers'
verdict; `wait=False` returns with `s.outcome.status == "held"`, and `s.wait_decided()`
or `await s.decided()` gives the verdict later. A rule can require the wait: the
scope's session (`session=`) then opens no new scope until the verdict.

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
import errno
import functools
import io
import os
import subprocess
import weakref
from collections.abc import Callable
from typing import Any

from escrow._config import _cfg, _drop_client, _escrow_exe
from escrow._errors import EscrowStaleHandleError, EscrowUnscopedError
from escrow._scope import Scope, _current, _rewrite, _rewrite_to, _under, _unmap


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
        return _cfg.orig[(os, "symlink")](
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
        f = _cfg.orig[(builtins, "open")](_rewrite(file), *args, **kwargs)
    except OSError as e:
        raise _unscoped_error(e, file) from None
    s = _current.get()
    if s is not None:
        s._files.add(f)
        _guard_stale(f, s)
    return f


def _realpath(path, *args, **kwargs):
    return _unmap(_cfg.orig[(os.path, "realpath")](path, *args, **kwargs))


def _getcwd():
    return _unmap(_cfg.orig[(os, "getcwd")]())


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
        return _cfg.orig[(os, "system")](command)
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


def _spawner(orig):
    """`os.posix_spawn` and `os.posix_spawnp`: in a scope, the child runs through `escrow
    exec` (file actions apply to it: stdio redirects reach the child, opened paths are
    rewritten). Returns the PID of `escrow exec`, whose exit status is the child's."""

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
        return _cfg.orig[(os, "posix_spawn")](_cfg.exe, _exec_argv(s, argv), env, *args, **kwargs)

    return wrapper


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
_SPAWN = ("posix_spawn", "posix_spawnp")


def _fixed(wrapper) -> Callable[[Any], Any]:
    """A registry factory for a wrapper that does not wrap the original."""
    return lambda _orig: wrapper


# Every patched attribute: (owner, attribute, factory from the original to its wrapper).
# `_install` and `_uninstall` both walk this list; the originals are kept in
# `_cfg.orig[(owner, attribute)]`.
_REGISTRY: list[tuple[Any, str, Callable[[Any], Any]]] = [
    (builtins, "open", _fixed(_open)),
    (io, "open", _fixed(_open)),
    *((os, name, _wrap1) for name in _WRAP1),
    *((os, name, _wrap2) for name in _WRAP2),
    (os, "symlink", _fixed(_symlink)),
    (os.path, "realpath", _fixed(_realpath)),
    (os, "getcwd", _fixed(_getcwd)),
    (subprocess, "Popen", _fixed(_Popen)),
    *((os, name, _guard_fd) for name in _STALE_FD),
    (os, "system", _fixed(_system)),
    *((os, name, _spawner) for name in _SPAWN),
]


def _install(project: str, socket: str, views: str | None, unscoped: str) -> None:
    if _cfg.orig:
        _uninstall()
    if _cfg.client is not None and _cfg.client.socket != socket:
        _drop_client()
    _cfg.project, _cfg.socket, _cfg.views, _cfg.unscoped = project, socket, views, unscoped
    _cfg.exe = _escrow_exe()
    for owner, name, _ in _REGISTRY:
        _cfg.orig[(owner, name)] = getattr(owner, name)
    for owner, name, factory in _REGISTRY:
        setattr(owner, name, factory(_cfg.orig[(owner, name)]))


def _uninstall() -> None:
    """Undo `init()`'s wrappers (tests)."""
    _drop_client()
    if not _cfg.orig:
        return
    for owner, name, _ in _REGISTRY:
        setattr(owner, name, _cfg.orig[(owner, name)])
    _cfg.orig.clear()
    _cfg.socket = ""
