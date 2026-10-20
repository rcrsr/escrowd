"""The phase 1 test app: each exit test's IO with ordinary Python file and subprocess calls.

    python app.py PROJECT MODE POLICY CHECK SCENARIO

`escrow.init()` re-executes the app under `escrow run` with PROJECT in the unscoped MODE
(passthrough, implicit or deny) and the policy file POLICY, then runs CHECK. SCENARIO is a
JSON file whose `mutate` ops (`tests/fixtures/scenarios/app.json`) the checks replay with
plain file calls. The app prints one JSON object: what it saw, each scope's outcome and,
under "scopes", the path prefixes each scope id may touch. Each check keeps a scope's IO
under its own prefix and writes content tagged with it, so the suite can flag any ledger
entry that names a scope for an operation it did not perform.
"""

import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path

import escrow

P = Path()  # the project, from argv
SCENARIO: dict = {}  # the scenario file, from argv
out: dict = {"scopes": {"unscoped": []}}


def owns(s: escrow.Scope, *prefixes: str) -> None:
    out["scopes"][s.id] = list(prefixes)


def outcome(s: escrow.Scope) -> dict:
    o = s.outcome
    assert o is not None
    return {"status": o.status, "paths": sorted(o.paths), "reasons": o.reasons}


def write(rel: str, tag: str) -> None:
    (P / rel).write_text(f"{tag}:{rel}\n")


def fill(s: str, d: str, path: str = "") -> str:
    return s.replace("{dir}", d).replace("{path}", path)


def apply(ops: list[dict], d: str) -> None:
    """Run scenario ops in order under the project; `{dir}` is `d`, `{path}` the op's path."""
    for op in ops:
        kind = op["op"]
        match kind:
            case "mkdir":
                (P / fill(op["path"], d)).mkdir(parents=True, exist_ok=True)
            case "write":
                path = fill(op["path"], d)
                (P / path).write_text(fill(op["text"], d, path))
            case "rename":
                os.rename(P / fill(op["from"], d), P / fill(op["to"], d))
            case "remove":
                os.remove(P / fill(op["path"], d))
            case "chmod":
                os.chmod(P / fill(op["path"], d), int(op["mode"], 8))
            case "symlink":
                os.symlink(fill(op["target"], d), P / fill(op["path"], d))
            case _:
                raise ValueError(f"unknown op {kind!r}")


def mutate(d: str) -> None:
    """The scenario's `mutate` ops: a write, a create, a rename, a delete, a chmod, a
    symlink swap and a new tree."""
    apply(SCENARIO["mutate"], d)


# ---- exit tests ----


def escrowed() -> None:
    """1: writes, reads, renames and deletes leave the project unchanged until commit."""

    def gate(cs: escrow.ChangeSet) -> escrow.Decision:
        out["project_at_decide"] = sorted(os.listdir(P / "w"))  # unscoped: the live base
        return escrow.commit()

    out["scopes"]["unscoped"] = ["w"]  # the gate's listing
    with escrow.scope("writes", decide=gate) as s:
        owns(s, "w")
        mutate("w")
        out["own_read"] = (P / "w" / "edit.txt").read_text()
        with escrow.scope("peek", decide=lambda cs: escrow.discard()) as peek:
            owns(peek, "w")
            out["other_scope_sees"] = sorted(os.listdir(P / "w"))
    out["writes"] = outcome(s)


def child() -> None:
    """2: a subprocess's write lands in the scope that started it."""
    with escrow.scope("child") as s:
        owns(s, "c")
        script = "echo c:c/f.txt > f.txt; mkdir -p d; echo c:c/d/g.txt > d/g.txt"
        subprocess.run(["bash", "-c", script], cwd=P / "c", check=True)
        out["read_back"] = (P / "c" / "f.txt").read_text()
    out["child"] = outcome(s)


def concurrent() -> None:
    """3: two async scopes on one thread, each with a file write and a bash -c write."""
    import threading

    threads = set()

    async def work(name: str) -> escrow.Scope:
        async with escrow.scope(name) as s:
            owns(s, name)
            threads.add(threading.get_ident())
            write(f"{name}/{name}.txt", name)
            await asyncio.sleep(0.05)
            cmd = f"echo {name}:{name}/{name}-sub.txt > {name}-sub.txt"
            p = await asyncio.create_subprocess_exec("bash", "-c", cmd, cwd=P / name)
            await p.wait()
            await asyncio.sleep(0.05)
            out[f"{name}_reads"] = (P / name / f"{name}-sub.txt").read_text()
        return s

    async def main() -> None:
        for s in await asyncio.gather(work("a"), work("b")):
            out[s.name] = outcome(s)

    asyncio.run(main())
    out["threads"] = len(threads)


def discarded() -> None:
    """4: a discard leaves the project exactly as it was."""
    with escrow.scope("discard", decide=lambda cs: escrow.discard("not wanted")) as s:
        owns(s, "d")
        mutate("d")
    out["discard"] = outcome(s)


