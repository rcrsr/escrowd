"""A running `escrow daemon` for tests and soaks: its directories, policy file, clients
and the scope tokens those clients share."""

import os
import signal
import socket
import subprocess
import tempfile
import time
from pathlib import Path

from escrow import lowlevel

# A python writer for close-under-load checks: appends numbered lines to `log` and
# reports each one on stdout after the write returns.
WRITER = """
import os, sys
fd = os.open("log", os.O_WRONLY | os.O_CREAT | os.O_APPEND)
i = 0
while True:
    os.write(fd, b"%d\\n" % i)
    sys.stdout.write("%d\\n" % i)
    sys.stdout.flush()
    i += 1
"""


def isolate_git_env() -> None:
    """Keep the host's git config out of the run (a global signing program outside the
    sandbox's bind list fails every commit; issue #11). Children inherit these. No
    background maintenance or gc either: a detached `git maintenance` after a commit
    creates and deletes .git/objects/maintenance.lock while a check copies the repo."""
    os.environ |= {
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_SYSTEM": "/dev/null",
        "GIT_CONFIG_COUNT": "2",
        "GIT_CONFIG_KEY_0": "maintenance.auto",
        "GIT_CONFIG_VALUE_0": "false",
        "GIT_CONFIG_KEY_1": "gc.auto",
        "GIT_CONFIG_VALUE_1": "0",
    }


def make_runtime_dir() -> Path:
    # Unix socket paths are capped at 108 bytes, so stay out of pytest's long tmp paths;
    # Ubuntu 26.04 confines fusermount3 to /run/user/<uid>, /tmp and a few other roots.
    base = os.environ.get("XDG_RUNTIME_DIR") or tempfile.gettempdir()
    return Path(tempfile.mkdtemp(prefix="escrow-test.", dir=base))


