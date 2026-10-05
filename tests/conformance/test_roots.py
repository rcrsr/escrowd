"""Phase 2.4: path roots for $HOME and /tmp (issue #11).

`roots.home` serves the daemon's $HOME through the views: a scope child sees its scope's
home view at $HOME, captured paths commit and discard with the project's changes in one
decision, denied paths get EACCES (logged), passthrough paths are the host's own, and
ephemeral paths never reach the change set. `roots.tmp` does the same for /tmp.
"""

import os
import subprocess
import threading
import uuid
from pathlib import Path

import pytest

import escrow
from escrow.v1 import escrow_pb2 as pb

HOME_RULES = (
    "  home:\n"
    "    default: capture\n"
    "    deny: ['~/.ssh']\n"
    "    passthrough: ['~/.npm']\n"
    "    ephemeral: ['~/.bash_history']\n"
)

# Children read ~/.gitconfig: undo the suite's isolation from the host's git config.
GIT_ENV = {"GIT_CONFIG_GLOBAL": None, "GIT_CONFIG_SYSTEM": None}


def client(d):
    return escrow.connect(str(d.socket))


def open_scope(d) -> str:
    with client(d) as c:
        return c.open_scope().scope_id


def changes(cs) -> set[tuple[int, str]]:
    return {(c.kind, c.path) for c in cs.changes}


@pytest.fixture
def home(runtime_dir) -> Path:
    h = runtime_dir / "home"
    (h / ".ssh").mkdir(parents=True)
    (h / ".ssh" / "id_ed25519").write_text("private\n")
    (h / ".gitconfig").write_text("[user]\n\tname = Base\n")
    return h


@pytest.fixture
def homed(start_daemon, home):
    return start_daemon(roots=HOME_RULES, env={"HOME": str(home)})


def test_child_sees_the_scope_home_view(homed, home):
    d = homed
    sid = open_scope(d)
    r = d.exec(sid, "sh", "-c", 'echo "$HOME"; cat ~/.gitconfig', env=GIT_ENV)
    assert r.returncode == 0, r.stderr
    assert r.stdout == f"{home}\n[user]\n\tname = Base\n"


def test_home_capture_commits_with_the_project_change(homed, home):
    d = homed
    sid = open_scope(d)
    r = d.exec(
        sid, "sh", "-c", "git config --global user.name Scope && echo x > f.txt", env=GIT_ENV
    )
    assert r.returncode == 0, r.stderr
    assert "Base" in (home / ".gitconfig").read_text()  # held in escrow
    with client(d) as c:
        cs = c.close_scope(sid)
        assert changes(cs) == {
            (pb.CHANGE_KIND_MODIFY, "~/.gitconfig"),
            (pb.CHANGE_KIND_CREATE, "f.txt"),
        }
        out = c.commit(sid)
    assert out.status == pb.OUTCOME_STATUS_COMMITTED, out
    assert sorted(out.paths) == ["f.txt", "~/.gitconfig"]
    assert "name = Scope" in (home / ".gitconfig").read_text()
    assert (d.project / "f.txt").read_text() == "x\n"
    assert not [n for n in os.listdir(home) if n.startswith(".escrow-")]


def test_home_capture_discards_with_the_project_change(homed, home):
    d = homed
    sid = open_scope(d)
    r = d.exec(
        sid, "sh", "-c", "git config --global user.name Scope && echo x > f.txt", env=GIT_ENV
    )
    assert r.returncode == 0, r.stderr
    with client(d) as c:
        c.close_scope(sid)
        assert c.discard(sid).status == pb.OUTCOME_STATUS_DISCARDED
    assert "name = Base" in (home / ".gitconfig").read_text()
    assert not (d.project / "f.txt").exists()


def test_home_conflict_drops_the_whole_scope(homed, home):
    d = homed
    sid = open_scope(d)
    d.exec(sid, "sh", "-c", "echo scope >> ~/.gitconfig && echo x > f.txt")
    (home / ".gitconfig").write_text("[user]\n\tname = Editor\n")
    with client(d) as c:
        c.close_scope(sid)
        out = c.commit(sid)
    assert out.status == pb.OUTCOME_STATUS_CONFLICT
    assert list(out.paths) == ["~/.gitconfig"]
    assert not (d.project / "f.txt").exists()  # the project part was not applied either


def test_reading_a_denied_path_fails_and_is_logged(homed):
    d = homed
    sid = open_scope(d)
    r = d.exec(sid, "sh", "-c", "cat ~/.ssh/id_ed25519")
    assert r.returncode != 0 and "Permission denied" in r.stderr, r.stderr
    ledger = "\n".join(d.ledger)
    assert f" scope={sid} op=read path=~/.ssh/id_ed25519 decision=deny" in ledger
    r = d.exec(sid, "sh", "-c", "echo k > ~/.ssh/new; ls ~/.ssh")
    assert r.stderr.count("Permission denied") == 2, r.stderr
    with client(d) as c:
        cs = c.close_scope(sid)
    assert ("~/.ssh/id_ed25519", pb.READ_DECISION_DENY) in {(x.path, x.decision) for x in cs.reads}
    assert not cs.changes


def test_ephemeral_paths_never_reach_the_change_set(homed, home):
    d = homed
    sid = open_scope(d)
    r = d.exec(sid, "sh", "-c", "echo ls >> ~/.bash_history && cat ~/.bash_history")
    assert (r.returncode, r.stdout) == (0, "ls\n"), r.stderr
    with client(d) as c:
        assert not c.close_scope(sid).changes
        assert c.commit(sid).status == pb.OUTCOME_STATUS_COMMITTED
    assert not (home / ".bash_history").exists()


