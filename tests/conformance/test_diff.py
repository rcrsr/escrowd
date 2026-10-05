"""Phase 2.5: the change set's content diff (#12).

The diff is against the scope's snapshot, so another scope's commit never shows up in
it; renames are renames; binary and over-cap files are summarized, and paths under
read.deny show no content. `git apply` of the diff on a copy of the snapshot yields
the staged tree, which is what `diff -ru` between the two would show.
"""

import hashlib
import os
import shutil
import stat
import subprocess

import grpc
import pytest
from scenario import mutate, seed, tree

import escrow
from escrow.v1 import escrow_pb2 as pb


def client(d):
    return escrow.connect(str(d.socket))


def files(root):
    """Files and symlinks with mode and content: what a diff can carry (no directories)."""
    return {
        rel: (mode, body)
        for rel, mode, body in tree(root)
        if (stat.S_ISREG(mode) or stat.S_ISLNK(mode)) and not rel.startswith(".git/")
    }


def headers(diff: str) -> set[str]:
    """Both paths of every `diff --git a/X b/Y` line, and the path of every mode note
    (test paths need no quoting)."""
    out = set()
    for line in diff.splitlines():
        if line.startswith("diff --git a/"):
            a, b = line.removeprefix("diff --git a/").split(" b/")
            out |= {a, b}
        elif line.startswith("# escrow: mode of b/"):
            out.add(line.removeprefix("# escrow: mode of b/").rsplit(": ", 1)[0])
    return out


def edit(root):
    """Edits beyond the shared scenario: a hunk mid-file, a file without a final
    newline, a rename with an edit, a rename over an existing file, a space in a name."""
    (root / "multi.txt").write_text("".join(f"line {i}\n" for i in range(20)).replace("10", "X"))
    (root / "nonl.txt").write_text("a\nc")
    os.rename(root / "mv.txt", root / "moved.txt")
    with open(root / "moved.txt", "a") as f:
        f.write("appended\n")
    os.rename(root / "over-src.txt", root / "over-dst.txt")
    (root / "space name.txt").write_text("two\n")


def seed_more(project):
    seed(project)
    (project / "multi.txt").write_text("".join(f"line {i}\n" for i in range(20)))
    (project / "nonl.txt").write_text("a\nb")
    (project / "mv.txt").write_text("moving\n" * 5)
    (project / "over-src.txt").write_text("src\n")
    (project / "over-dst.txt").write_text("dst\n")
    (project / "space name.txt").write_text("one\n")


def test_diff_applied_to_the_snapshot_gives_the_staged_tree(start_daemon):
    d = start_daemon(deny_read=())  # the check reads every staged file, .env too
    seed_more(d.project)
    snap = d.work / "snap"
    shutil.copytree(d.project, snap, symlinks=True)
    before = files(snap)
    with client(d) as c:
        sid = c.open_scope().scope_id
        root = d.mount / sid
        mutate(root)
        edit(root)
        staged = files(root)
        cs = c.close_scope(sid)
        c.discard(sid)
    changed = {p for p in before.keys() | staged.keys() if before.get(p) != staged.get(p)}
    assert headers(cs.diff) == changed
    assert "rename from old.txt\nrename to renamed.txt\n" in cs.diff
    assert "rename from mv.txt\nrename to moved.txt\n" in cs.diff
    mode = stat.S_IMODE(before["mode.txt"][0])
    assert f"# escrow: mode of b/mode.txt: {0o100000 | mode:o} -> 100600\n" in cs.diff
    subprocess.run(["git", "init", "-q"], cwd=snap, check=True)
    apply = ["git", "apply", "-"]
    r = subprocess.run(apply, cwd=snap, input=cs.diff, text=True, capture_output=True)
    assert r.returncode == 0, r.stderr + cs.diff
    applied = files(snap)
    # git apply sets the executable bit only; mode.txt's 0600 is in the diff (above).
    assert {p: v[1] for p, v in applied.items()} == {p: v[1] for p, v in staged.items()}


def test_diff_is_against_the_snapshot_not_the_live_base(daemon):
    d = daemon
    seed(d.project)
    with client(d) as c:
        a = c.open_scope().scope_id
        b = c.open_scope().scope_id
        (d.mount / b / "edit.txt").write_text("from B\n")
        (d.mount / b / "keep.txt").write_text("B too\n")
        c.close_scope(b)
        assert c.commit(b).status == pb.OUTCOME_STATUS_COMMITTED
        (d.mount / a / "edit.txt").write_text("from A\n")
        cs = c.close_scope(a)
        c.discard(a)
    assert (d.project / "edit.txt").read_text() == "from B\n"
    assert headers(cs.diff) == {"edit.txt"}
    assert "-edit\n+from A\n" in cs.diff
    assert "from B" not in cs.diff and "B too" not in cs.diff


