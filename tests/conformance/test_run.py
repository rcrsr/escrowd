"""Phase 1.5: `escrow run` and the unscoped modes (exit test 7).

The launcher starts the daemon and runs the app in bwrap. The app sees the project as
its unscoped mode says (passthrough: the real directory; implicit: a default scope;
deny: a read-only view), every scope under /escrow, the socket at /run/escrowd, and
nothing else of the host besides the system directories and the policy's read paths.
"""

import os
import subprocess
import time

import pytest
from conftest import accepts, write_policy

import escrow
from escrow.v1 import escrow_pb2 as pb


class App:
    """`escrow run` with its state, views and socket under one work directory."""

    def __init__(
        self,
        bin,
        work,
        mode,
        script,
        on_exit=None,
        deny_read=(".env",),
        sandbox_read=(),
        env=None,
        roots="",
    ):
        self.work = work
        self.project = work / "proj"
        self.project.mkdir(exist_ok=True)
        self.state, self.mount, self.socket = work / "state", work / "mnt", work / "s.sock"
        policy = write_policy(work, deny_read, sandbox_read, roots=roots)
        args = [bin, "run", "--project", self.project, "--unscoped", mode, "--policy", policy]
        args += ["--state", self.state, "--mount", self.mount, "--socket", self.socket]
        if on_exit:
            args += ["--on-exit", on_exit]
        self.proc = subprocess.Popen(
            [*args, "--", "sh", "-c", script],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env={**os.environ, **(env or {})},
        )

    def wait_ready(self):
        deadline = time.monotonic() + 10
        while not accepts(self.socket):
            assert self.proc.poll() is None, self.proc.communicate()
            assert time.monotonic() < deadline, "escrow run never opened its socket"
            time.sleep(0.02)

    def finish(self, stdin=""):
        out, err = self.proc.communicate(stdin, timeout=60)
        return self.proc.returncode, out, err

    @property
    def ledger(self):
        return (self.state / "ledger.log").read_text().splitlines()


@pytest.fixture
def app(escrow_bin, runtime_dir):
    started = []

    def start(mode, script, **kw):
        a = App(escrow_bin, runtime_dir, mode, script, **kw)
        started.append(a)
        return a

    yield start
    for a in started:
        if a.proc.poll() is None:
            a.proc.kill()
            a.proc.wait()
        subprocess.run(["fusermount3", "-u", "-z", a.mount], capture_output=True)


def run(app, mode, script, **kw):
    a = app(mode, script, **kw)
    return a, *a.finish()


def test_passthrough_writes_reach_the_project(app, runtime_dir):
    (runtime_dir / "proj").mkdir()
    (runtime_dir / "proj" / "f.txt").write_text("base\n")
    a, code, out, err = run(app, "passthrough", "cat f.txt; echo new > g.txt")
    assert (code, out) == (0, "base\n"), err
    assert (a.project / "g.txt").read_text() == "new\n"


def test_implicit_captures_and_discards_at_exit(app):
    a, code, out, err = run(app, "implicit", "echo new > g.txt; cat g.txt")
    assert (code, out) == (0, "new\n"), err
    assert not (a.project / "g.txt").exists()
    assert "1 unscoped change(s) discarded" in err
    assert any(" scope=unscoped op=create path=g.txt " in line for line in a.ledger)


def test_implicit_commits_at_exit_when_asked(app):
    a, code, _, err = run(app, "implicit", "echo new > g.txt", on_exit="commit")
    assert code == 0 and "1 unscoped change(s) committed" in err
    assert (a.project / "g.txt").read_text() == "new\n"


def test_settle_unscoped_mid_run_and_keep_writing(app):
    a = app("implicit", "echo one > a.txt; read go; echo two > b.txt", on_exit="commit")
    a.wait_ready()
    deadline = time.monotonic() + 10
    while not (a.state / "scopes" / "unscoped" / "upper" / "a.txt").exists():
        assert time.monotonic() < deadline
        time.sleep(0.02)
    with escrow.connect(str(a.socket)) as c:
        cs = c.settle_unscoped()
        assert cs.scope_id == "unscoped"
        assert [(ch.path, ch.kind) for ch in cs.changes] == [("a.txt", pb.CHANGE_KIND_CREATE)]
        assert c.commit("unscoped").status == pb.OUTCOME_STATUS_COMMITTED
    assert (a.project / "a.txt").read_text() == "one\n"
    code, _, err = a.finish("go\n")  # the app keeps its bind mount through the reset
    assert code == 0, err
    assert (a.project / "b.txt").read_text() == "two\n"