def test_concurrent_scopes_share_a_passthrough_cache(homed, home):
    d = homed
    a, b = open_scope(d), open_scope(d)
    script = 'for i in $(seq 50); do echo "$0" > ~/.npm/pkg-$0-$i; done; ls ~/.npm | wc -l'
    results = {}

    def run(sid):
        results[sid] = d.exec(sid, "sh", "-c", script, sid)

    threads = [threading.Thread(target=run, args=(s,)) for s in (a, b)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert all(r.returncode == 0 for r in results.values()), results
    assert len(os.listdir(home / ".npm")) == 100  # on the host at once: not escrowed
    with client(d) as c:
        for sid in (a, b):
            assert not c.close_scope(sid).changes
            assert c.commit(sid).status == pb.OUTCOME_STATUS_COMMITTED
    assert f" scope={a} op=sandbox-write path={home / '.npm'} decision=allow" in "\n".join(d.ledger)


def test_project_inside_home_is_the_project_view(start_daemon, home):
    project = home / "src" / "proj"
    d = start_daemon(roots=HOME_RULES, env={"HOME": str(home)}, project=project)
    sid = open_scope(d)
    r = d.exec(sid, "sh", "-c", f"echo x > {project}/f.txt && ls ~/src/proj")
    assert (r.returncode, r.stdout) == (0, "f.txt\n"), r.stderr
    with client(d) as c:
        assert changes(c.close_scope(sid)) == {(pb.CHANGE_KIND_CREATE, "f.txt")}
        assert c.commit(sid).status == pb.OUTCOME_STATUS_COMMITTED
    assert (project / "f.txt").read_text() == "x\n"


def test_escrowd_state_and_views_stay_hidden_in_home(start_daemon, escrow_bin, runtime_dir):
    home = runtime_dir  # the work directory holds the state, the views and the sockets
    d = start_daemon(roots=HOME_RULES, env={"HOME": str(home)})
    sid = open_scope(d)
    r = d.exec(sid, "sh", "-c", "ls -A ~; mkdir ~/state")
    names = r.stdout.split()
    assert "proj" in names and "policy.yaml" in names
    for hidden in ("state", "mnt", "escrow.sock", "escrow.sock.exec"):
        assert hidden not in names, r.stdout
    assert "No such file" in r.stderr, r.stderr  # escrowd's own paths cannot be created either


SIGN = """#!/bin/sh
cat >/dev/null
printf -- '-----BEGIN PGP SIGNATURE-----\\nfake\\n-----END PGP SIGNATURE-----\\n'
printf '\\n[GNUPG:] SIG_CREATED D 1 8 00 0 fake\\n' >&2
"""


@pytest.mark.parametrize("listed", [True, False])
def test_git_commit_signs_with_a_listed_helper(start_daemon, home, listed):
    helper = home / "bin" / "sign"
    helper.parent.mkdir()
    helper.write_text(SIGN)
    helper.chmod(0o755)
    (home / ".gitconfig").write_text(
        f"[user]\n\tname = T\n\temail = t@example.com\n"
        f"[gpg]\n\tprogram = {helper}\n[commit]\n\tgpgsign = true\n"
    )
    capture = "['~/.gitconfig', '~/bin']" if listed else "['~/.gitconfig']"
    d = start_daemon(roots=f"  home:\n    capture: {capture}\n", env={"HOME": str(home)})
    subprocess.run(["git", "init", "-q", str(d.project)], check=True)
    sid = open_scope(d)
    r = d.exec(sid, "sh", "-c", "echo x > f && git add f && git commit -qm m", env=GIT_ENV)
    if listed:
        assert r.returncode == 0, r.stderr
    else:
        assert r.returncode != 0 and "sign" in r.stderr, r.stderr


@pytest.fixture
def tmp_rooted(start_daemon):
    return start_daemon(roots="  tmp: {default: ephemeral}\n")


def test_tmp_is_a_per_scope_scratch_layer(tmp_rooted):
    d = tmp_rooted
    name = f"/tmp/escrow-root-test-{uuid.uuid4().hex}"
    Path(name).write_text("host\n")
    try:
        a, b = open_scope(d), open_scope(d)
        r = d.exec(a, "sh", "-c", f"cat {name}; echo a > {name}; cat {name}")
        assert (r.returncode, r.stdout) == (0, "host\na\n"), r.stderr
        assert d.exec(b, "cat", name).stdout == "host\n"  # scopes do not share it
        assert Path(name).read_text() == "host\n"
        with client(d) as c:
            cs = c.close_scope(a)
            assert not cs.changes
            assert (name, pb.READ_DECISION_ALLOW) in {(x.path, x.decision) for x in cs.reads}
            assert c.commit(a).status == pb.OUTCOME_STATUS_COMMITTED
        assert Path(name).read_text() == "host\n"
    finally:
        os.unlink(name)


def test_passthrough_root_default_needs_no_lists(escrow_bin, runtime_dir):
    from conftest import Daemon

    with pytest.raises(RuntimeError, match="default passthrough"):
        Daemon(escrow_bin, runtime_dir, roots="  tmp: {default: passthrough, deny: ['/tmp/x']}\n")
