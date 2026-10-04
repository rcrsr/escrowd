"""Per-scope routing and read gating: spike 0.4's 17 checks, on the daemon and the SDK.

The Python SDK (installed by `escrow.init` against a running daemon) rewrites project
paths to each scope's view through a ContextVar and runs subprocesses through
`escrow exec`; the read gate denies `.env`. Scopes end with `send_back`, which keeps
their staged files for inspection and the project untouched.
"""

import asyncio
import errno
import hashlib
import os
import shutil
import threading
from pathlib import Path

import pytest

import escrow
from escrow import _sdk


def fingerprint(root: Path) -> list[str]:
    out = []
    for p in sorted(root.rglob("*")):
        st = p.lstat()
        out.append(
            f"{p.relative_to(root)}|{st.st_mode:o}|{st.st_size}|{st.st_mtime_ns}|"
            + (hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else "")
        )
    return out


class Run:
    """Everything the checks inspect, gathered in one pass like spike 0.4's test_04.py."""


@pytest.fixture(scope="module")
def run(escrow_bin):
    from conftest import Daemon, make_runtime_dir

    work = make_runtime_dir()
    d = Daemon(escrow_bin, work)
    try:
        r = setup_and_run(d, escrow_bin)
    except BaseException:
        d.stop()
        shutil.rmtree(work, ignore_errors=True)
        raise
    yield r
    d.stop()
    shutil.rmtree(work, ignore_errors=True)


def setup_and_run(d, escrow_bin) -> Run:
    project = d.project
    (project / "README.md").write_text("base")
    (project / ".env").write_text("SECRET=1\n")
    (project / "base.txt").write_text("b\n")
    r = Run()
    r.daemon, r.project = d, project
    r.before = fingerprint(project)
    cwd = os.getcwd()
    env = {"ESCROW_SOCKET": str(d.socket), "ESCROW_EXE": str(escrow_bin)}
    saved = {k: os.environ.get(k) for k in env}
    os.environ.update(env)
    try:
        escrow.init(project, unscoped="passthrough")
        os.chdir(project)
        asyncio.run(gather(r))
    finally:
        _sdk._uninstall()
        os.chdir(cwd)
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
    r.after = fingerprint(project)
    return r


def keep(changes):
    """Return to agent: the scope reopens with its staged files, the project stays as it was."""
    return escrow.send_back()


async def worker(r, name, readme, extra):
    """One scope: interleaved writes, reads, a pathlib write and a bash subprocess."""
    seen = {}
    async with escrow.scope(name, decide=keep) as s:
        seen["id"] = s.id
        r.threads.add(threading.get_ident())
        with open(os.path.join(r.project, "README.md"), "w") as f:  # absolute path
            f.write(readme)
        await asyncio.sleep(0)
        seen["readme"] = open("README.md").read()  # relative path, cwd is the project
        (r.project / extra).write_text(name)  # pathlib
        await asyncio.sleep(0)
        proc = await asyncio.create_subprocess_exec(
            "bash", "-c", f"echo built{name} > build.log", cwd=r.project
        )
        await proc.wait()
        await asyncio.sleep(0)
        seen["build"] = open("build.log").read()
        seen["ino"] = os.stat("base.txt").st_ino
    return seen


async def gather(r):
    r.threads = set()
    r.a, r.b = await asyncio.gather(
        worker(r, "a", "A", "notes-a.txt"), worker(r, "b", "B", "notes-b.txt")
    )
    r.base_readme = (r.project / "README.md").read_text()  # outside any scope

    async with escrow.scope("c", decide=keep) as s:
        r.c = s.id
        try:
            open(".env").read()
            r.env_error = None
        except PermissionError as e:
            r.env_error = e
        proc = await asyncio.create_subprocess_exec(
            "cat", ".env", cwd=r.project, stderr=asyncio.subprocess.PIPE
        )
        _, r.cat_err = await proc.communicate()
        r.cat_rc = proc.returncode
        r.allowed_read = open("README.md").read()

    n = 4 * 1024 * 1024
    r.n = n
    async with escrow.scope("d", decide=keep) as s:
        up = r.daemon.upper(s.id)
        f = open("big.bin", "wb")
        f.write(b"x" * n)
        f.flush()  # Python buffer -> kernel page cache; file still open
        s.flush()  # fsync of the scope's open files
        r.after_flush = (up / "big.bin").stat().st_size
        f.close()
        g = open("closed.bin", "wb")
        g.write(b"y" * n)
        g.close()
        r.after_close = (up / "closed.bin").stat().st_size


def staged(r, scope_id, rel):
    p = r.daemon.upper(scope_id) / rel
    return p.read_text() if p.exists() else None


def test_two_tasks_ran_on_one_thread(run):
    assert len(run.threads) == 1


def test_each_scope_reads_its_own_readme(run):
    assert (run.a["readme"], run.b["readme"]) == ("A", "B")


def test_each_scope_reads_its_own_subprocess_output(run):
    assert (run.a["build"], run.b["build"]) == ("builta\n", "builtb\n")


def test_scope_a_staged_its_writes(run):
    a = run.a["id"]
    got = (staged(run, a, "README.md"), staged(run, a, "notes-a.txt"), staged(run, a, "build.log"))
    assert got == ("A", "a", "builta\n")


def test_scope_b_staged_its_writes(run):
    b = run.b["id"]
    got = (staged(run, b, "README.md"), staged(run, b, "notes-b.txt"), staged(run, b, "build.log"))
    assert got == ("B", "b", "builtb\n")


def test_no_cross_scope_writes(run):
    assert staged(run, run.a["id"], "notes-b.txt") is None
    assert staged(run, run.b["id"], "notes-a.txt") is None


def test_same_lower_file_has_a_different_inode_per_scope(run):
    assert run.a["ino"] != run.b["ino"]


def test_base_readme_untouched_outside_scopes(run):
    assert run.base_readme == "base"


def test_in_process_read_of_env_denied_with_eacces(run):
    assert run.env_error is not None and run.env_error.errno == errno.EACCES


def test_subprocess_read_of_env_denied(run):
    assert run.cat_rc != 0 and b"Permission denied" in run.cat_err


def test_allowed_read_still_works(run):
    assert run.allowed_read == "base"


def test_both_denials_in_the_ledger_with_scope(run):
    denies = [x for x in run.daemon.ledger if f"scope={run.c} op=read path=.env decision=deny" in x]
    assert len(denies) == 2, denies


def test_subprocess_writes_attributed_by_path_in_the_ledger(run):
    log = run.daemon.ledger
    for s in (run.a["id"], run.b["id"]):
        assert any(f"scope={s} op=create path=build.log" in x for x in log)


def test_scope_flush_delivers_every_byte_while_open(run):
    assert run.after_flush == run.n


def test_close_alone_delivers_every_byte(run):
    assert run.after_close == run.n


def test_base_byte_identical(run):
    assert run.after == run.before


def test_views_mounted(run):
    assert os.path.ismount(run.daemon.mount)
