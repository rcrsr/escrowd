"""Phase 4.2: the policy file and the reviewer programs are protected.

A change to one of them through any view (a scope child, or the host through
`<mount>/unscoped/`) gets EACCES and a `decision=deny` ledger line, whatever the
rules say; reads work. A folder above a protected path cannot be renamed or removed,
but its mode may change. A start that would leave a protected path outside escrow
(`--unscoped passthrough` over the project, a passthrough path over a program) is
refused, and `escrow daemon --check` prints the protected paths.
"""

import os
import shutil
import subprocess
from pathlib import Path

import pytest

from escrow_testkit import Daemon

HOME_RULES = "  home:\n    default: capture\n"


class InProject(Daemon):
    """A daemon whose policy file lives at `<project>/ci/policy.yaml`."""

    def _start(self) -> None:
        if self.policy.parent == self.work:
            target = self.project / "ci" / "policy.yaml"
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.move(self.policy, target)
            self.argv[self.argv.index("--policy") + 1] = target
            self.policy = target
        super()._start()


def make_program(directory: Path, name: str = "lint") -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    prog = directory / name
    prog.write_text("#!/bin/sh\nexit 0\n")
    prog.chmod(0o755)
    return prog


def reviewer_policy(prog: Path) -> str:
    return f"reviewers:\n  lint: {{run: ['{prog}']}}\n"


def open_scope(d) -> str:
    with d.client() as c:
        return c.open_scope().scope_id


def denied(d, scope: str, op: str, path: str) -> bool:
    """A `decision=deny` line of `scope` for `op` on `path` (a `proc=` field may sit between)."""
    return any(
        f" scope={scope} op={op} path={path} " in line and line.endswith(" decision=deny")
        for line in d.ledger
    )


@pytest.fixture
def implicit(escrow_bin, runtime_dir):
    d = InProject(escrow_bin, runtime_dir, unscoped="implicit")
    yield d
    d.stop()


@pytest.mark.parametrize(
    ("script", "op"),
    [
        ("echo x >> {p}", "open-write"),
        ("rm {p}", "unlink"),
        ("mv {parent} {parent}.moved", "rename"),
        ("chmod 600 {p}", "setattr"),
    ],
)
def test_a_scope_child_cannot_change_the_policy_file(implicit, script, op):
    d = implicit
    sid = open_scope(d)
    policy = d.project / "ci" / "policy.yaml"
    before = policy.read_text()

    r = d.exec(sid, "sh", "-c", script.format(p=policy, parent=policy.parent))
    read = d.exec(sid, "cat", str(policy))

    assert r.returncode != 0 and "Permission denied" in r.stderr, r.stderr
    assert read.returncode == 0 and read.stdout == before
    assert denied(d, sid, op, "ci/policy.yaml" if op != "rename" else "ci")
    assert policy.read_text() == before


def test_a_mode_change_on_the_folder_above_is_allowed(implicit):
    d = implicit
    sid = open_scope(d)

    r = d.exec(sid, "chmod", "755", str(d.project / "ci"))

    assert r.returncode == 0, r.stderr


def test_the_host_cannot_write_the_policy_file_through_the_unscoped_view(implicit):
    d = implicit
    view = d.mount / "unscoped" / "ci" / "policy.yaml"

    with pytest.raises(PermissionError):
        view.open("a").write("x\n")

    assert denied(d, "unscoped", "open-write", "ci/policy.yaml")
    assert view.read_text() == (d.project / "ci" / "policy.yaml").read_text()


def test_a_reviewer_program_in_a_served_home_is_protected(escrow_bin, runtime_dir):
    home = runtime_dir / "home"
    prog = make_program(home / "bin")
    d = Daemon(
        escrow_bin,
        runtime_dir,
        roots=HOME_RULES,
        policy=reviewer_policy(prog),
        env={"HOME": str(home)},
    )
    try:
        sid = open_scope(d)

        r = d.exec(sid, "sh", "-c", 'echo x >> "$HOME/bin/lint"')
        read = d.exec(sid, "cat", str(prog))
        other = d.exec(sid, "sh", "-c", 'echo ok > "$HOME/bin/other"')

        assert r.returncode != 0 and "Permission denied" in r.stderr, r.stderr
        assert read.returncode == 0 and read.stdout == "#!/bin/sh\nexit 0\n"
        assert other.returncode == 0, other.stderr
        assert denied(d, sid, "open-write", "~/bin/lint")
    finally:
        d.stop()


def test_unscoped_passthrough_is_refused_with_a_policy_in_the_project(escrow_bin, runtime_dir):
    with pytest.raises(
        RuntimeError,
        match=r"unscoped passthrough: .*ci/policy\.yaml is protected and lies in the project",
    ):
        InProject(escrow_bin, runtime_dir)


def test_a_passthrough_path_over_a_reviewer_program_is_refused(escrow_bin, runtime_dir):
    bindir = runtime_dir / "tools"
    prog = make_program(bindir)

    with pytest.raises(RuntimeError, match=f"passthrough {bindir} contains protected {prog}"):
        Daemon(escrow_bin, runtime_dir, sandbox_write=(str(bindir),), policy=reviewer_policy(prog))


def test_check_prints_the_protected_paths_and_creates_no_state(escrow_bin, runtime_dir):
    project = runtime_dir / "proj"
    project.mkdir()
    prog = make_program(runtime_dir / "tools")
    link = runtime_dir / "link"
    link.symlink_to(prog)
    policy = runtime_dir / "policy.yaml"
    policy.write_text(f"version: 1\nreviewers:\n  lint: {{run: ['{link}']}}\n")
    state, mount = runtime_dir / "state", runtime_dir / "mnt"

    r = subprocess.run(
        [escrow_bin, "daemon", "--check", "--unscoped", "implicit", "--project", project]
        + ["--state", state, "--mount", mount, "--policy", policy],
        capture_output=True,
        text=True,
        timeout=30,
        env=os.environ,
    )

    paths = {os.path.realpath(p) for p in (policy, prog)} | {str(policy), str(link), str(prog)}
    lines = r.stdout.splitlines()
    assert r.returncode == 0, r.stderr
    assert lines == sorted(lines)
    assert lines == [f"protected: {p}" for p in sorted(paths)]
    assert not state.exists() and not mount.exists()