def test_deny_refuses_writes_and_gates_reads(app, runtime_dir):
    (runtime_dir / "proj").mkdir()
    for name, text in {"f.txt": "base\n", ".env": "SECRET=1\n"}.items():
        (runtime_dir / "proj" / name).write_text(text)
    script = "cat f.txt; cat .env; echo x > g.txt; echo x >> f.txt; mkdir d"
    a, code, out, err = run(app, "deny", script)
    assert out == "base\n"
    assert ".env: Permission denied" in err
    assert err.count("Read-only file system") == 3
    assert sorted(os.listdir(a.project)) == [".env", "f.txt"]
    assert (a.project / "f.txt").read_text() == "base\n"
    assert any(" scope=unscoped op=read path=.env decision=deny" in line for line in a.ledger)


def test_exit_code_passes_through(app):
    assert run(app, "deny", "exit 5")[1] == 5


def test_app_sees_scopes_and_socket_but_not_escrowd_state(app, runtime_dir):
    (runtime_dir / "canary.txt").write_text("c\n")
    script = (
        "ls /escrow; test -S /run/escrowd/escrow.sock && echo socket; "
        f"ls -A {runtime_dir}/state | wc -l; ls -A {runtime_dir}/mnt | wc -l; "
        f"cat {runtime_dir}/canary.txt"
    )
    _, code, out, err = run(app, "deny", script, sandbox_read=(str(runtime_dir),))
    assert out.splitlines() == ["unscoped", "socket", "0", "0", "c"], out + err  # c: a read path


def test_app_runs_scope_children_through_escrow_exec(app, escrow_bin):
    script = f'read sid; {escrow_bin} exec --scope "$sid" -- sh -c "echo x > f.txt; ls /escrow"'
    a = app("deny", script)
    a.wait_ready()
    with escrow.connect(str(a.socket)) as c:
        sid = c.open_scope().scope_id
    code, out, err = a.finish(sid + "\n")
    assert code != 0 and "/escrow" in err  # the scope child sees no other views
    assert not (a.project / "f.txt").exists()
    assert any(f" scope={sid} op=create path=f.txt " in line for line in a.ledger)


@pytest.mark.parametrize("on_exit", ["commit", "discard"])
def test_implicit_mode_settles_home_with_the_project(app, runtime_dir, on_exit):
    home = runtime_dir / "home"
    home.mkdir()
    (home / ".profile").write_text("base\n")
    roots = "  home: {default: capture}\n  tmp: {default: ephemeral}\n"
    script = 'echo app > ~/.profile; echo t > /tmp/scratch; echo x > f.txt; echo "$HOME"'
    a, code, out, err = run(
        app, "implicit", script, on_exit=on_exit, env={"HOME": str(home)}, roots=roots
    )
    assert code == 0, err
    assert out == f"{home}\n"
    verb = {"commit": "committed", "discard": "discarded"}[on_exit]
    assert f"2 unscoped change(s) {verb}" in err, err  # ~/.profile and f.txt; /tmp is ephemeral
    committed = on_exit == "commit"
    assert (home / ".profile").read_text() == ("app\n" if committed else "base\n")
    assert (a.project / "f.txt").exists() == committed


def test_unscoped_modes_with_roots(app, runtime_dir):
    home = runtime_dir / "home"
    home.mkdir()
    (home / ".profile").write_text("base\n")
    roots = "  home: {default: capture}\n  tmp: {default: ephemeral}\n"
    # deny: $HOME reads the base, captured paths are read-only, ephemeral /tmp is writable.
    script = "cat ~/.profile; echo t > /tmp/x && cat /tmp/x; echo y > ~/.profile"
    _, code, out, err = run(app, "deny", script, env={"HOME": str(home)}, roots=roots)
    assert out == "base\nt\n", err
    assert "Read-only file system" in err
    # passthrough: no unscoped scope serves the roots: $HOME and /tmp are empty tmpfs.
    script = "ls -A ~ /tmp | grep -c . ; echo t > /tmp/x && cat /tmp/x"
    _, code, out, err = run(app, "passthrough", script, env={"HOME": str(home)}, roots=roots)
    assert (code, out) == (0, "2\nt\n"), err
