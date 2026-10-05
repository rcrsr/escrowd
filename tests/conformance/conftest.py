"""Fixtures shared by the conformance suite: the daemon binary, running daemons, bwrap."""

import os
import shutil
import signal
import socket
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]

# The suite must not depend on the host's git config (a global signing program outside
# the sandbox's bind list fails every commit; issue #11). Children inherit these.
os.environ |= {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null"}


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


def write_policy(work: Path, deny_read=(".env",), sandbox_read=(), sandbox_write=()) -> Path:
    policy = work / "policy.yaml"

    def items(xs):
        return ", ".join(f"'{x}'" for x in xs)

    policy.write_text(
        f"version: 1\nread:\n  deny: [{items(deny_read)}]\n"
        f"sandbox:\n  read: [{items(sandbox_read)}]\n  write: [{items(sandbox_write)}]\n"
    )
    return policy


def accepts(path: Path) -> bool:
    """The Unix socket at `path` takes connections (a crashed daemon leaves a stale file)."""
    with socket.socket(socket.AF_UNIX) as s:
        try:
            s.connect(str(path))
            return True
        except OSError:
            return False


class Daemon:
    """`escrow daemon` with its socket, project, state and mount under one work directory."""

    def __init__(
        self,
        bin: Path,
        work: Path,
        deny_read=(".env",),
        project: Path | None = None,
        env: dict[str, str] | None = None,
        unscoped: str | None = None,
        sandbox_read=(),
        sandbox_write=(),
    ):
        """`deny_read`, `sandbox_read` and `sandbox_write` become the policy file's read.deny,
        sandbox.read and sandbox.write lists; `env` adds to the environment."""
        self.work = work
        self.bin = bin
        self.socket = work / "escrow.sock"
        self.project = project or work / "proj"
        self.state = work / "state"
        self.mount = work / "mnt"
        self.project.mkdir(parents=True, exist_ok=True)
        self.policy = write_policy(work, deny_read, sandbox_read, sandbox_write)
        args = [bin, "daemon", "--socket", self.socket, "--project", self.project]
        args += ["--state", self.state, "--mount", self.mount, "--policy", self.policy]
        if unscoped:
            args += ["--unscoped", unscoped]
        self.log = open(work / "daemon.log", "ab")
        self.proc = subprocess.Popen(args, stderr=self.log, env={**os.environ, **(env or {})})
        self.wait_ready()

    def wait_ready(self):
        deadline = time.monotonic() + 10
        while not accepts(self.socket):  # the socket is bound after the views are mounted
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

    def exec(self, scope_id: str, *argv, **kw) -> subprocess.CompletedProcess:
        """`escrow exec --scope scope_id -- argv` from the host, output captured as text."""
        return subprocess.run(
            self.exec_args(scope_id, *argv),
            env={**os.environ, "ESCROW_SOCKET": str(self.socket)},
            capture_output=True,
            text=True,
            timeout=30,
            **kw,
        )

    def exec_args(self, scope_id: str, *argv) -> list:
        return [self.bin, "exec", "--scope", scope_id, "--", *argv]

    def generations(self) -> list[str]:
        """Generation directories holding pre-images."""
        d = self.state / "generations"
        return sorted(os.listdir(d)) if d.exists() else []


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
