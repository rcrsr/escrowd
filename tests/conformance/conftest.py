"""Fixtures shared by the conformance suite: the daemon binary, a running daemon, bwrap."""

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


@pytest.fixture
def runtime_dir():
    # Unix socket paths are capped at 108 bytes, so stay out of pytest's long tmp paths.
    base = os.environ.get("XDG_RUNTIME_DIR") or tempfile.gettempdir()
    d = Path(tempfile.mkdtemp(prefix="escrow-test.", dir=base))
    yield d
    shutil.rmtree(d, ignore_errors=True)


class Daemon:
    def __init__(self, bin: Path, socket: Path):
        self.socket = socket
        self.proc = subprocess.Popen([bin, "daemon", "--socket", socket], stderr=subprocess.PIPE)
        deadline = time.monotonic() + 10
        while not socket.exists():
            if self.proc.poll() is not None:
                raise RuntimeError(f"daemon exited: {self.proc.stderr.read().decode()}")
            if time.monotonic() > deadline:
                raise TimeoutError(f"daemon did not create {socket}")
            time.sleep(0.02)

    def stop(self) -> int:
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
        return self.proc.wait(timeout=10)


@pytest.fixture
def daemon(escrow_bin, runtime_dir):
    d = Daemon(escrow_bin, runtime_dir / "escrow.sock")
    yield d
    d.stop()


def ubuntu_userns_restricted() -> bool:
    try:
        return (
            Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").read_text().strip()
            == "1"
        )
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
        pytest.fail(
            "AppArmor restricts user namespaces here: run packaging/ubuntu/install.sh first"
        )
    path = shutil.which("bwrap")
    if not path:
        pytest.fail("bwrap not found")
    return path
