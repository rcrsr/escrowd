"""Phase 1.1: the sandbox runs unprivileged and blocks nested user namespaces (as in spike 0.2)."""

import subprocess


def run_in_bwrap(bwrap, *cmd, binds=()):
    args = [
        bwrap,
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
        "--tmpfs",
        "/tmp",
    ]
    for src, dst in binds:
        args += ["--bind", src, dst]
    return subprocess.run([*args, "--", *cmd], capture_output=True, text=True, timeout=30)


def test_bwrap_starts_a_sandbox(bwrap):
    r = run_in_bwrap(bwrap, "true")
    assert r.returncode == 0, r.stderr


def test_bind_mount_over_project(bwrap, tmp_path):
    src, project = tmp_path / "view", tmp_path / "project"
    src.mkdir()
    project.mkdir()
    (src / "marker").write_text("view\n")
    r = run_in_bwrap(bwrap, "cat", f"{project}/marker", binds=[(src, project)])
    assert r.returncode == 0, r.stderr
    assert r.stdout == "view\n"


def test_nested_user_namespace_is_blocked(bwrap):
    r = run_in_bwrap(bwrap, "unshare", "-Ur", "true")
    assert r.returncode != 0, "a sandboxed process created a nested user namespace"
