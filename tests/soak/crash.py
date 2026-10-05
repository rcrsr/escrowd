"""Crash soak (phase 2, check 1): SIGKILL the daemon at a random time inside a commit.

    uv run --project sdk/python --frozen python tests/soak/crash.py [RUNS] [--files N]

Each run seeds a project, stages a large change set in one scope (edits, creates,
deletes, renames, mode changes, a directory replaced by a file), starts the commit and
kills the daemon after a delay drawn uniformly from the measured commit time. A quarter
of the runs also kill the restarted daemon during its recovery. The next clean start
must leave the project exactly as before the commit (byte-identical, no temporary
files, no generations) or exactly as the commit makes it; any third state fails the
run and keeps its directory. Run 0 calibrates: it commits without a kill.
"""

import argparse
import os
import random
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "conformance"))

from conftest import Daemon, make_runtime_dir  # noqa: E402
from scenario import fingerprint, tree  # noqa: E402

import escrow  # noqa: E402

os.environ |= {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null"}


def seed(root: Path, files: int) -> None:
    per = 100
    for i in range(files):
        d = root / f"d{i // per:03}"
        d.mkdir(exist_ok=True)
        (d / f"f{i % per:03}.txt").write_text(f"base {i}\n" * 4)
    (root / "tree" / "sub").mkdir(parents=True)
    (root / "tree" / "sub" / "x.txt").write_text("x\n")
    os.symlink("d000/f000.txt", root / "link")


def mutate(root: Path, files: int) -> None:
    """Deterministic: the same IO on the view and on the reference copy."""
    per = 100
    rng = random.Random(7)
    for i in range(files):
        p = root / f"d{i // per:03}" / f"f{i % per:03}.txt"
        r = rng.random()
        if r < 0.4:
            with open(p, "a") as f:
                f.write(f"edit {i}\n")
        elif r < 0.5:
            p.unlink()
        elif r < 0.6:
            os.rename(p, root / f"d{(i // per + 1) % (files // per or 1):03}" / f"moved{i}.txt")
        elif r < 0.65:
            os.chmod(p, 0o600)
    new = root / "new"
    for i in range(files // 4):
        (new / f"n{i // 50:02}").mkdir(parents=True, exist_ok=True)
        (new / f"n{i // 50:02}" / f"{i}.txt").write_text(f"new {i}\n")
    shutil.rmtree(root / "tree")
    (root / "tree").write_text("tree is a file now\n")
    os.unlink(root / "link")
    os.symlink("new/n00/0.txt", root / "link")


def spawn_daemon(d: Daemon) -> subprocess.Popen:
    """Start the daemon on `d`'s directories without waiting (to kill it mid-recovery)."""
    args = [d.bin, "daemon", "--socket", d.socket, "--project", d.project]
    args += ["--state", d.state, "--mount", d.mount, "--policy", d.policy]
    return subprocess.Popen(args, stderr=subprocess.DEVNULL)


def umount(d: Daemon) -> None:
    subprocess.run(["fusermount3", "-u", "-z", d.mount], capture_output=True)


def leftovers(root: Path) -> list[str]:
    return [n for _, dns, fns in os.walk(root) for n in dns + fns if n.startswith(".escrow-")]


def one(bin: Path, files: int, kill_after: float | None, double: bool) -> tuple[str, float, Path]:
    work = make_runtime_dir()
    d = Daemon(bin, work)
    seed(d.project, files)
    ref = work / "ref"
    shutil.copytree(d.project, ref, symlinks=True)
    mutate(ref, files)
    want = tree(ref)
    before, before_fp = tree(d.project), fingerprint(d.project)
    with escrow.connect(str(d.socket)) as c:
        sid = c.open_scope().scope_id
        mutate(d.mount / sid, files)
        c.close_scope(sid)
    result: dict = {}

    def commit():
        start = time.monotonic()
        try:
            with escrow.connect(str(d.socket)) as c:
                result["status"] = c.commit(sid, timeout=120).status
        except Exception as e:  # the kill
            result["error"] = e
        result["took"] = time.monotonic() - start

    t = threading.Thread(target=commit)
    t.start()
    if kill_after is not None:
        time.sleep(kill_after)
        d.proc.send_signal(signal.SIGKILL)
        d.proc.wait()
        d.log.close()
        umount(d)
        if double:  # kill the restart while it recovers
            p = spawn_daemon(d)
            time.sleep(random.uniform(0, 0.05))
            p.send_signal(signal.SIGKILL)
            p.wait()
            umount(d)
        t.join()
        d = Daemon(bin, work)  # recovery
    else:
        t.join()
    state = "fail"
    if tree(d.project) == before:
        ok = fingerprint(d.project) == before_fp and not leftovers(d.project)
        state = "pre" if ok and d.generations() == [] else "fail"
    elif tree(d.project) == want and not leftovers(d.project):
        state = "post"
    d.stop()
    if state != "fail":
        shutil.rmtree(work, ignore_errors=True)
    return state, result.get("took", 0.0), work


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("runs", type=int, nargs="?", default=100)
    ap.add_argument("--files", type=int, default=2000)
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument(
        "--bin", default=os.environ.get("ESCROW_BIN", ROOT / "target" / "debug" / "escrow")
    )
    a = ap.parse_args()
    seed_ = a.seed if a.seed is not None else random.randrange(1 << 30)
    random.seed(seed_)
    bin = Path(a.bin)
    host = subprocess.run(
        ["sh", "-c", ". /etc/os-release; echo $PRETTY_NAME"], capture_output=True, text=True
    )
    where = f"{host.stdout.strip()}, kernel {os.uname().release}"
    print(f"crash soak: {a.runs} runs, {a.files} files, seed {seed_}, {bin}, {where}")
    state, took, _ = one(bin, a.files, None, False)
    if state != "post":
        print(f"calibration failed: {state}")
        return 1
    print(f"calibration: commit took {took:.3f} s")
    counts = {"pre": 0, "post": 0, "fail": 0}
    for i in range(1, a.runs + 1):
        delay = random.uniform(0, took * 1.1)
        double = random.random() < 0.25
        state, _, work = one(bin, a.files, delay, double)
        counts[state] += 1
        note = f" kept {work}" if state == "fail" else ""
        how = f"kill_after={delay * 1000:.0f}ms double={'yes' if double else 'no'}"
        print(f"run {i}: {state} {how}{note}", flush=True)
    pre, post, fail = counts["pre"], counts["post"], counts["fail"]
    print(f"result: {a.runs} runs, {pre} rolled back, {post} committed, {fail} partial")
    return 1 if counts["fail"] else 0


if __name__ == "__main__":
    sys.exit(main())
