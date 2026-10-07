"""Phase 1.2: copy-on-write semantics of a scope view (spike 0.3's 14 checks, on the daemon).

One scenario runs twice, on a native copy of the project and inside a scope view
mounted over the project in bwrap; trees, git status and git log must match, and
the base must stay byte-identical.
"""

import hashlib
import os
import shutil
import stat
import subprocess
from pathlib import Path

import pytest
from conftest import Daemon, make_runtime_dir, sandbox_args

import escrow

GIT_ENV = {
    "GIT_AUTHOR_NAME": "escrow",
    "GIT_AUTHOR_EMAIL": "escrow@example.invalid",
    "GIT_COMMITTER_NAME": "escrow",
    "GIT_COMMITTER_EMAIL": "escrow@example.invalid",
    "GIT_AUTHOR_DATE": "2026-10-03T12:00:00Z",
    "GIT_COMMITTER_DATE": "2026-10-03T12:00:00Z",
}

SCENARIO = """set -e
echo appended >> a.txt
mv dir/b.txt dir/b2.txt
rm dir/sub/c.txt
rm -r dir/sub
mkdir dir/sub
test -z "$(ls -A dir/sub)"
mkdir -p new/deep && echo x > new/deep/x.txt
chmod 600 a.txt
: > trunc.txt
ln -s a.txt link2
printf "edited\\n" > .ed.tmp && mv .ed.tmp edit.txt
mv vim.txt vim.txt~ && printf "vim new\\n" > vim.txt && rm vim.txt~
git add -A && git commit -qm scenario
git status --porcelain"""


def listing(root: Path) -> list[str]:
    """Names, types, modes, symlink targets and content hashes; no sizes, times or inodes."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if not (dirpath == str(root) and d == ".git"))
        for name in sorted(dirnames + filenames):
            p = Path(dirpath) / name
            st = p.lstat()
            line = f"{p.relative_to(root)}|{stat.S_IFMT(st.st_mode):o}|{st.st_mode & 0o7777:o}"
            if stat.S_ISLNK(st.st_mode):
                line += f"|{os.readlink(p)}"
            elif stat.S_ISREG(st.st_mode):
                line += f"|{hashlib.sha256(p.read_bytes()).hexdigest()}"
            out.append(line)
    return sorted(out)


def fingerprint(root: Path) -> list[str]:
    """Everything in the base including .git, with sizes and mtimes: any write changes it."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            p = Path(dirpath) / name
            st = p.lstat()
            line = f"{p.relative_to(root)}|{st.st_mode:o}|{st.st_size}|{st.st_mtime_ns}"
            if stat.S_ISREG(st.st_mode):
                line += f"|{hashlib.sha256(p.read_bytes()).hexdigest()}"
            out.append(line)
    return sorted(out)


def sh(script: str, cwd: Path) -> subprocess.CompletedProcess:
    env = {**os.environ, **GIT_ENV}
    return subprocess.run(["sh", "-c", script], cwd=cwd, env=env, capture_output=True, text=True)


class Cow:
    def __init__(self, escrow_bin, bwrap):
        self.work = make_runtime_dir()
        base = self.work / "proj"
        (base / "dir" / "sub").mkdir(parents=True)
        files = ["a.txt", "dir/b.txt", "dir/sub/c.txt", "trunc.txt", "edit.txt", "vim.txt"]
        for name in [*files, "stable.txt"]:
            (base / name).write_text(Path(name).stem[0] + "\n")
        (base / "exec.sh").write_text("#!/bin/sh\n")
        (base / "exec.sh").chmod(0o755)
        (base / "link").symlink_to("a.txt")
        git = "git -c init.defaultBranch=main init -q && git add -A && git commit -qm base"
        assert sh(git, base).returncode == 0
        self.native = self.work / "native"
        shutil.copytree(base, self.native, symlinks=True)
        self.base_before = fingerprint(base)
        self.lower_ino = (base / "stable.txt").stat().st_ino
        self.base, self.bwrap = base, bwrap
        self.daemon = Daemon(escrow_bin, self.work)

    def open_scope(self):
        with escrow.connect(str(self.daemon.socket)) as c:
            scope = c.open_scope("cow")
        self.scope_id, self.view = scope.scope_id, Path(scope.root)
        self.idx = int(self.scope_id.removeprefix("s"))

    def run(self, script: str) -> subprocess.CompletedProcess:
        args = sandbox_args(self.bwrap, self.view, self.base)
        env = {**os.environ, **GIT_ENV}
        return subprocess.run([*args, "sh", "-c", script], env=env, capture_output=True, text=True)

    def close(self):
        self.daemon.stop()
        shutil.rmtree(self.work, ignore_errors=True)


