"""Phase 3.3: process attribution.

Every change names the processes that made it: each `Change.writers` refers to a
`ChangeSet.processes` entry with the program the kernel ran (path, device, inode), its
arguments and its parent chain, which stops before the daemon. The ledger names the
process of every change (`proc=<id>`) and holds one `proc` line per process. A
`write.only_by` rule names the programs allowed to change a path, by binary identity.
"""

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
from test_sdk import Sdk

import escrow
from escrow.v1 import escrow_pb2 as pb


def client(d):
    return escrow.connect(str(d.socket))


def writers(cs, path):
    """The processes of the change at `path`, by id."""
    procs = {p.id: p for p in cs.processes}
    (change,) = [c for c in cs.changes if c.path == path]
    return [procs[w] for w in change.writers]


def chain(cs, p):
    """`p` and its recorded ancestors, nearest first."""
    procs = {x.id: x for x in cs.processes}
    out = [p]
    while out[-1].parent:
        out.append(procs[out[-1].parent])
    return out


def real(program: str) -> str:
    return os.path.realpath(shutil.which(program) or program)


def test_a_sandboxed_command_is_the_writer(daemon):
    d = daemon
    with client(d) as c:
        s = c.open_scope().scope_id
        r = d.exec(s, "sh", "-c", "echo x > a.txt; mkdir d; cp a.txt d/b.txt")
        assert r.returncode == 0, r.stderr
        cs = c.close_scope(s)
    (sh,) = writers(cs, "a.txt")
    assert (sh.program, sh.args) == (
        real("sh"),
        ["sh", "-c", "echo x > a.txt; mkdir d; cp a.txt d/b.txt"],
    )
    (mkdir,) = writers(cs, "d")
    assert (mkdir.program, mkdir.args, mkdir.parent) == (real("mkdir"), ["mkdir", "d"], sh.id)
    (cp,) = writers(cs, "d/b.txt")
    assert (cp.program, cp.args) == (real("cp"), ["cp", "a.txt", "d/b.txt"])
    st = os.stat(real("cp"))
    assert (cp.dev, cp.ino) == (st.st_dev, st.st_ino)
    # cp was started by the shell; the chain stops before the daemon, at the sandbox.
    ancestors = chain(cs, cp)
    assert ancestors[1] == sh
    assert all(p.pid != d.proc.pid for p in ancestors)
    assert Path(ancestors[-1].program).name == "bwrap"


def test_an_exec_starts_a_new_writer(daemon):
    """The shell opens `out.txt`, then execs cp in the same process: one PID, two
    programs, each with its own changes."""
    d = daemon
    (d.project / "a.txt").write_text("a\n")
    with client(d) as c:
        s = c.open_scope().scope_id
        r = d.exec(s, "sh", "-c", "exec 3>out.txt; exec cp a.txt b.txt")
        assert r.returncode == 0, r.stderr
        cs = c.close_scope(s)
    (shell,) = writers(cs, "out.txt")
    (cp,) = writers(cs, "b.txt")
    assert (shell.program, cp.program) == (real("sh"), real("cp"))
    assert cp.args == ["cp", "a.txt", "b.txt"]
    assert shell.pid == cp.pid and shell.id != cp.id


@pytest.mark.skipif(shutil.which("git") is None, reason="git not installed")
def test_git_commit_is_attributed_to_git(daemon):
    """Exit criterion 2: `.git/` changes name git and its arguments, started by the shell."""
    d = daemon
    with client(d) as c:
        s = c.open_scope().scope_id
        script = "git init -q && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m m"
        r = d.exec(s, "sh", "-c", script, env={"HOME": None, "GIT_CONFIG_NOSYSTEM": "1"})
        assert r.returncode == 0, r.stderr
        cs = c.close_scope(s)
    gits = {p.program for p in cs.processes if p.program == real("git")}
    assert gits == {real("git")}
    commit = [p for c in cs.changes if c.path.startswith(".git/") for p in writers(cs, c.path)]
    args = ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty"]
    assert args + ["-m", "m"] in [p.args for p in commit]
    assert all(p.program == real("git") for p in commit)
    assert all(chain(cs, p)[1].args[:2] == ["sh", "-c"] for p in commit)


def test_in_process_io_names_the_host_process(daemon):
    d = daemon
    with client(d) as c:
        s = c.open_scope().scope_id
        (d.mount / s / "a.txt").write_text("a\n")
        cs = c.close_scope(s)
    (me,) = writers(cs, "a.txt")
    assert (me.pid, me.program) == (os.getpid(), os.path.realpath(sys.executable))
    assert me.args[0] == sys.executable or Path(me.args[0]).name.startswith("python")


def test_a_rename_carries_the_writers_of_its_source(daemon):
    d = daemon
    (d.project / "old.txt").write_text("base\n")
    with client(d) as c:
        s = c.open_scope().scope_id
        (d.mount / s / "tmp").write_text("staged\n")  # this process
        r = d.exec(s, "sh", "-c", "mv tmp final; mv old.txt new.txt")
        assert r.returncode == 0, r.stderr
        cs = c.close_scope(s)
    progs = lambda path: sorted(p.program for p in writers(cs, path))  # noqa: E731
    mv = real("mv")  # a symlink on some hosts (gnumv on Ubuntu 26.04)
    assert progs("final") == sorted([os.path.realpath(sys.executable), mv])
    kinds = {ch.path: (ch.kind, ch.from_path) for ch in cs.changes}
    assert kinds["new.txt"] == (pb.CHANGE_KIND_RENAME, "old.txt")
    assert progs("new.txt") == [mv]


