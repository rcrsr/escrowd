"""Spike 0.4 checks, driven by run.sh: python3 test_04.py <project> <mount> <uppers> <ledger> <bwrap>"""

import asyncio
import errno
import os
import pathlib
import sys
import threading

import escrow_shim as escrow

project, mount, uppers, ledger, bwrap = sys.argv[1:6]
escrow.init(project, mount, bwrap)
P = pathlib.Path(project)
U = pathlib.Path(uppers)
fails = 0
threads = set()


def check(name, ok, detail=""):
    global fails
    print(f"{'PASS' if ok else 'FAIL'}  {name}{'' if ok else ': ' + str(detail)}", flush=True)
    fails += 0 if ok else 1


def upper(scope, rel):
    p = U / scope / rel
    return p.read_text() if p.exists() else None


async def worker(name, readme, extra):
    """One scope: interleaved writes, reads, a pathlib write and a bash subprocess."""
    seen = {}
    async with escrow.scope(name):
        threads.add(threading.get_ident())
        with open(os.path.join(project, "README.md"), "w") as f:  # absolute path
            f.write(readme)
        await asyncio.sleep(0)
        seen["readme"] = open("README.md").read()  # relative path, cwd is the project
        (P / extra).write_text(name)  # pathlib
        await asyncio.sleep(0)
        proc = await asyncio.create_subprocess_exec("bash", "-c", f"echo built{name} > build.log", cwd=project)
        await proc.wait()
        await asyncio.sleep(0)
        seen["build"] = open("build.log").read()
        seen["ino"] = os.stat("base.txt").st_ino
    return seen


async def main():
    os.chdir(project)

    # 1. Two scopes, two tasks, one thread, interleaved on the same paths.
    a, b = await asyncio.gather(worker("a", "A", "notes-a.txt"), worker("b", "B", "notes-b.txt"))
    check("two tasks ran on one thread", len(threads) == 1, threads)
    check("each scope reads its own README (page cache not shared)", a["readme"] == "A" and b["readme"] == "B", (a, b))
    check("each scope reads its own subprocess output", a["build"] == "builta\n" and b["build"] == "builtb\n", (a, b))
    check("scope a staged README, notes-a, build.log",
          (upper("a", "README.md"), upper("a", "notes-a.txt"), upper("a", "build.log")) == ("A", "a", "builta\n"))
    check("scope b staged README, notes-b, build.log",
          (upper("b", "README.md"), upper("b", "notes-b.txt"), upper("b", "build.log")) == ("B", "b", "builtb\n"))
    check("no cross-scope writes", upper("a", "notes-b.txt") is None and upper("b", "notes-a.txt") is None)
    check("same lower file has a different inode per scope", a["ino"] != b["ino"], (a["ino"], b["ino"]))
    check("base README untouched", (P / "README.md").read_text() == "base")  # outside any scope

    # 2. Read gate: EACCES, decided synchronously, in-process and in a subprocess.
    async with escrow.scope("c"):
        try:
            open(".env").read()
            check("in-process read of .env denied", False, "read succeeded")
        except PermissionError as e:
            check("in-process read of .env denied (EACCES)", e.errno == errno.EACCES, e)
        proc = await asyncio.create_subprocess_exec("cat", ".env", cwd=project, stderr=asyncio.subprocess.PIPE)
        _, err = await proc.communicate()
        check("subprocess read of .env denied", proc.returncode != 0 and b"Permission denied" in err, err)
        check("allowed read still works", open("README.md").read() == "base")
    log = pathlib.Path(ledger).read_text().splitlines()
    denies = [l for l in log if "scope=c op=read path=.env decision=deny" in l]
    check("both denials in the ledger with scope", len(denies) == 2, denies)
    check("subprocess writes attributed by path in the ledger",
          any("scope=a op=create path=build.log" in l for l in log) and any("scope=b op=create path=build.log" in l for l in log))

    # 3. Flush before decision. With the writeback cache, written data can sit in the kernel.
    n = 4 * 1024 * 1024
    async with escrow.scope("d"):
        f = open("big.bin", "wb")
        f.write(b"x" * n)
        f.flush()  # Python buffer -> kernel page cache; file still open
        pre = (U / "d" / "big.bin").stat().st_size
        escrow.syncfs("d")
        after_syncfs = (U / "d" / "big.bin").stat().st_size
        escrow.flush("d")  # fsync of tracked open files, then syncfs
        after_flush = (U / "d" / "big.bin").stat().st_size
        f.close()
        g = open("closed.bin", "wb")
        g.write(b"y" * n)
        g.close()
        after_close = (U / "d" / "closed.bin").stat().st_size
    print(f"INFO  daemon had {pre} of {n} bytes before any flush, {after_syncfs} after syncfs alone")
    check("scope flush (fsync open files) delivers every byte while the file is open", after_flush == n, after_flush)
    check("close() alone delivers every byte", after_close == n, after_close)

    print(f"result: {fails} failed")
    sys.exit(1 if fails else 0)


asyncio.run(main())