@pytest.fixture(scope="module")
def cow(escrow_bin, bwrap):
    c = Cow(escrow_bin, bwrap)
    try:
        c.open_scope()
        run_scenario(c)
    except BaseException:
        c.close()
        raise
    yield c
    c.close()


def run_scenario(c: Cow):
    # Inode checks first, as in spike 0.3: they edit stable.txt, mirrored on the native copy.
    c.ino_plain = c.run("stat -c %i stable.txt").stdout.strip()
    c.ino_after_copy_up = c.run("echo more >> stable.txt && stat -c %i stable.txt").stdout.strip()
    rename = "mv stable.txt s2 && stat -c %i s2 && mv s2 stable.txt"
    c.ino_after_rename = c.run(rename).stdout.strip()
    sh("echo more >> stable.txt", c.native)
    c.native_run = sh(SCENARIO, c.native)
    c.view_run = c.run(SCENARIO)


def scoped_ino(c: Cow, lower: int) -> str:
    return str((c.idx << 48) | lower)


def test_inode_is_scope_prefix_plus_lower_inode(cow):
    assert cow.ino_plain == scoped_ino(cow, cow.lower_ino)


def test_inode_stable_across_copy_up(cow):
    assert cow.ino_after_copy_up == scoped_ino(cow, cow.lower_ino)


def test_inode_stable_across_rename(cow):
    assert cow.ino_after_rename == scoped_ino(cow, cow.lower_ino)


def test_scenario_runs_in_view(cow):
    assert cow.view_run.returncode == 0, cow.view_run.stderr
    assert cow.view_run.stdout.strip() == ""


def test_git_status_matches_native(cow):
    assert cow.view_run.stdout == cow.native_run.stdout


def test_tree_matches_native(cow):
    assert listing(cow.view) == listing(cow.native)


def test_git_log_matches_native(cow):
    log = "git log --format='%T %s'"
    assert cow.run(log).stdout == sh(log, cow.native).stdout


def test_deleted_lower_files_hidden(cow):
    assert cow.run("test ! -e dir/sub/c.txt && test ! -e dir/b.txt").returncode == 0


def test_recreated_dir_hides_lower_contents(cow):
    assert cow.run('test -d dir/sub && test -z "$(ls -A dir/sub)"').returncode == 0


def test_mv_of_lower_directory_falls_back_on_exdev(cow):
    r = cow.run("mv dir dir-moved && test -f dir-moved/b2.txt && test ! -e dir")
    assert r.returncode == 0, r.stderr


def test_base_byte_identical(cow):
    assert fingerprint(cow.base) == cow.base_before


def test_upper_holds_staged_changes(cow):
    up = cow.daemon.upper(cow.scope_id)
    assert (up / "a.txt").is_file() and (up / "new/deep/x.txt").is_file() and (up / ".git").is_dir()


def test_upper_has_no_copy_of_untouched_files(cow):
    assert not (cow.daemon.upper(cow.scope_id) / "exec.sh").exists()


def test_view_is_mounted(cow):
    assert os.path.ismount(cow.daemon.mount)


def swap_for_symlink(project: Path, outside: Path) -> None:
    """Replace project/d with a symlink to `outside`, as an editor or another user could."""
    shutil.rmtree(project / "d")
    (project / "d").symlink_to(outside)


def test_base_symlink_swap_is_not_followed(daemon, runtime_dir):
    outside = runtime_dir / "outside"
    outside.mkdir()
    (outside / "secret").write_text("outside\n")
    (daemon.project / "d").mkdir()
    (daemon.project / "d" / "f").write_text("in\n")
    with escrow.connect(str(daemon.socket)) as c:
        sid = c.open_scope().scope_id
        view = daemon.mount / sid
        assert (view / "d" / "f").read_text() == "in\n"  # the kernel caches d as a directory
        swap_for_symlink(daemon.project, outside)
        try:
            leaked = (view / "d" / "secret").read_text()
        except OSError:
            leaked = None
        assert leaked is None
        c.discard(sid)


def test_commit_does_not_follow_a_swapped_base_dir(daemon, runtime_dir):
    outside = runtime_dir / "outside"
    outside.mkdir()
    (daemon.project / "d").mkdir()
    with escrow.connect(str(daemon.socket)) as c:
        sid = c.open_scope().scope_id
        (daemon.mount / sid / "d" / "new.txt").write_text("staged\n")
        c.close_scope(sid)
        swap_for_symlink(daemon.project, outside)
        try:
            c.commit(sid)
        except escrow.EscrowRpcError:
            pass
    assert list(outside.iterdir()) == []
