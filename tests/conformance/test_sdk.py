"""Phase 1.6: the Python SDK, end to end.

Each check runs a small app as its own Python process. `escrow.init()` finds no
`ESCROW_SOCKET`, re-executes the app under `escrow run` (binding the interpreter and
sys.path read-only into the sandbox), and the app then uses `escrow.scope` with plain
`open`, pathlib and subprocess calls. The app prints JSON; the checks read it, the
project and the ledger. The exit tests themselves run through the test app
(`test_app.py`); these checks cover the rest of the SDK's API.
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

    def run(self, body: str, mode: str = "deny", check: bool = True) -> dict:
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


def test_decide_and_outcome_see_the_diff(sdk):
    out = sdk.run("""
        def gate(cs):
            out["seen"] = cs.diff
            return escrow.commit() if "+hello" in cs.diff else escrow.discard("no hello")

        with escrow.scope("diff", decide=gate) as s:
            (P / "hello.txt").write_text("hello\\n")
        out["outcome"] = [s.outcome.status, s.outcome.diff == out["seen"]]
    """)
    assert out["seen"].startswith("diff --git a/hello.txt b/hello.txt\nnew file mode 100644\n")
    assert out["outcome"] == ["committed", True]


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


def test_home_paths_are_rewritten_into_the_scope(escrow_bin, runtime_dir):
    sdk = Sdk(escrow_bin, runtime_dir, roots="  home: {default: capture, deny: ['~/.ssh']}\n")
    assert sdk.home
    (sdk.home / ".gitconfig").write_text("base\n")
    try:
        out = sdk.run("""
            home = Path(os.path.expanduser("~"))
            with escrow.scope("cfg") as s:
                (home / ".gitconfig").write_text("scoped\\n")
                out["inside"] = (home / ".gitconfig").read_text()
                cat = ["cat", str(home / ".gitconfig")]
                out["child"] = subprocess.run(cat, capture_output=True, text=True).stdout
                out["view"] = s.path(home / ".gitconfig")
                out["realpath"] = os.path.realpath(home / ".gitconfig")
                out["held"] = open("/escrow/unscoped.home/.gitconfig").read()
            out["outcome"] = [s.outcome.status, s.outcome.paths]
        """)
    finally:
        for m in (runtime_dir / "run" / "escrowd").glob("*/view"):
            subprocess.run(["fusermount3", "-u", "-z", m], capture_output=True)
    gitconfig = str(sdk.home / ".gitconfig")
    assert out["inside"] == out["child"] == "scoped\n"
    assert out["held"] == "base\n"  # the app outside the scope still sees the base
    assert out["view"].startswith("/escrow/s") and out["view"].endswith(".home/.gitconfig")
    assert out["realpath"] == gitconfig
    assert out["outcome"] == ["committed", ["~/.gitconfig"]]
    assert (sdk.home / ".gitconfig").read_text() == "scoped\n"


# ---- 2.6: unscoped escapes and typed errors (#14, #16) ----


def test_os_system_and_posix_spawn_run_in_the_scope(sdk):
    out = sdk.run("""
        with escrow.scope("spawn") as s:
            out["system"] = os.system("echo x > sys.txt")
            out["exit3"] = os.system("exit 3")
            pid = os.posix_spawnp("sh", ["sh", "-c", "echo y > spawnp.txt"], os.environ)
            out["spawnp"] = os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1])
            redirect = (os.POSIX_SPAWN_OPEN, 1, str(P / "fa.txt"),
                        os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
            pid = os.posix_spawn("/bin/sh", ["sh", "-c", "echo z"], os.environ,
                                 file_actions=[redirect])
            out["spawn"] = os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1])
            out["staged"] = sorted(os.listdir(P))
        out["paths"] = sorted(s.outcome.paths)
        out["unscoped"] = s.outcome.unscoped
    """)
    assert out["system"] == 0 and out["exit3"] == 3 << 8
    assert out["spawnp"] == 0 and out["spawn"] == 0
    assert out["staged"] == out["paths"] == ["fa.txt", "spawnp.txt", "sys.txt"]
    assert out["unscoped"] == 0
    assert (sdk.project / "fa.txt").read_text() == "z\n"


def test_chdir_moves_into_the_view_and_back(sdk):
    out = sdk.run("""
        import ctypes, escrow._sdk as sdk_impl
        real_cwd = sdk_impl._cfg.orig["os.getcwd"]
        libc = ctypes.CDLL(None, use_errno=True)
        with escrow.scope("cd") as s:
            os.mkdir(P / "d")
            os.chdir(P / "d")
            out["cwd"] = os.getcwd()
            out["real_in_view"] = real_cwd().startswith(s.root)
            fd = libc.creat(b"native.txt", 0o644)  # native code, relative path
            out["native_fd_ok"] = fd >= 0
            os.close(fd)
        out["after"] = real_cwd()
        out["paths"] = sorted(s.outcome.paths)
    """)
    assert out["cwd"] == str(sdk.project / "d")
    assert out["real_in_view"] and out["native_fd_ok"]
    assert out["after"] == str(sdk.project / "d")
    assert out["paths"] == ["d", "d/native.txt"]


def test_unscoped_error_names_the_fix(sdk):
    out = sdk.run("""
        try:
            (P / "outside.txt").write_text("x")
        except escrow.EscrowUnscopedError as e:
            out["error"] = [e.errno, e.filename, isinstance(e, PermissionError),
                            isinstance(e, OSError), "escrow.scope" in str(e)]
        try:
            os.mkdir(P / "dir")
        except escrow.EscrowUnscopedError as e:
            out["mkdir"] = e.errno
        try:
            open("/proc/version", "w")
        except OSError as e:
            out["other"] = [type(e).__name__, e.errno]
    """)
    assert out["error"] == [30, str(sdk.project / "outside.txt"), True, True, True]
    assert out["mkdir"] == 30
    assert out["other"][0] != "EscrowUnscopedError"


def test_write_on_a_closed_scope_file_is_a_stale_handle(sdk):
    out = sdk.run("""
        with escrow.scope("stale", decide=lambda cs: escrow.discard()) as s:
            f = open(P / "kept.txt", "w")
            f.write("in scope\\n")
        try:
            f.write("after\\n")
            f.flush()
            os.fsync(f.fileno())
            out["error"] = None
        except escrow.EscrowStaleHandleError as e:
            out["error"] = [e.errno, isinstance(e, OSError), s.id in str(e)]
        except OSError as e:
            out["error"] = ["plain", e.errno]
    """)
    assert out["error"] == [9, True, True]  # EBADF


def test_native_escape_counts_as_unscoped(sdk):
    out = sdk.run(
        """
        import ctypes, warnings
        libc = ctypes.CDLL(None, use_errno=True)
        with warnings.catch_warnings(record=True) as w:
            warnings.simplefilter("always")
            with escrow.scope("leak", decide=lambda cs: escrow.discard()) as s:
                fd = libc.creat(str(P / "leak.txt").encode(), 0o644)  # not rewritten
                os.close(fd)
            out["warned"] = [x.category.__name__ for x in w]
        out["unscoped"] = s.outcome.unscoped
        out["paths"] = s.outcome.paths
        """,
        mode="implicit",
    )
    assert out["unscoped"] >= 1
    assert out["warned"] == ["EscrowUnscopedWarning"]
    assert out["paths"] == []


def test_conflict_return_reopens_the_scope_for_a_fix(escrow_bin, runtime_dir):
    sdk = Sdk(escrow_bin, runtime_dir, policy="conflict:\n  verdict: return\n")
    (sdk.project / "f.txt").write_text("base\n")
    out = sdk.run(
        """
        def first_commits_a(cs):
            with escrow.scope("a"):
                (P / "f.txt").write_text("from a\\n")

        with escrow.scope("b", decide=first_commits_a) as b:
            (P / "f.txt").write_text("from b\\n")
            (P / "g.txt").write_text("g\\n")
            env = subprocess.run(
                ["sh", "-c", 'echo "${ESCROW_SCOPE_TOKEN:-none}"'],
                env={"PATH": os.environ["PATH"]}, capture_output=True, text=True,
            )
        out["child"] = env.stdout.strip()
        o = b.outcome
        out["first"] = [o.status, o.reopened, o.paths]
        with escrow.scope(resume=b) as again:
            out["kept"] = (P / "f.txt").read_text()
            (P / "f.txt").write_text("from a\\n")  # take the other side: same content
        out["second"] = [again.outcome.status, again.outcome.reopened]
        """
    )
    assert out["child"] == "none"  # the scope's token never reaches a child
    assert out["first"] == ["conflict", True, ["f.txt"]]
    assert out["kept"] == "from b\n"
    assert out["second"] == ["conflict", True]  # f.txt is still a write over a changed file
    assert (sdk.project / "f.txt").read_text() == "from a\n"
    assert not (sdk.project / "g.txt").exists()
