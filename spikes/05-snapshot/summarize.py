"""Median create/delete seconds per filesystem and mechanism: python3 summarize.py < log"""

import re
import statistics
import sys
from collections import defaultdict

t = defaultdict(list)
for line in sys.stdin:
    m = re.match(r"time fs=(\S+) mech=(\S+) op=(\S+) secs=(\S+)", line)
    if m:
        t[(m[1], m[2], m[3])].append(float(m[4]))
names = {"full": "full copy", "reflink": "reflink copy", "subvol": "btrfs subvolume snapshot", "statall": "stat every file (version check)"}
print("| Filesystem | Mechanism | Create median (ms) | Create min–max (ms) | Delete median (ms) |")
print("| --- | --- | --- | --- | --- |")
for fs in ("ext4", "xfs", "btrfs"):
    for mech in ("full", "reflink", "subvol", "statall"):
        c = t.get((fs, mech, "create"))
        if not c:
            continue
        d = t.get((fs, mech, "delete"))
        dm = f"{statistics.median(d) * 1000:.0f}" if d else "–"
        print(f"| {fs} | {names[mech]} | {statistics.median(c) * 1000:.0f} | {min(c) * 1000:.0f}–{max(c) * 1000:.0f} | {dm} |")