def test_binary_and_over_cap_files_are_summarized(start_daemon):
    d = start_daemon(policy="diff:\n  file_bytes: 1000\n")
    blob, big, latin = bytes(range(256)) * 2, b"x\n" * 1000, "caf\xe9\n".encode("latin-1")
    sha = {n: hashlib.sha256(b).hexdigest() for n, b in [("b", blob), ("g", big), ("l", latin)]}
    with client(d) as c:
        sid = c.open_scope().scope_id
        (d.mount / sid / "blob.bin").write_bytes(blob)
        (d.mount / sid / "big.txt").write_bytes(big)
        (d.mount / sid / "latin.txt").write_bytes(latin)
        (d.mount / sid / "small.txt").write_text("small\n")
        cs = c.close_scope(sid)
        c.discard(sid)
    summaries = [
        f"Binary files /dev/null and b/blob.bin differ (absent -> 512 bytes, sha256 {sha['b']})",
        f"Files /dev/null and b/big.txt differ (absent -> 2000 bytes, sha256 {sha['g']})",
        f"Binary files /dev/null and b/latin.txt differ (absent -> 5 bytes, sha256 {sha['l']})",
    ]
    for line in summaries:
        assert line + "\n" in cs.diff
    assert "+small\n" in cs.diff and "+x\n" not in cs.diff


def test_diff_stops_at_max_bytes(start_daemon):
    d = start_daemon(policy="diff:\n  max_bytes: 400\n")
    with client(d) as c:
        sid = c.open_scope().scope_id
        for i in range(10):
            (d.mount / sid / f"f{i}.txt").write_text(f"{i}\n" * 20)
        cs = c.close_scope(sid)
        c.discard(sid)
    body, last = cs.diff.rstrip("\n").rsplit("\n", 1)
    assert len(body) + 1 <= 400
    shown = len(headers(cs.diff))
    assert 0 < shown < 10
    left = 10 - shown
    assert (
        last == f"# escrow: {left} more changed file(s) left out of the diff (diff.max_bytes 400)"
    )


def test_read_denied_content_is_withheld(daemon):
    d = daemon
    seed(d.project)  # .env holds SECRET=1; the policy denies reads of .env
    with client(d) as c:
        sid = c.open_scope().scope_id
        (d.mount / sid / ".env").write_text("SECRET=2\n")
        cs = c.close_scope(sid)
        c.discard(sid)
    assert "diff --git a/.env b/.env\nFiles a/.env and b/.env: content withheld (read.deny)\n" in (
        cs.diff
    )
    assert "SECRET" not in cs.diff


def test_home_paths_show_as_the_change_set_shows_them(start_daemon, runtime_dir):
    home = runtime_dir / "home"
    home.mkdir()
    (home / ".gitconfig").write_text("[user]\n\tname = Base\n")
    d = start_daemon(roots="  home:\n    default: capture\n", env={"HOME": str(home)})
    with client(d) as c:
        sid = c.open_scope().scope_id
        (d.mount / f"{sid}.home" / ".gitconfig").write_text("[user]\n\tname = Scope\n")
        cs = c.close_scope(sid)
        c.discard(sid)
    assert "diff --git a/~/.gitconfig b/~/.gitconfig\n--- a/~/.gitconfig\n+++ b/~/.gitconfig\n" in (
        cs.diff
    )
    assert "-\tname = Base\n+\tname = Scope\n" in cs.diff


def test_escrow_diff_prints_a_closed_scope(daemon):
    d = daemon
    seed(d.project)

    def escrow_diff(sid):
        args = [d.bin, "diff", "--socket", d.socket, sid]
        return subprocess.run(args, capture_output=True, text=True, timeout=30)

    with client(d) as c:
        sid = c.open_scope().scope_id
        (d.mount / sid / "edit.txt").write_text("edited\n")
        r = escrow_diff(sid)
        assert r.returncode != 0 and "not closed" in r.stderr
        cs = c.close_scope(sid)
        r = escrow_diff(sid)
        assert r.returncode == 0, r.stderr
        assert r.stdout == cs.diff == c.get_change_set(sid).diff
        c.discard(sid)
        with pytest.raises(grpc.RpcError) as e:
            c.get_change_set(sid)
        assert e.value.code() == grpc.StatusCode.NOT_FOUND