def write_policy(
    work: Path,
    deny_read=(".env",),
    sandbox_read=(),
    sandbox_write=(),
    grace_ms=None,
    roots="",
    extra="",
) -> Path:
    policy = work / "policy.yaml"

    def items(xs):
        return ", ".join(f"'{x}'" for x in xs)

    policy.write_text(
        f"version: 1\nread:\n  deny: [{items(deny_read)}]\n"
        f"roots:\n  other:\n    read: [{items(sandbox_read)}]\n"
        f"    passthrough: [{items(sandbox_write)}]\n{roots}"
        + (f"close:\n  grace_ms: {grace_ms}\n" if grace_ms is not None else "")
        + extra
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


def wait_for(path: Path, timeout: float = 10) -> None:
    deadline = time.monotonic() + timeout
    while not path.exists():
        assert time.monotonic() < deadline, f"{path} never appeared"
        time.sleep(0.02)


def umount(d: "Daemon") -> None:
    """Lazily unmount `d`'s FUSE mount (a killed daemon leaves it behind)."""
    subprocess.run(["fusermount3", "-u", "-z", d.mount], capture_output=True, check=False)


def spawn(d: "Daemon", sid: str, script: str) -> subprocess.Popen:
    """`sh -c script` in scope `sid` through `escrow exec`, stdout piped."""
    return subprocess.Popen(
        d.exec_args(sid, "sh", "-c", script),
        env=d.exec_env(sid),
        stdout=subprocess.PIPE,
        text=True,
    )


class Daemon:
    """`escrow daemon` with its socket, project, state and mount under one work directory.

    `tokens` maps scope id to token for every client from `client()`, so one client can
    close or decide a scope another opened; it survives `restart()`."""

    def __init__(
        self,
        bin: Path,
        work: Path,
        *,
        deny_read=(".env",),
        project: Path | None = None,
        env: dict[str, str] | None = None,
        unscoped: str | None = None,
        sandbox_read=(),
        sandbox_write=(),
        grace_ms=None,
        roots="",
        policy="",
    ):
        """`deny_read`, `sandbox_read` and `sandbox_write` become the policy file's
        read.deny, roots.other.read and roots.other.passthrough lists, `grace_ms` its
        close.grace_ms; `roots` is YAML for more keys under roots: (`home`, `tmp`), indented
        by two spaces; `policy` is YAML for more top-level keys; `env` adds to the
        environment."""
        self.work = work
        self.bin = bin
        self.socket = work / "escrow.sock"
        self.project = project or work / "proj"
        self.state = work / "state"
        self.mount = work / "mnt"
        self.env = env
        self.tokens: dict[str, str] = {}
        self.project.mkdir(parents=True, exist_ok=True)
        self.policy = write_policy(
            work, deny_read, sandbox_read, sandbox_write, grace_ms, roots, policy
        )
        self.argv: list = [bin, "daemon", "--socket", self.socket, "--project", self.project]
        self.argv += ["--state", self.state, "--mount", self.mount, "--policy", self.policy]
        if unscoped:
            self.argv += ["--unscoped", unscoped]
        self._start()

    def _start(self) -> None:
        self.log = open(self.work / "daemon.log", "ab")
        env = {**os.environ, **(self.env or {})}
        self.proc = subprocess.Popen(self.argv, stderr=self.log, env=env)
        self.wait_ready()

    def spawn(self) -> subprocess.Popen:
        """Start the daemon on these directories without waiting (to kill it mid-recovery)."""
        return subprocess.Popen(self.argv, stderr=subprocess.DEVNULL)

    def wait_ready(self) -> None:
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
            umount(self)
            self.log.close()

    def kill(self) -> None:
        """SIGKILL the daemon; the socket file and the mount stay behind (the mount is
        unmounted lazily)."""
        self.proc.send_signal(signal.SIGKILL)
        self.proc.wait()
        self.log.close()
        umount(self)

    def restart(self, *, kill: bool = False, env: dict[str, str] | None = None) -> None:
        """Stop (or SIGKILL) the daemon and start it again on the same directories. The
        object and its token map stay; `env` replaces the extra environment (None keeps
        it, {} clears it)."""
        if kill:
            self.kill()
        else:
            self.stop()
        if env is not None:
            self.env = env
        self._start()

    def client(self) -> lowlevel.Client:
        """A client whose scope tokens are shared with every other client of this daemon."""
        return lowlevel.Client(str(self.socket), tokens=self.tokens)

    def reviewer(self) -> lowlevel.Reviewer:
        return lowlevel.Reviewer(f"{self.socket}.review")

    @property
    def ledger(self) -> list[str]:
        return (self.state / "ledger.log").read_text().splitlines()

    def upper(self, scope_id: str) -> Path:
        return self.state / "scopes" / scope_id / "upper"

    def exec(self, scope_id: str, *argv, env=None, **kw) -> subprocess.CompletedProcess:
        """`escrow exec --scope scope_id -- argv` from the host, output captured as text;
        `env` adds to the environment (a None value removes the variable)."""
        merged = {**self.exec_env(scope_id), **(env or {})}
        return subprocess.run(
            self.exec_args(scope_id, *argv),
            env={k: v for k, v in merged.items() if v is not None},
            capture_output=True,
            text=True,
            timeout=30,
            **kw,
        )

    def exec_args(self, scope_id: str, *argv) -> list:
        return [self.bin, "exec", "--scope", scope_id, "--", *argv]

    def token(self, scope_id: str) -> str:
        """The token a client of this daemon got for `scope_id` ("" if none did)."""
        return self.tokens.get(scope_id, "")

    def exec_env(self, scope_id: str) -> dict[str, str]:
        """The environment `escrow exec` needs for `scope_id`: socket and token."""
        return {
            **os.environ,
            "ESCROW_SOCKET": str(self.socket),
            "ESCROW_SCOPE_TOKEN": self.token(scope_id),
        }

    def generations(self) -> list[str]:
        """Generation directories holding pre-images, in every root."""
        return sorted(
            f"{d.name}/{g}" for d in self.state.glob("generations*") for g in os.listdir(d)
        )
