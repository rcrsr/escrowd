"""Harnesses for the SDK tests: `Sdk` runs a snippet as a small app, `App` runs one of the
test apps in `APPS` and checks its ledger, `StandIn` reviews held scopes on a review socket.

An SDK adds its app to `APPS` as an argv prefix; the app then takes the project, mode,
policy, check and scenario paths as arguments.
"""

import json
import os
import subprocess
import sys
import textwrap
import threading
import time
from pathlib import Path

import grpc

import escrow
from escrow import lowlevel
from escrow.v1 import escrow_pb2 as pb
from escrow_testkit import write_policy

ROOT = Path(__file__).resolve().parents[3]
PYTHON_APP = ROOT / "examples" / "test-app" / "app.py"
SCENARIO = ROOT / "tests" / "fixtures" / "scenarios" / "app.json"
APPS = {"python": [sys.executable, PYTHON_APP]}


PRELUDE = """\
import asyncio, json, os, subprocess, sys
from pathlib import Path
import escrow

escrow.init(sys.argv[1], unscoped=sys.argv[2], policy=sys.argv[3], on_exit="discard")
CWD = Path.cwd()
P = Path(sys.argv[1]).resolve()
os.chdir(P)
out = {}
"""


class Sdk:
    def __init__(self, escrow_bin, runtime_dir, roots="", policy=""):
        """`roots`: more policy keys under roots:; with them, $HOME is `<work>/home`;
        `policy`: more top-level policy keys."""
        self.bin, self.work = escrow_bin, runtime_dir
        self.project = runtime_dir / "proj"
        self.project.mkdir()
        (runtime_dir / "app").mkdir()
        (runtime_dir / "run").mkdir()
        self.home = runtime_dir / "home" if roots else None
        if self.home:
            self.home.mkdir()
        self.policy = write_policy(
            runtime_dir / "app", deny_read=(".env",), roots=roots, extra=policy
        )

    def run(
        self,
        body: str,
        mode: str = "deny",
        check: bool = True,
        cwd: Path | None = None,
        relative: bool = False,
    ) -> dict:
        """`cwd`: the directory the app starts in; `relative`: name the script and the
        project relative to it, not absolutely. In the body, `CWD` is the directory the app
        is in after `init()`."""
        script = self.work / "app" / "app.py"
        script.write_text(PRELUDE + textwrap.dedent(body) + "\nprint(json.dumps(out))\n")
        env = {k: v for k, v in os.environ.items() if not k.startswith("ESCROW_")}
        env |= {
            "ESCROW_EXE": str(self.bin),
            "XDG_STATE_HOME": str(self.work / "state-home"),
            "XDG_RUNTIME_DIR": str(self.work / "run"),
        }
        if self.home:
            env["HOME"] = str(self.home)
        project = self.project
        if relative:
            cwd = cwd or self.work
            script, project = os.path.relpath(script, cwd), os.path.relpath(project, cwd)
        r = subprocess.run(
            [sys.executable, script, project, mode, self.policy],
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.stderr = r.stderr
        if check:
            assert r.returncode == 0, r.stderr
        self.returncode = r.returncode
        lines = r.stdout.strip().splitlines()
        return json.loads(lines[-1]) if lines else {}

    @property
    def ledger(self) -> list[str]:
        (log,) = (self.work / "state-home" / "escrowd").glob("*/ledger.log")
        return log.read_text().splitlines()


def misattributed(ledger: list[str], scopes: dict[str, list[str]]) -> list[str]:
    """Ledger entries naming a scope the app never opened, or a path outside the
    prefixes that scope's IO used."""

    def owned(path: str, prefixes: list[str]) -> bool:
        return any(path == p or path.startswith(p + "/") for p in prefixes)

    bad = []
    for line in ledger:
        fields = dict(f.split("=", 1) for f in line.split()[1:])
        if "scope" not in fields and "proc" in fields:
            continue  # a process, named by the scope lines that follow
        prefixes = scopes.get(fields["scope"])
        paths = [p for p in (fields.get("path"), fields.get("from")) if p]
        if prefixes is None or not all(owned(p, prefixes) for p in paths):
            bad.append(line)
    return bad


class App:
    def __init__(self, escrow_bin: Path, work: Path, argv: list):
        """`argv`: the command that starts the app, before its arguments."""
        self.bin, self.work, self.argv = escrow_bin, work, argv
        self.project = work / "proj"
        self.project.mkdir()
        (work / "run").mkdir()
        self.policy = write_policy(work, deny_read=(".env",))

    def run(self, check: str, mode: str = "deny", env: dict | None = None, reviewer=None) -> dict:
        """`reviewer(rv)`, if given, is called on the review socket until the app exits."""
        clean = {k: v for k, v in os.environ.items() if not k.startswith("ESCROW")}
        clean |= {
            "ESCROW_EXE": str(self.bin),
            "XDG_STATE_HOME": str(self.work / "state-home"),
            "XDG_RUNTIME_DIR": str(self.work / "run"),
        }
        argv = [*self.argv, self.project, mode, self.policy, check, SCENARIO]
        env = clean | (env or {})
        if reviewer is None:
            r = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=60)
            stdout, stderr, code = r.stdout, r.stderr, r.returncode
        else:
            stdout, stderr, code = self._reviewed(argv, env, reviewer)
        assert code == 0, stderr
        out = json.loads(stdout.strip().splitlines()[-1])
        assert misattributed(self.ledger, out["scopes"]) == [], "misattributed ledger entries"
        return out

    def _reviewed(self, argv, env, reviewer):
        p = subprocess.Popen(
            argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        deadline = time.monotonic() + 60
        rv = None
        try:
            while p.poll() is None and time.monotonic() < deadline:
                if rv is None:
                    socks = list((self.work / "run" / "escrowd").glob("*/escrow.sock.review"))
                    if socks:
                        rv = escrow.Reviewer(str(socks[0]))
                if rv is not None:
                    try:
                        reviewer(rv)
                    except grpc.RpcError as e:
                        if e.code() != grpc.StatusCode.UNAVAILABLE:  # the app exited
                            raise
                time.sleep(0.05)
        finally:
            if rv is not None:
                rv.close()
            if p.poll() is None:
                p.kill()
        stdout, stderr = p.communicate()
        return stdout, stderr, p.returncode

    @property
    def ledger(self) -> list[str]:
        (log,) = (self.work / "state-home" / "escrowd").glob("*/ledger.log")
        return log.read_text().splitlines()

    def entries(self, scope_id: str, op: str) -> list[str]:
        return sorted(
            line.split(" path=")[1].split()[0]
            for line in self.ledger
            if f" scope={scope_id} op={op} " in line
        )


VERDICTS = {
    "commit": pb.VERDICT_COMMIT,
    "discard": pb.VERDICT_DISCARD,
    "return": pb.VERDICT_RETURN,
}


class StandIn:
    """Reviews each held scope with the next verdict in `verdicts` (llm tier) on the
    review socket under `sdk`'s runtime directory, until stopped. A verdict is
    "commit", "discard" or "return"."""

    def __init__(self, sdk: Sdk, verdicts: list[tuple[str, list[str]]], delay: float = 0):
        self.sdk, self.verdicts, self.seen, self.delay = sdk, verdicts, [], delay
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def run(self) -> None:
        rv = None
        while not self.stop.is_set():
            time.sleep(0.05)
            if rv is None:
                socks = list((self.sdk.work / "run" / "escrowd").glob("*/escrow.sock.review"))
                if not socks:
                    continue
                rv = lowlevel.Reviewer(str(socks[0]))
            try:
                for h in rv.list_held():
                    if not self.verdicts:
                        return
                    verdict, reasons = self.verdicts.pop(0)
                    self.seen.append(h.name)
                    time.sleep(self.delay)
                    rv.review(h.scope_id, pb.TIER_LLM, VERDICTS[verdict], reasons=reasons)
            except grpc.RpcError:
                rv.close()
                rv = None

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.stop.set()
        self.thread.join(5)
