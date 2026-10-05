"""Medians per repository, workload, step and mode from run.sh output.

    python3 bench/summarize.py < log

`wall` is the whole run as the caller sees it (for escrow modes: daemon start, mount,
the steps, commit, unmount); `total` is the sum of the timed steps. A run whose test
count differs from native's is flagged. A run that reports an error, or a negative step
(the wall clock stepped: WSL syncs its clock), is listed and left out.
"""

import re
import statistics
import sys
from collections import defaultdict

data = defaultdict(list)  # (repo, workload, step, mode) -> [seconds]
passing = defaultdict(set)  # (repo, mode) -> {count}
errors = []
for line in sys.stdin:
    if not line.startswith("run="):
        continue
    if "error=" in line:
        errors.append(line.strip())
        continue
    kv = dict(re.findall(r"(\w+)=(\S+)", line))
    if any(v.startswith("-") for v in kv.values()):
        errors.append(line.strip() + "  (clock step)")
        continue
    key = (kv["repo"], kv["workload"])
    for step, val in kv.items():
        if step in ("run", "mode", "repo", "workload"):
            continue
        if step == "passing":
            passing[(kv["repo"], kv["mode"])].add(val)
        else:
            data[(*key, step, kv["mode"])].append(float(val))

modes = sorted({m for (*_, m) in data if m != "native"})
med = statistics.median


def cells(values, base):
    return f"{med(values):.3f} | {med(values) / base:.2f}×" if values else "– | –"


heads = " | ".join(f"{m} (s) | {m} / native" for m in modes)
print(f"| Repo | Workload | Step | native (s) | {heads} |")
print("| --- | --- | --- | --- | " + " | ".join("--- | ---" for _ in modes) + " |")
groups = sorted({(r, w) for (r, w, _, _) in data})
for repo, wl in groups:
    seen = dict.fromkeys(s for (r, w, s, _) in data if (r, w) == (repo, wl))
    steps = [s for s in seen if s != "wall"]
    for step in steps:
        base = med(data[(repo, wl, step, "native")])
        row = " | ".join(cells(data.get((repo, wl, step, m)), base) for m in modes)
        print(f"| {repo} | {wl} | {step} | {base:.3f} | {row} |")

    def total(m, repo=repo, wl=wl, steps=steps):
        return sum(med(data[(repo, wl, s, m)]) for s in steps if data.get((repo, wl, s, m)))

    tn = total("native")
    row = " | ".join(f"{total(m):.3f} | {total(m) / tn:.2f}×" for m in modes)
    print(f"| {repo} | {wl} | **total** | {tn:.3f} | {row} |")
    wn = med(data[(repo, wl, "wall", "native")])
    row = " | ".join(cells(data.get((repo, wl, "wall", m)), wn) for m in modes)
    print(f"| {repo} | {wl} | **wall** | {wn:.3f} | {row} |")

print()
for repo in sorted({r for (r, _) in passing}):
    counts = {m: sorted(v) for (r, m), v in passing.items() if r == repo}
    same = len({c for v in counts.values() for c in v}) == 1
    print(f"tests passing, {repo}: {counts}" + ("" if same else "  ← MISMATCH"))
for e in errors:
    print("error:", e)
