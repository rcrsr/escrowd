"""Phase 1.6: the Python SDK, end to end.

Each check runs a small app as its own Python process. `escrow.init()` finds no
`ESCROW_SOCKET`, re-executes the app under `escrow run` (binding the interpreter and
sys.path read-only into the sandbox), and the app then uses `escrow.scope` with plain
`open`, pathlib and subprocess calls. The app prints JSON; the checks read it, the
project and the ledger.
"""

import json
import os
import subprocess
import sys
import textwrap

import pytest
from conftest import write_policy

PRELUDE = """\
import asyncio, json, os, subprocess, sys
from pathlib import Path
import escrow

P = Path(sys.argv[1])
escrow.init(P, unscoped=sys.argv[2], policy=sys.argv[3], on_exit="discard")
os.chdir(P)
out = {}
"""


class Sdk:
    def __init__(self, escrow_bin, runtime_dir):
        self.bin, self.work = escrow_bin, runtime_dir
        self.project = runtime_dir / "proj"
        self.project.mkdir()
        (runtime_dir / "app").mkdir()
        (runtime_dir / "run").mkdir()
        self.policy = write_policy(runtime_dir / "app", deny_read=(".env",))

    def run(self, body: str, mode: str = "deny", check: bool = True) -> dict:
        script = self.work / "app" / "app.py"
        script.write_text(PRELUDE + textwrap.dedent(body) + "\nprint(json.dumps(out))\n")
        env = {k: v for k, v in os.environ.items() if not k.startswith("ESCROW_")}
        env |= {
            "ESCROW_EXE": str(self.bin),
            "XDG_STATE_HOME": str(self.work / "state-home"),
            "XDG_RUNTIME_DIR": str(self.work / "run"),
        }
        r = subprocess.run(
            [sys.executable, script, self.project, mode, self.policy],
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


@pytest.fixture
def sdk(escrow_bin, runtime_dir):
    s = Sdk(escrow_bin, runtime_dir)
    yield s
    for m in (runtime_dir / "run" / "escrowd").glob("*/view"):
        subprocess.run(["fusermount3", "-u", "-z", m], capture_output=True)


def test_init_reexecutes_under_escrow_run(sdk):
    out = sdk.run("""
        out["socket"] = os.environ["ESCROW_SOCKET"]
        out["views"] = sorted(os.listdir("/escrow"))
    """)
    assert out == {"socket": "/run/escrowd/escrow.sock", "views": ["unscoped"]}


def test_scope_writes_stay_escrowed_until_commit(sdk):
    """Exit test 1: writes, reads, renames and deletes inside a scope leave the project
    unchanged until the scope commits."""
    (sdk.project / "old.txt").write_text("old\n")
    (sdk.project / "gone.txt").write_text("gone\n")
    out = sdk.run("""
        with escrow.scope("work") as s:
            (P / "README.md").write_text("# Demo\\n")
            out["own_read"] = open("README.md").read()
            os.rename("old.txt", "new.txt")
            os.remove(P / "gone.txt")
            os.makedirs(P / "docs" / "api")
            with escrow.scope("peek", decide=lambda cs: escrow.discard()):
                out["other_scope_sees"] = sorted(os.listdir(P))
        out["status"], out["paths"] = s.outcome.status, sorted(s.outcome.paths)
    """)
    assert out["own_read"] == "# Demo\n"
    assert out["other_scope_sees"] == ["gone.txt", "old.txt"]
    assert out["status"] == "committed"
    assert out["paths"] == ["README.md", "docs", "docs/api", "gone.txt", "new.txt"]
    assert sorted(os.listdir(sdk.project)) == ["README.md", "docs", "new.txt"]
    assert (sdk.project / "new.txt").read_text() == "old\n"


def test_concurrent_async_scopes_on_one_thread(sdk):
    """Exit test 3: two async scopes on one thread each write a file and run bash -c to
    write another; all four writes land in the right scopes."""
    out = sdk.run("""
        import threading

        async def work(name):
            async with escrow.scope(name) as s:
                out.setdefault("threads", []).append(threading.get_ident())
                (P / f"{name}.txt").write_text(name)
                await asyncio.sleep(0.05)
                p = await asyncio.create_subprocess_exec(
                    "bash", "-c", f"echo {name} > {name}-sub.txt", cwd=P)
                await p.wait()
                await asyncio.sleep(0.05)
                out[name + "_reads"] = (P / f"{name}-sub.txt").read_text()
            return s

        async def main():
            a, b = await asyncio.gather(work("a"), work("b"))
            out["a"] = [a.id, sorted(a.outcome.paths)]
            out["b"] = [b.id, sorted(b.outcome.paths)]

        asyncio.run(main())
        out["one_thread"] = len(set(out.pop("threads"))) == 1
    """)
    assert out["one_thread"]
    assert (out["a_reads"], out["b_reads"]) == ("a\n", "b\n")
    (a, a_paths), (b, b_paths) = out["a"], out["b"]
    assert (a_paths, b_paths) == (["a-sub.txt", "a.txt"], ["b-sub.txt", "b.txt"])
    for sid, name in ((a, "a"), (b, "b")):
        mine = [line for line in sdk.ledger if f" scope={sid} " in line and "op=create" in line]
        assert sorted(line.split()[3] for line in mine) == [
            f"path={name}-sub.txt",
            f"path={name}.txt",
        ]
    assert sorted(os.listdir(sdk.project)) == ["a-sub.txt", "a.txt", "b-sub.txt", "b.txt"]


def test_denied_read_fails_and_is_reported(sdk):
    """Exit test 6: a denied read fails with EACCES and appears in the outcome and ledger."""
    (sdk.project / ".env").write_text("SECRET=1\n")
    out = sdk.run("""
        with escrow.scope("leaky") as s:
            try:
                open(".env").read()
            except PermissionError as e:
                out["errno"] = e.errno
        out["reads"] = [[r.path, r.allowed] for r in s.outcome.reads]
        out["id"] = s.id
    """)
    assert out["errno"] == 13
    assert out["reads"] == [[".env", False]]
    assert any(f" scope={out['id']} op=read path=.env decision=deny" in x for x in sdk.ledger)


def test_decide_discards_with_reasons(sdk):
    out = sdk.run("""
        def gate(cs):
            if "secrets.txt" in cs.paths:
                return escrow.discard("touches secrets")
            return escrow.commit()

        with escrow.scope("bad", decide=gate, labels={"tool": "write"}) as s:
            (P / "secrets.txt").write_text("x")
        out["outcome"] = [s.outcome.status, s.outcome.reasons, s.outcome.labels]
    """)
    assert out["outcome"] == ["discarded", ["touches secrets"], {"tool": "write"}]
    assert not (sdk.project / "secrets.txt").exists()


def test_send_back_then_resume_and_commit(sdk):
    out = sdk.run("""
        def gate(cs):
            if "tests.txt" not in cs.paths:
                return escrow.send_back("add tests.txt")
            return escrow.commit()

        with escrow.scope("feature", decide=gate) as s:
            (P / "code.txt").write_text("code")
        out["first"] = [s.outcome.status, s.outcome.reasons]
        with escrow.scope(resume=s, decide=gate) as s2:
            out["still_staged"] = (P / "code.txt").read_text()
            (P / "tests.txt").write_text("tests")
        out["second"] = [s2.outcome.status, sorted(s2.outcome.paths)]
    """)
    assert out["first"] == ["returned", ["add tests.txt"]]
    assert out["still_staged"] == "code"
    assert out["second"] == ["committed", ["code.txt", "tests.txt"]]


def test_async_decide_and_exception_discards(sdk):
    out = sdk.run("""
        async def gate(cs):
            await asyncio.sleep(0)
            return escrow.commit()

        async def main():
            async with escrow.scope("ok", decide=gate) as s:
                (P / "ok.txt").write_text("ok")
            out["ok"] = s.outcome.status
            s2 = escrow.scope("boom")
            try:
                async with s2:
                    (P / "boom.txt").write_text("x")
                    raise ValueError("tool crashed")
            except ValueError:
                out["boom"] = [s2.outcome.status, s2.outcome.reasons]

        asyncio.run(main())
    """)
    assert out["ok"] == "committed"
    assert out["boom"] == ["discarded", ["ValueError: tool crashed"]]
    assert sorted(os.listdir(sdk.project)) == ["ok.txt"]


def test_paths_map_back_to_the_project(sdk):
    (sdk.project / "target.txt").write_text("t\n")
    out = sdk.run("""
        with escrow.scope("paths", decide=lambda cs: escrow.discard()) as s:
            os.symlink("target.txt", P / "link")
            out["realpath"] = os.path.realpath(P / "link")
            out["resolve"] = str((P / "link").resolve())
            out["link_target"] = os.readlink(P / "link")
            os.chdir(s.root)
            out["cwd_in_view"] = os.getcwd()
            os.chdir(P)
            out["root"] = s.root
    """)
    assert out["realpath"] == out["resolve"] == str(sdk.project / "target.txt")
    assert out["link_target"] == "target.txt"  # link content is not rewritten
    assert out["cwd_in_view"] == str(sdk.project)
    assert out["root"].startswith("/escrow/s")


def test_subprocess_in_a_directory_the_scope_created(sdk):
    out = sdk.run("""
        with escrow.scope("build") as s:
            (P / "build").mkdir()
            r = subprocess.run("pwd; echo done > out.log", shell=True, cwd=P / "build",
                               capture_output=True, text=True, check=True)
            out["pwd"] = r.stdout.strip()
        out["paths"] = sorted(s.outcome.paths)
    """)
    assert out["pwd"] == str(sdk.project / "build")
    assert out["paths"] == ["build", "build/out.log"]


def test_unscoped_deny_refuses_writes_outside_scopes(sdk):
    out = sdk.run("""
        try:
            (P / "outside.txt").write_text("x")
        except OSError as e:
            out["errno"] = e.errno
    """)
    assert out["errno"] == 30  # EROFS
    assert not (sdk.project / "outside.txt").exists()


def test_settle_unscoped_in_implicit_mode(sdk):
    out = sdk.run(
        """
        (P / "loose.txt").write_text("x")
        o = escrow.settle_unscoped(lambda cs: escrow.commit())
        out["settled"] = [o.status, o.paths]
        (P / "after.txt").write_text("y")  # the fresh default scope, discarded at exit
        """,
        mode="implicit",
    )
    assert out["settled"] == ["committed", ["loose.txt"]]
    assert sorted(os.listdir(sdk.project)) == ["loose.txt"]