def test_the_ledger_names_each_process_once(daemon):
    d = daemon
    with client(d) as c:
        s = c.open_scope().scope_id
        assert d.exec(s, "sh", "-c", "echo x > 'a b.txt'; echo y > c.txt").returncode == 0
        c.close_scope(s)
    lines = d.ledger
    creates = [x for x in lines if f" scope={s} op=create " in x]
    (pid,) = {x.split(" proc=")[1].split()[0] for x in creates}
    procs = [x for x in lines if x.split()[1] == f"proc={pid}"]
    assert len(procs) == 1
    fields = dict(f.split("=", 1) for f in procs[0].split()[1:])
    assert fields["exe"] == real("sh")
    assert fields["args"] == "sh,-c,echo%20x%20>%20'a%20b.txt';%20echo%20y%20>%20c.txt"
    assert lines.index(procs[0]) < lines.index(creates[0])
    # The parent's line comes before its child's.
    parent = fields["parent"]
    assert any(x.split()[1] == f"proc={parent}" for x in lines[: lines.index(procs[0])])


def test_only_by_allows_the_named_program_and_no_other(start_daemon):
    cp = real("cp")
    d = start_daemon(
        policy=f"write:\n  only_by:\n    - {{paths: ['locked/**'], programs: ['{cp}']}}\n"
    )
    (d.project / "locked").mkdir()
    shutil.copy(cp, d.project / "mycp")  # the same program under another inode
    with client(d) as c:
        ok, sh, copy, host = (c.open_scope().scope_id for _ in range(4))
        assert d.exec(ok, "cp", "mycp", "locked/x").returncode == 0
        assert d.exec(sh, "sh", "-c", "echo y > locked/y").returncode == 0
        assert d.exec(copy, "./mycp", "mycp", "locked/z").returncode == 0
        (d.mount / host / "locked" / "w").write_text("w\n")
        assert c.close_scope(ok).review.verdict == pb.VERDICT_COMMIT
        assert list(c.close_scope(sh).review.reasons) == [
            f"write.only_by: locked/y changed by {real('sh')}"
        ]
        # The program's path as the sandbox sees it: the project, not the daemon's view.
        assert list(c.close_scope(copy).review.reasons) == [
            f"write.only_by: locked/z changed by {d.project / 'mycp'}"
        ]
        assert list(c.close_scope(host).review.reasons) == [
            f"write.only_by: locked/w changed by {os.path.realpath(sys.executable)}"
        ]
        assert c.commit(ok).status == pb.OUTCOME_STATUS_COMMITTED
        assert c.commit(sh).status == pb.OUTCOME_STATUS_DISCARDED
    assert (d.project / "locked" / "x").exists() and not (d.project / "locked" / "y").exists()
    assert any(f" scope={sh} op=write-only-by path=locked/y decision=deny" in x for x in d.ledger)


def test_only_by_needs_programs_that_exist(start_daemon):
    with pytest.raises(Exception, match="no/such/program"):
        start_daemon(
            policy="write:\n  only_by:\n    - {paths: [a], programs: [/no/such/program]}\n"
        )


def test_writers_survive_a_restart(start_daemon):
    policy = "review:\n  - {paths: ['**'], tier: llm, wait: optional}\n"
    d = start_daemon(policy=policy)
    with client(d) as c:
        s = c.open_scope().scope_id
        assert d.exec(s, "sh", "-c", "echo x > a.txt").returncode == 0
        c.close_scope(s)
        assert c.commit(s).status == pb.OUTCOME_STATUS_HELD
        before = c.get_change_set(s)
    d.stop()
    d = start_daemon(policy=policy)
    with client(d) as c:
        after = c.get_change_set(s)
    assert [p.program for p in writers(after, "a.txt")] == [real("sh")]
    assert list(after.processes) == list(before.processes)


def test_the_sdk_sees_each_changes_writers(escrow_bin, runtime_dir):
    sdk = Sdk(escrow_bin, runtime_dir)
    out = sdk.run(
        """
        import subprocess
        seen = {}

        def look(cs):
            for ch in cs.changes:
                seen[ch.path] = [p.args[:2] for p in cs.writers(ch)]
            return escrow.commit()

        with escrow.scope("t", decide=look):
            (P / "a.txt").write_text("a\\n")
            subprocess.run(["sh", "-c", "echo b > b.txt"], check=True)
        out["seen"] = seen
        """
    )
    assert out["seen"]["a.txt"] == [[sys.executable, str(sdk.work / "app" / "app.py")]]
    assert out["seen"]["b.txt"] == [["sh", "-c"]]


def test_escrow_log_of_a_scope_prints_the_processes_it_names(daemon, escrow_bin):
    d = daemon
    with client(d) as c:
        a, b = c.open_scope().scope_id, c.open_scope().scope_id
        assert d.exec(a, "sh", "-c", "echo x > a.txt").returncode == 0
        assert d.exec(b, "sh", "-c", "echo y > b.txt").returncode == 0
    log = ["log", "--state", d.state, a]
    lines = subprocess.run([escrow_bin, *log], capture_output=True, text=True, check=True)
    lines = lines.stdout.splitlines()
    (create,) = [x for x in lines if " op=create " in x]
    named = create.split(" proc=")[1].split()[0]
    procs = [x.split()[1].removeprefix("proc=") for x in lines if x.split()[1].startswith("proc=")]
    # The shell and its parents in the sandbox, farthest first; none of scope b's.
    assert procs[-1] == named and len(procs) == len(set(procs)) >= 2
    assert all(" scope=" in x or x.split()[1].startswith("proc=") for x in lines)
    assert not any(f" scope={b} " in x for x in lines)
