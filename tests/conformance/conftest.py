"""Fixtures shared by the conformance suite: the daemon binary, running daemons, bwrap."""

import os
import shutil
from pathlib import Path

import pytest

from escrow_testkit import Daemon, isolate_git_env, make_runtime_dir

ROOT = Path(__file__).resolve().parents[2]

isolate_git_env()


@pytest.fixture(scope="session")
def escrow_bin() -> Path:
    path = Path(os.environ.get("ESCROW_BIN", ROOT / "target" / "debug" / "escrow"))
    if not path.is_file():
        pytest.fail(f"escrow binary not found at {path}; run `cargo build` or set ESCROW_BIN")
    return path


@pytest.fixture
def runtime_dir():
    d = make_runtime_dir()
    yield d
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def start_daemon(escrow_bin, runtime_dir):
    started = []

    def start(**kw):
        d = Daemon(escrow_bin, runtime_dir, **kw)
        started.append(d)
        return d

    yield start
    for d in started:
        d.stop()


@pytest.fixture
def daemon(start_daemon):
    return start_daemon()


def ubuntu_userns_restricted() -> bool:
    try:
        flag = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        return flag.read_text().strip() == "1"
    except OSError:
        return False


@pytest.fixture(scope="session")
def bwrap() -> str:
    """escrowd's bwrap where the AppArmor userns restriction applies, else the system bwrap."""
    if env := os.environ.get("ESCROW_BWRAP"):
        return env
    if Path("/usr/lib/escrowd/bwrap").is_file():
        return "/usr/lib/escrowd/bwrap"
    if ubuntu_userns_restricted():
        pytest.fail("AppArmor restricts user namespaces: run packaging/ubuntu/install.sh first")
    path = shutil.which("bwrap")
    if not path:
        pytest.fail("bwrap not found")
    return path


def sandbox_args(bwrap: str, view: Path, project: Path, *extra_binds) -> list[str]:
    """bwrap with the scope's view mounted over the project, as escrowd's sandboxes run."""
    args = [bwrap, "--unshare-user", "--disable-userns", "--unshare-pid", "--die-with-parent"]
    args += ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"]
    for src, dst in extra_binds:
        args += ["--bind", str(src), str(dst)]
    args += ["--bind", str(view), str(project), "--chdir", str(project)]
    return args
