"""Median per workload, step and mode from run.sh output: python3 summarize.py < log"""

import re
import statistics
import sys
from collections import defaultdict

data = defaultdict(list)  # (workload, step, mode) -> [seconds]
passing = defaultdict(set)
for line in sys.stdin:
    if not line.startswith("run=") or "error=" in line:
        continue
    kv = dict(re.findall(r"(\w+)=(\S+)", line))
    for step, val in kv.items():
        if step in ("run", "mode", "workload"):
            continue
        if step == "passing":
            passing[kv["mode"]].add(val)
        else:
            data[(kv["workload"], step, kv["mode"])].append(float(val))

modes = sorted({m for (_, _, m) in data if m != "native"})
print("| Workload | Step | Native (s) | " + " | ".join(f"{m} (s) | {m} / native" for m in modes) + " |")
print("| --- | --- | --- | " + " | ".join("--- | ---" for _ in modes) + " |")
steps = sorted({(w, s) for (w, s, _) in data})
for wl, step in steps:
    mn = statistics.median(data[(wl, step, "native")])
    cells = []
    for m in modes:
        v = data.get((wl, step, m))
        cells.append(f"{statistics.median(v):.3f} | {statistics.median(v) / mn:.2f}×" if v else "– | –")
    print(f"| {wl} | {step} | {mn:.3f} | " + " | ".join(cells) + " |")
for wl in sorted({w for (w, _) in steps}):
    tot = lambda m: sum(statistics.median(data[(w, s, m)]) for (w, s) in steps if w == wl and data.get((w, s, m)))
    tn = tot("native")
    print(f"| {wl} | **total** | {tn:.3f} | " + " | ".join(f"{tot(m):.3f} | {tot(m) / tn:.2f}×" for m in modes) + " |")
print()
print("tests passing per mode:", {m: sorted(v) for m, v in passing.items()})