def atomic() -> None:
    """4: a commit applies every change or none (ESCROWD_FAULT makes it fail midway)."""
    s = escrow.scope("atomic")
    try:
        with s:
            owns(s, "d")
            mutate("d")
        out["atomic"] = outcome(s)
    except Exception as e:  # the daemon aborts the commit: grpc ABORTED
        out["atomic"] = {"status": "error", "error": type(e).__name__}


def conflict() -> None:
    """5: two scopes write one path; the second to commit hits the conflict policy."""
    first = escrow.scope("first")
    with escrow.scope("second") as second:
        owns(second, "x/shared.txt")
        write("x/shared.txt", "second")
        with first:
            owns(first, "x/shared.txt")
            write("x/shared.txt", "first")
    out["first"], out["second"] = outcome(first), outcome(second)


def denied_read() -> None:
    """6: a denied read fails with EACCES and is reported."""
    with escrow.scope("leaky") as s:
        owns(s, ".env")
        try:
            (P / ".env").read_text()
            out["errno"] = 0
        except PermissionError as e:
            out["errno"] = e.errno
    assert s.outcome is not None
    out["leaky"] = outcome(s) | {"reads": [[r.path, r.allowed] for r in s.outcome.reads]}


def unscoped() -> None:
    """7: IO outside every scope follows the unscoped mode."""
    out["scopes"]["unscoped"] = ["u"]
    try:
        write("u/out.txt", "unscoped")
        out["write"] = "ok"
        out["read_back"] = (P / "u" / "out.txt").read_text()
    except OSError as e:
        out["write"] = e.errno
    out["base_read"] = (P / "u" / "base.txt").read_text()


def snapshot() -> None:
    """8: a scope opened before another commits keeps reading the base as it was."""
    with escrow.scope("old") as old:
        owns(old, "s")
        out["before"] = (P / "s" / "f.txt").read_text()
        with escrow.scope("new") as new:
            owns(new, "s")
            write("s/f.txt", "new")
        out["new"] = outcome(new)
        out["after"] = (P / "s" / "f.txt").read_text()
        out["listing"] = sorted(os.listdir(P / "s"))
    out["old"] = outcome(old)


def held() -> None:
    """Held decisions, 1.1 to 1.3 (a policy sends `h/` to reviewers, `h/auth/` to a
    human; the suite reviews): the opener's commit is held, its own token cannot
    commit it, and the session's next scope opens only after the verdict."""
    with escrow.scope("guarded", session="agent", wait=False) as s:
        owns(s, "h")
        os.makedirs(P / "h" / "auth")
        write("h/auth/login.py", "guarded")
    out["held"] = outcome(s) | {"tiers": s.outcome.tiers, "wait": s.outcome.wait}
    with escrow.connect() as c:
        try:
            c.commit(s.id, token=s._scope_token)
            out["self_commit"] = "committed"
        except escrow.EscrowRpcError as e:
            out["self_commit"] = e.code().name
    # A required wait: this open waits for the verdict, so it reads the committed file.
    with escrow.scope("next", session="agent") as n:
        owns(n, "h")
        out["next_reads"] = (P / "h" / "auth" / "login.py").read_text()
    out["next"] = outcome(n)
    s.wait_decided()
    out["guarded"] = outcome(s)


def held_continue() -> None:
    """Held decisions, 1.4 and 1.5 (`n/` is reviewed, the wait optional): the agent
    continues past a held scope, so its next scope edits a stale snapshot and that
    edit conflicts; the reviewer of the second sees the first."""
    with escrow.scope("turn1", session="agent", wait=False) as a:
        owns(a, "n")
        write("n/guide.md", "turn1")
    with escrow.scope("turn2", session="agent", wait=False) as b:
        owns(b, "n")
        out["turn2_sees"] = (P / "n" / "guide.md").read_text()
        write("n/guide.md", "turn2")
    out["held"] = [a.outcome.status, b.outcome.status]  # ty: ignore[possibly-missing-attribute]
    a.wait_decided()
    b.wait_decided()
    out["turn1"], out["turn2"] = outcome(a), outcome(b)


CHECKS = {
    f.__name__.replace("_", "-"): f
    for f in (escrowed, child, concurrent, discarded, atomic, conflict, denied_read)
    + (unscoped, snapshot, held, held_continue)
}

if __name__ == "__main__":
    P = Path(sys.argv[1])
    MODE, POLICY, CHECK, SCENARIO_FILE = sys.argv[2:6]
    escrow.init(P, unscoped=MODE, policy=POLICY, on_exit="discard", read=(SCENARIO_FILE,))
    SCENARIO = json.loads(Path(SCENARIO_FILE).read_text())
    CHECKS[CHECK]()
    print(json.dumps(out))
