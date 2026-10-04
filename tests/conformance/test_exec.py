"""Phase 1.5: scope children through the exec socket.

`escrow exec --scope ID -- cmd` asks the daemon to start cmd in bwrap with the scope's
view over the project (exit test 2). The caller keeps ordinary child semantics: stdio,
exit codes and signals pass through, and a dead caller takes its child along. A scope's
children stop when it closes. The sandbox shows the child nothing of escrowd: no state
directory, no other scope's view, no socket, nothing outside the bind list.
"""

import os
import signal
import subprocess
import time

import pytest

import escrow
from escrow.v1 import escrow_pb2 as pb


def client(d):
    return escrow.connect(str(d.socket))


def open_scope(d):
    with client(d) as c:
        return c.open_scope().scope_id


def test_subprocess_write_lands_in_its_scope(daemon):
    d = daemon
    (d.project / "f.txt").write_text("base\n")
    a, b = open_scope(d), open_scope(d)
    r = d.exec(a, "bash", "-c", "echo x > f.txt; echo y > new.txt")
    assert r.returncode == 0, r.stderr
    assert (d.project / "f.txt").read_text() == "base\n"
    assert not (d.project / "new.txt").exists()
    assert (d.mount / a / "f.txt").read_text() == "x\n"
    assert (d.mount / b / "f.txt").read_text() == "base\n"
    with client(d) as c:
        cs = c.close_scope(a)
    assert [(ch.path, ch.kind) for ch in cs.changes] == [
        ("f.txt", pb.CHANGE_KIND_MODIFY),
        ("new.txt", pb.CHANGE_KIND_CREATE),
    ]
    assert any(f" scope={a} op=create path=new.txt " in line for line in d.ledger)
    assert not any(f" scope={b} op=create" in line for line in d.ledger)


def test_stdio_and_exit_code_pass_through(daemon):
    sid = open_scope(daemon)
    r = daemon.exec(sid, "sh", "-c", "cat; echo err >&2; exit 7", input="from stdin\n")
    assert (r.returncode, r.stdout, r.stderr) == (7, "from stdin\n", "err\n")


def test_cwd_inside_the_project_is_kept(daemon):
    (daemon.project / "sub").mkdir()
    sid = open_scope(daemon)
    r = daemon.exec(sid, "pwd", cwd=daemon.project / "sub")
    assert r.stdout.strip() == str(daemon.project / "sub")
    r = daemon.exec(sid, "pwd", cwd="/")
    assert r.stdout.strip() == str(daemon.project)


def test_unknown_or_closed_scope_is_refused(daemon):
    r = daemon.exec("s999", "true")
    assert r.returncode == 125 and "no scope s999" in r.stderr
    sid = open_scope(daemon)
    with client(daemon) as c:
        c.close_scope(sid)
    r = daemon.exec(sid, "true")
    assert r.returncode == 125 and "closed" in r.stderr


def spawn(d, sid, script):
    env = {**os.environ, "ESCROW_SOCKET": str(d.socket)}
    return subprocess.Popen(
        d.exec_args(sid, "sh", "-c", script), env=env, stdout=subprocess.PIPE, text=True
    )


def wait_for(path, timeout=10):
    deadline = time.monotonic() + timeout
    while not path.exists():
        assert time.monotonic() < deadline, f"{path} never appeared"
        time.sleep(0.02)


def test_signals_reach_the_child(daemon):
    sid = open_scope(daemon)
    script = "trap 'echo got TERM; exit 3' TERM; touch ready; while :; do sleep 0.05; done"
    p = spawn(daemon, sid, script)
    wait_for(daemon.mount / sid / "ready")
    p.send_signal(signal.SIGTERM)
    out, _ = p.communicate(timeout=10)
    assert (p.returncode, out) == (3, "got TERM\n")


def test_a_dead_caller_takes_its_child_along(daemon):
    sid = open_scope(daemon)
    tick = daemon.mount / sid / "tick"
    p = spawn(daemon, sid, "while :; do date +%s%N > tick; sleep 0.05; done")
    wait_for(tick)
    p.kill()
    p.wait()
    time.sleep(0.5)
    last = tick.read_text()
    time.sleep(0.5)
    assert tick.read_text() == last


def test_close_stops_the_scopes_children_after_their_writes(daemon):
    sid = open_scope(daemon)
    p = spawn(daemon, sid, "echo kept > kept.txt; touch ready; exec sleep 600")
    wait_for(daemon.mount / sid / "ready")
    start = time.monotonic()
    with client(daemon) as c:
        cs = c.close_scope(sid)
    assert time.monotonic() - start < 5
    assert p.wait(timeout=10) == 128 + signal.SIGKILL
    assert "kept.txt" in [ch.path for ch in cs.changes]


@pytest.fixture
def contained(start_daemon, runtime_dir):
    """A daemon whose policy lets sandboxes read the whole work directory, which holds the
    project, the state, the views and the sockets: escrowd must still hide its own parts."""
    (runtime_dir / "canary.txt").write_text("readable\n")
    d = start_daemon(sandbox_read=(str(runtime_dir),))
    a, b = open_scope(d), open_scope(d)
    (d.mount / a / "secret-a.txt").write_text("a only\n")
    return d, a, b


def test_sandbox_read_paths_are_visible(contained):
    d, _, b = contained
    r = d.exec(b, "cat", str(d.work / "canary.txt"))
    assert r.stdout == "readable\n", r.stderr


def test_child_cannot_find_another_scopes_files(contained):
    d, a, b = contained
    # Everything but /proc and the read-only system directories (large on CI runners).
    pruned = " -o ".join(f"-path {p}" for p in ("/proc", "/usr", "/opt", "/etc", "/sys"))
    find = f"find / \\( {pruned} \\) -prune -o -name 'secret-a*' -print"
    r = d.exec(b, "sh", "-c", find)
    assert (r.returncode, r.stdout) == (0, ""), r.stderr
    assert d.exec(a, "sh", "-c", find).stdout == f"{d.project}/secret-a.txt\n"  # the probe works


def test_child_sees_no_state_views_or_socket(contained):
    d, a, b = contained
    for hidden in (d.state, d.mount):
        r = d.exec(b, "ls", "-A", str(hidden))
        assert (r.returncode, r.stdout) == (0, ""), f"{hidden}: {r.stdout}{r.stderr}"
    assert d.exec(b, "ls", "/escrow").returncode != 0
    probe = (
        "import socket, sys\n"
        "for p in sys.argv[1:]:\n"
        "    try:\n"
        "        socket.socket(socket.AF_UNIX).connect(p); print('connected', p)\n"
        "    except OSError as e: print('refused', p, e.errno)\n"
    )
    r = d.exec(b, "python3", "-c", probe, str(d.socket), f"{d.socket}.exec")
    assert "connected" not in r.stdout, r.stdout + r.stderr
    assert r.stdout.count("refused") == 2


def test_home_and_unlisted_paths_are_absent(daemon, runtime_dir):
    (runtime_dir / "canary.txt").write_text("hidden\n")
    sid = open_scope(daemon)
    r = daemon.exec(sid, "sh", "-c", f"cat {runtime_dir}/canary.txt; ls -A $HOME | wc -l")
    assert "No such file" in r.stderr
    assert r.stdout.strip() == "0"  # $HOME is an empty tmpfs
