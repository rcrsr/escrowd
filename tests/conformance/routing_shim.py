"""Test-only path-rewrite shim, ported from spike 0.4; the Python SDK replaces it in phase 1.6.

The current scope's root lives in a ContextVar, so concurrent asyncio tasks on one thread
keep separate scopes. Wrapped file functions rewrite <project>/x to <scope root>/x;
wrapped subprocess.Popen starts children in bwrap with the scope root over <project>.
Closing a scope fsyncs every file it still has open: syncfs is not a barrier on plain FUSE.
"""

import builtins
import contextlib
import contextvars
import io
import os
import subprocess
import weakref

import escrow

_root: contextvars.ContextVar[str | None] = contextvars.ContextVar("escrow_root", default=None)
_cfg: dict = {"project": None, "socket": None, "bwrap": "bwrap"}
_orig: dict = {}
_open_files: dict[str, weakref.WeakSet] = {}
_WRAP1 = (
    "open",
    "stat",
    "lstat",
    "listdir",
    "scandir",
    "mkdir",
    "remove",
    "unlink",
    "rmdir",
    "chmod",
    "utime",
    "truncate",
    "access",
    "readlink",
)
_WRAP2 = ("rename", "replace", "link", "symlink")


def _rewrite(path):
    root = _root.get()
    if root is None or isinstance(path, int):
        return path
    p = os.fspath(path)
    if isinstance(p, bytes):
        return path
    a = os.path.abspath(p)
    project = _cfg["project"]
    if a == project or a.startswith(project + os.sep):
        return os.path.join(root, os.path.relpath(a, project))
    return path


def _wrap1(fn):
    def wrapper(path, *args, **kwargs):
        return fn(_rewrite(path), *args, **kwargs)

    wrapper.__wrapped__ = fn
    return wrapper


def _wrap2(fn):
    def wrapper(src, dst, *args, **kwargs):
        return fn(_rewrite(src), _rewrite(dst), *args, **kwargs)

    wrapper.__wrapped__ = fn
    return wrapper


def _open(file, *args, **kwargs):
    f = _orig["open"](_rewrite(file), *args, **kwargs)
    root = _root.get()
    if root is not None:
        _open_files.setdefault(root, weakref.WeakSet()).add(f)
    return f


class _Popen(subprocess.Popen):
    def __init__(self, args, *a, **kw):
        root = _root.get()
        if root is not None:
            if isinstance(args, (str, bytes, os.PathLike)):
                args = ["/bin/sh", "-c", os.fspath(args)] if kw.get("shell") else [os.fspath(args)]
            kw["shell"] = False
            cwd = os.path.abspath(kw.pop("cwd", None) or os.getcwd())
            args = [
                _cfg["bwrap"],
                "--unshare-user",
                "--disable-userns",
                "--unshare-pid",
                "--die-with-parent",
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--bind",
                root,
                _cfg["project"],
                "--chdir",
                cwd,
                "--",
                *args,
            ]
        super().__init__(args, *a, **kw)


def install(project: str, socket: str, bwrap: str) -> None:
    _cfg.update(project=os.path.abspath(project), socket=socket, bwrap=bwrap)
    _orig["open"] = builtins.open
    _orig["subprocess.Popen"] = subprocess.Popen
    builtins.open = io.open = _open
    for name in _WRAP1:
        _orig[f"os.{name}"] = getattr(os, name)
        setattr(os, name, _wrap1(getattr(os, name)))
    for name in _WRAP2:
        _orig[f"os.{name}"] = getattr(os, name)
        setattr(os, name, _wrap2(getattr(os, name)))
    subprocess.Popen = _Popen


def uninstall() -> None:
    builtins.open = io.open = _orig["open"]
    for name in _WRAP1 + _WRAP2:
        setattr(os, name, _orig[f"os.{name}"])
    subprocess.Popen = _orig["subprocess.Popen"]


def flush(root: str) -> None:
    """Push every dirty page of the scope to the daemon: fsync its open files."""
    for f in list(_open_files.get(root, ())):
        if not f.closed and f.writable():
            f.flush()
            os.fsync(f.fileno())


@contextlib.asynccontextmanager
async def scope(name: str):
    with escrow.connect(_cfg["socket"]) as c:
        s = c.open_scope(name)
    token = _root.set(s.root)
    try:
        yield s
    finally:
        _root.reset(token)
        flush(s.root)
