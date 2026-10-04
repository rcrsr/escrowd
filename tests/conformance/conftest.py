"""Fixtures shared by the conformance suite: the daemon binary, running daemons, bwrap."""

import os
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture(scope="session")
def escrow_bin() -> Path:
    path = Path(os.environ.get("ESCROW_BIN", ROOT / "target" / "debug" / "escrow"))
    if not path.is_file():
        pytest.fail(f"escrow binary not found at {path}; run `cargo build` or set ESCROW_BIN")
    return path


def make_runtime_dir() -> Path:
    # Unix socket paths are capped at 108 bytes, so stay out of pytest's long tmp paths;
    # Ubuntu 26.04 confines fusermount3 to /run/user/<uid>, /tmp and a few other roots.
    base = os.environ.get("XDG_RUNTIME_DIR") or tempfile.gettempdir()
    return Path(tempfile.mkdtemp(prefix="escrow-test.", dir=base))


@pytest.fixture
def runtime_dir():
    d = make_runtime_dir()
    yield d
    shutil.rmtree(d, ignore_errors=True)


class Daemon:
    """`escrow daemon` with its socket, project, state and mount under one work directory."""

    def __init__(self, bin: Path, work: Path, deny_read=(".env",), project: Path | None = None):
        """`deny_read` becomes the policy file's read.deny list."""
        self.work = work
        self.socket = work / "escrow.sock"
        self.project = project or work / "proj"
        self.state = work / "state"
        self.mount = work / "mnt"
        self.project.mkdir(parents=True, exist_ok=True)
        self.policy = work / "policy.yaml"
        deny = ", ".join(f"'{g}'" for g in deny_read)
        self.policy.write_text(f"version: 1\nread:\n  deny: [{deny}]\n")
        args = [bin, "daemon", "--socket", self.socket, "--project", self.project]
        args += ["--state", self.state, "--mount", self.mount, "--policy", self.policy]
        self.log = open(work / "daemon.log", "ab")
        self.proc = subprocess.Popen(args, stderr=self.log)
        self.wait_ready()

    def wait_ready(self):
        deadline = time.monotonic() + 10
        while not self.socket.exists():  # created after the views are mounted
            if self.proc.poll() is not None:
                raise RuntimeError(f"daemon exited: {(self.work / 'daemon.log').read_text()}")
            if time.monotonic() > deadline:
                raise TimeoutError(f"daemon did not create {self.socket}")
            time.sleep(0.02)

    def stop(self) -> int:
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
        try:
            return self.proc.wait(timeout=10)
        finally:
            umount = ["fusermount3", "-u", "-z", self.mount]
            subprocess.run(umount, capture_output=True, check=False)
            self.log.close()

    @property
    def ledger(self) -> list[str]:
        return (self.state / "ledger.log").read_text().splitlines()

    def upper(self, scope_id: str) -> Path:
        return self.state / "scopes" / scope_id / "upper"


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
