"""Spike 0.4 path-rewrite shim: attributes in-process IO and subprocesses to scopes by path.

The current scope lives in a ContextVar, so concurrent asyncio tasks on one thread keep
separate scopes. Wrapped file functions rewrite <project>/x to <mount>/<scope>/x. Wrapped
subprocess.Popen starts children in bwrap with <mount>/<scope> mounted over <project>, so
bash, heredocs and absolute paths in the child see ordinary project paths.

Closing a scope fsyncs every file the scope still has open, then calls syncfs on its root.
fsync and close wait for FUSE writeback to reach the daemon; syncfs alone does not (measured
in spike 0.4 on Ubuntu 26.04), because plain FUSE mounts have no sync_fs to wait on.
"""

import builtins
import contextlib
import contextvars
import ctypes
import io
import os
import subprocess
import weakref

_scope: contextvars.ContextVar[str | None] = contextvars.ContextVar("escrow_scope", default=None)
_cfg = {"project": None, "mount": None, "bwrap": "bwrap"}
_libc = ctypes.CDLL(None, use_errno=True)
_orig = {}
_open_files: dict[str, weakref.WeakSet] = {}


def current() -> str | None:
    return _scope.get()


def _rewrite(path):
    s = _scope.get()
    if s is None or isinstance(path, int):
        return path
    p = os.fspath(path)
    if isinstance(p, bytes):
        return path
    a = os.path.abspath(p)
    project = _cfg["project"]
    if a == project or a.startswith(project + os.sep):
        return os.path.join(_cfg["mount"], s, os.path.relpath(a, project))
    return path


def _wrap1(fn):
    def wrapper(path, *args, **kwargs):
        return fn(_rewrite(path), *args, **kwargs)
    wrapper.__wrapped__ = fn
    return wrapper


def _open(file, *args, **kwargs):
    f = _orig["open"](_rewrite(file), *args, **kwargs)
    s = _scope.get()
    if s is not None:
        _open_files.setdefault(s, weakref.WeakSet()).add(f)
    return f


def _wrap2(fn):
    def wrapper(src, dst, *args, **kwargs):
        return fn(_rewrite(src), _rewrite(dst), *args, **kwargs)
    wrapper.__wrapped__ = fn
    return wrapper


class _Popen(subprocess.Popen):
    def __init__(self, args, *a, **kw):
        s = _scope.get()
        if s is not None:
            if isinstance(args, (str, bytes, os.PathLike)):
                args = ["/bin/sh", "-c", os.fspath(args)] if kw.get("shell") else [os.fspath(args)]
            kw["shell"] = False
            cwd = os.path.abspath(kw.pop("cwd", None) or os.getcwd())
            args = [
                _cfg["bwrap"], "--unshare-user", "--disable-userns", "--unshare-pid", "--die-with-parent",
                "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
                "--bind", os.path.join(_cfg["mount"], s), _cfg["project"],
                "--chdir", cwd, "--", *args,
            ]
        super().__init__(args, *a, **kw)


def init(project: str, mount: str, bwrap: str = "bwrap") -> None:
    _cfg.update(project=os.path.abspath(project), mount=os.path.abspath(mount), bwrap=bwrap)
    _orig["open"] = builtins.open
    _orig["os.open"] = os.open
    _orig["os.mkdir"] = os.mkdir
    builtins.open = io.open = _open
    for name in ("open", "stat", "lstat", "listdir", "scandir", "mkdir", "remove", "unlink", "rmdir",
                 "chmod", "utime", "truncate", "access", "readlink"):
        setattr(os, name, _wrap1(getattr(os, name)))
    for name in ("rename", "replace", "link", "symlink"):
        setattr(os, name, _wrap2(getattr(os, name)))
    subprocess.Popen = _Popen


def flush(name: str) -> None:
    """Push every dirty page of the scope to the daemon: fsync its open files, then syncfs."""
    for f in list(_open_files.get(name, ())):
        if not f.closed and f.writable():
            f.flush()
            os.fsync(f.fileno())
    syncfs(name)


def syncfs(name: str) -> None:
    fd = _orig["os.open"](os.path.join(_cfg["mount"], name), os.O_RDONLY | os.O_DIRECTORY)
    try:
        if _libc.syncfs(fd) != 0:
            err = ctypes.get_errno()
            raise OSError(err, os.strerror(err))
    finally:
        os.close(fd)


@contextlib.asynccontextmanager
async def scope(name: str):
    _orig["os.mkdir"](os.path.join(_cfg["mount"], name))  # the daemon creates the scope
    token = _scope.set(name)
    try:
        yield name
    finally:
        _scope.reset(token)
        flush(name)
