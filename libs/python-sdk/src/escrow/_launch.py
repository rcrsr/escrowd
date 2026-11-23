"""Launching: put the program under `escrow run` and install the wrappers."""

from __future__ import annotations

import os
import pathlib
import sys

from escrow._client import Client
from escrow._config import _escrow_exe
from escrow._errors import EscrowError
from escrow._patch import _install


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


# Interpreter options that take a value (-c and -m end the options: no script follows).
_VALUE_FLAGS = "WX"
_INIT_CWD = "ESCROW_INIT_CWD"
# The oldest daemon protocol (`PingResponse.protocol_version`) this SDK works with.
_MIN_PROTOCOL = 7


def _script_index(argv: list[str]) -> int | None:
    """Index in `argv` (the interpreter's arguments after its name) of the script path, or
    None for the -c, -m and stdin forms."""
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--":
            return i + 1 if i + 1 < len(argv) else None
        if a == "-" or not a.startswith("-"):
            return None if a == "-" else i
        if a.startswith("--"):
            i += 2 if a == "--check-hash-based-pycs" else 1
            continue
        for j, ch in enumerate(a[1:], 1):
            if ch in "cm":
                return None
            if ch in _VALUE_FLAGS:
                i += 1 if j < len(a) - 1 else 2
                break
        else:
            i += 1
    return None


def _absolute_script(argv: list[str]) -> tuple[list[str], str | None]:
    """`argv` with the script path made absolute against the current directory, and that
    absolute path (None when there is no script file)."""
    idx = _script_index(argv)
    if idx is None:
        return argv, None
    script = os.path.abspath(argv[idx])
    return [*argv[:idx], script, *argv[idx + 1 :]], script


def _within(path: str, root: str) -> bool:
    return path == root or path.startswith(root + os.sep)


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
    socket = os.environ.get("ESCROW_SOCKET")
    # Set by the first pass for the second; removed before user code or children see it.
    init_cwd = os.environ.pop(_INIT_CWD, None) if socket else None
    project = os.fspath(project)
    if init_cwd:
        project = os.path.join(init_cwd, project)
    project = os.path.realpath(project)
    if not socket:
        exe = _escrow_exe()
        args = [exe, "run", "--project", project, "--unscoped", unscoped]
        if policy:
            args += ["--policy", os.path.realpath(policy)]
        if on_exit:
            args += ["--on-exit", on_exit]
        argv, script = _absolute_script(sys.orig_argv[1:])
        extra = list(read)
        if script and not _within(os.path.realpath(script), project):
            extra.append(script)
        for p in _read_paths(extra):
            args += ["--read", p]
        sys.stdout.flush()
        sys.stderr.flush()
        os.environ[_INIT_CWD] = os.getcwd()
        os.execv(exe, [*args, "--", sys.executable, *argv])
    for var, want in (("ESCROW_PROJECT", project), ("ESCROW_UNSCOPED", unscoped)):
        got = os.environ.get(var)
        if got and got != want:
            raise EscrowError(f"escrow run serves {var}={got}, init() asked for {want}")
    if init_cwd and os.path.isdir(init_cwd):
        os.chdir(init_cwd)
    with Client(socket) as client:
        version = client.ping().protocol_version
    if version < _MIN_PROTOCOL:
        raise EscrowError(
            f"the daemon at {socket} speaks protocol {version}; this SDK needs "
            f"protocol {_MIN_PROTOCOL} or newer"
        )
    _install(project, socket, os.environ.get("ESCROW_VIEWS"), unscoped)
