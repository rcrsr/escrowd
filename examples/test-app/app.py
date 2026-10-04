"""The phase 1 test app: each exit test's IO with ordinary Python file and subprocess calls.

    python app.py PROJECT MODE POLICY CHECK

`escrow.init()` re-executes the app under `escrow run` with PROJECT in the unscoped MODE
(passthrough, implicit or deny) and the policy file POLICY, then runs CHECK. The app
prints one JSON object: what it saw, each scope's outcome and, under "scopes", the path
prefixes each scope id may touch. Each check keeps a scope's IO under its own prefix and
writes content tagged with it, so the suite can flag any ledger entry that names a scope
for an operation it did not perform.
"""

import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path

import escrow

P = Path()  # the project, from argv; the suite sets it to replay `mutate` natively
out: dict = {"scopes": {"unscoped": []}}


def owns(s: escrow.Scope, *prefixes: str) -> None:
    out["scopes"][s.id] = list(prefixes)


def outcome(s: escrow.Scope) -> dict:
    o = s.outcome
    assert o is not None
    return {"status": o.status, "paths": sorted(o.paths), "reasons": o.reasons}


def write(rel: str, tag: str) -> None:
    (P / rel).write_text(f"{tag}:{rel}\n")


def mutate(d: str) -> None:
    """A write, a create, a rename, a delete, a chmod, a symlink swap and a new tree."""
    write(f"{d}/edit.txt", d)
    write(f"{d}/new.txt", d)
    os.rename(P / d / "old.txt", P / d / "renamed.txt")
    os.remove(P / d / "gone.txt")
    os.chmod(P / d / "mode.txt", 0o600)
    os.remove(P / d / "link")
    os.symlink("edit.txt", P / d / "link")
    os.makedirs(P / d / "sub" / "deep")
    write(f"{d}/sub/deep/f.txt", d)


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


CHECKS = {
    f.__name__.replace("_", "-"): f
    for f in (escrowed, child, concurrent, discarded, atomic, conflict, denied_read)
    + (unscoped, snapshot)
}

if __name__ == "__main__":
    P = Path(sys.argv[1])
    MODE, POLICY, CHECK = sys.argv[2:5]
    escrow.init(P, unscoped=MODE, policy=POLICY, on_exit="discard")
    CHECKS[CHECK]()
    print(json.dumps(out))
