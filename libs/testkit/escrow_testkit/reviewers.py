"""Command-reviewer stand-ins: executable Python scripts a policy's `run:` can name.

Each run of a stand-in records what it saw under `<work>/<name>.runs/<n>/` (stdin, the
environment it was started with, the files in its working directory, its pid and
attempt, when it started and ended), then answers as configured. Put them in the work
directory, outside the project: escrowd protects a reviewer's program from the scopes.
"""

import json
import sys
import time
from dataclasses import dataclass
from pathlib import Path

_SCRIPT = """#!{python} -I
import itertools, json, os, signal, sys, time

CFG = json.loads({cfg!r})
RUNS = {runs!r}

stdin = sys.stdin.buffer.read()
os.makedirs(RUNS, exist_ok=True)
for n in itertools.count(1):
    try:
        os.mkdir(os.path.join(RUNS, str(n)))
        break
    except FileExistsError:
        continue
here = os.path.join(RUNS, str(n))


def put(name, data):
    mode = "wb" if isinstance(data, bytes) else "w"
    with open(os.path.join(here, name), mode) as f:
        f.write(data)


def on_term(*_):
    put("terminated", "")
    os._exit(0)


signal.signal(signal.SIGTERM, on_term)
raw_env = open("/proc/self/environ", "rb").read().split(b"\\0")
put("stdin.json", stdin)
put("env.json", json.dumps(dict(e.decode().split("=", 1) for e in raw_env if e)))
put("cwd.json", json.dumps(dict(
    cwd=os.getcwd(),
    files={{f: open(f, errors="replace").read() for f in sorted(os.listdir("."))}},
)))
put("pid", str(os.getpid()))
put("attempt", str(json.loads(stdin or b"{{}}").get("attempt", 0)))
put("start", repr(time.time()))

if CFG["stderr"]:
    sys.stderr.write(CFG["stderr"] + "\\n")
    sys.stderr.flush()
time.sleep(CFG["sleep"])
put("end", repr(time.time()))

if CFG["kill_self"]:
    os.kill(os.getpid(), signal.SIGKILL)
if CFG["big"]:
    sys.stdout.write("x" * (2 << 20))
elif CFG["stdout"] is not None:
    sys.stdout.write(CFG["stdout"])
else:
    sys.stdout.write(json.dumps(dict(verdict=CFG["verdict"], reasons=CFG["reasons"])))
sys.stdout.flush()
sys.exit(CFG["exit_code"])
"""


@dataclass(frozen=True)
class Run:
    """One run of a stand-in, as it recorded itself."""

    dir: Path

    def _text(self, name: str) -> str:
        return (self.dir / name).read_text()

    @property
    def stdin(self) -> dict:
        """The request JSON the daemon wrote to its stdin."""
        return json.loads(self._text("stdin.json"))

    @property
    def env(self) -> dict[str, str]:
        """The environment it was started with."""
        return json.loads(self._text("env.json"))

    @property
    def cwd(self) -> Path:
        return Path(json.loads(self._text("cwd.json"))["cwd"])

    @property
    def cwd_files(self) -> dict[str, str]:
        """The files in its working directory when it started, with their text."""
        return json.loads(self._text("cwd.json"))["files"]

    @property
    def pid(self) -> int:
        return int(self._text("pid"))

    @property
    def attempt(self) -> int:
        return int(self._text("attempt"))

    @property
    def started(self) -> float:
        return float(self._text("start"))

    @property
    def ended(self) -> float | None:
        """When it finished its work; None while it sleeps (or was terminated)."""
        end = self.dir / "end"
        return float(end.read_text()) if end.exists() else None

    @property
    def terminated(self) -> bool:
        """It caught SIGTERM."""
        return (self.dir / "terminated").exists()


class StandIn:
    """A reviewer program and the record of its runs."""

    def __init__(self, path: Path, runs_dir: Path):
        self.path = path
        self.runs_dir = runs_dir

    def runs(self) -> list[Run]:
        """The runs that have started, oldest first."""
        if not self.runs_dir.exists():
            return []
        dirs = sorted((p for p in self.runs_dir.iterdir() if (p / "start").exists()), key=_number)
        return [Run(p) for p in dirs]

    def wait_for_runs(self, count: int, timeout: float = 10) -> list[Run]:
        """Wait until `count` runs have started; return them."""
        deadline = time.monotonic() + timeout
        while len(runs := self.runs()) < count:
            assert time.monotonic() < deadline, f"{self.path.name}: {len(runs)} of {count} runs"
            time.sleep(0.02)
        return runs

    def wait_until_done(self, run: Run, timeout: float = 10) -> None:
        """Wait until `run` has finished its work."""
        deadline = time.monotonic() + timeout
        while run.ended is None:
            assert time.monotonic() < deadline, f"{self.path.name}: run {run.dir.name} never ended"
            time.sleep(0.02)


def _number(p: Path) -> int:
    return int(p.name)


def stand_in(
    work: Path,
    name: str,
    *,
    verdict: str = "commit",
    reasons: tuple[str, ...] | list[str] = (),
    exit_code: int = 0,
    sleep: float = 0.0,
    stderr: str = "",
    stdout: str | None = None,
    kill_self: bool = False,
    big: bool = False,
) -> StandIn:
    """Write an executable stand-in `name` in `work`. It answers `{"verdict", "reasons"}`
    after sleeping `sleep` seconds, and exits `exit_code`; `stdout` replaces the answer,
    `big` writes 2 MiB instead, `kill_self` ends it with SIGKILL, and `stderr` is a line
    it writes first."""
    cfg = {
        "verdict": verdict,
        "reasons": list(reasons),
        "exit_code": exit_code,
        "sleep": sleep,
        "stderr": stderr,
        "stdout": stdout,
        "kill_self": kill_self,
        "big": big,
    }
    runs = work / f"{name}.runs"
    path = work / name
    path.write_text(_SCRIPT.format(python=sys.executable, cfg=json.dumps(cfg), runs=str(runs)))
    path.chmod(0o755)
    return StandIn(path, runs)
