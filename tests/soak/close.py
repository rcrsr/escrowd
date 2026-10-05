"""Close soak (phase 2.2): no acknowledged write lost at close; close latency.

    uv run --project sdk/python --frozen python tests/soak/close.py [RUNS]

Loss: RUNS times, a writer in a scope's sandbox (a grandchild of the shell, as under a
test runner) appends a numbered line and then reports it, without pause; the scope
closes after a random delay. The scope's file must hold every reported line, in order.

Latency: close of a scope with 0, 1 and 10 running children (each exits on SIGTERM),
50 closes each, 50th and 99th percentiles.
"""

import argparse
import os
import random
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "conformance"))

from conftest import Daemon, make_runtime_dir  # noqa: E402
from test_exec import WRITER, spawn, wait_for  # noqa: E402

import escrow  # noqa: E402


def loss_run(d: Daemon) -> tuple[bool, int, int]:
    with escrow.connect(str(d.socket)) as c:
        sid = c.open_scope().scope_id
    p = spawn(d, sid, f"python3 -c '{WRITER}'; true")
    acked: list[str] = []
    drain = threading.Thread(target=lambda: acked.extend(p.stdout or ()))
    drain.start()
    wait_for(d.mount / sid / "log")
    time.sleep(random.uniform(0, 0.2))
    with escrow.connect(str(d.socket)) as c:
        c.close_scope(sid)
        p.wait(timeout=10)
        drain.join(timeout=10)
        complete = (d.upper(sid) / "log").read_text().split("\n")[:-1]
        c.discard(sid)  # keep the state directory small over 1,000 runs
    ok = complete == [str(i) for i in range(len(complete))] and len(acked) <= len(complete)
    return ok, len(acked), len(complete)


def latency(d: Daemon, children: int, n: int) -> list[float]:
    out = []
    for _ in range(n):
        with escrow.connect(str(d.socket)) as c:
            sid = c.open_scope().scope_id
        procs = [spawn(d, sid, f"touch ready{k}; exec sleep 600") for k in range(children)]
        for k in range(children):
            wait_for(d.mount / sid / f"ready{k}")
        start = time.monotonic()
        with escrow.connect(str(d.socket)) as c:
            c.close_scope(sid)
        out.append(time.monotonic() - start)
        for p in procs:
            p.wait(timeout=10)
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("runs", type=int, nargs="?", default=100)
    ap.add_argument("--bin", default=os.environ.get("ESCROW_BIN", ROOT / "target/debug/escrow"))
    a = ap.parse_args()
    d = Daemon(Path(a.bin), make_runtime_dir())
    lost = 0
    try:
        print(f"close soak: {a.runs} runs, {a.bin}, kernel {os.uname().release}")
        for i in range(1, a.runs + 1):
            ok, acked, kept = loss_run(d)
            lost += not ok
            if not ok or i % 100 == 0:
                print(f"run {i}: {'ok' if ok else 'LOST'} acked={acked} kept={kept}", flush=True)
        for k in (0, 1, 10):
            t = sorted(latency(d, k, 50))
            p50, p99 = statistics.median(t), t[int(len(t) * 0.99) - 1]
            print(f"close latency, {k} children: p50 {p50 * 1000:.1f} ms, p99 {p99 * 1000:.1f} ms")
        print(f"result: {a.runs} closes under load, {lost} lost acknowledged writes")
    finally:
        d.stop()
        subprocess.run(["rm", "-rf", d.work])
    return 1 if lost else 0


if __name__ == "__main__":
    sys.exit(main())
