#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Stamp the root VERSION into every manifest that shows a version (APP-RL-1, APP-RL-2).

uv run --script scripts/versions.py           # fix:versions
uv run --script scripts/versions.py --check   # check:versions: exit 1 on drift
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# (file, pattern whose group 1 is the version). One match per file, in the named section.
MANIFESTS = [
    ("Cargo.toml", re.compile(r'(?ms)^\[workspace\.package\]\n(?:[^\[]*?\n)?version = "([^"]+)"')),
    ("libs/python-sdk/pyproject.toml", re.compile(r'(?ms)^\[project\]\n(?:[^\[]*?\n)?version = "([^"]+)"')),
    ("libs/testkit/pyproject.toml", re.compile(r'(?ms)^\[project\]\n(?:[^\[]*?\n)?version = "([^"]+)"')),
]


def main(argv: list[str]) -> int:
    check = "--check" in argv
    want = (ROOT / "VERSION").read_text().strip()
    drift = []
    for rel, pattern in MANIFESTS:
        path = ROOT / rel
        text = path.read_text()
        m = pattern.search(text)
        if not m:
            drift.append(f"{rel}: no version field found")
            continue
        if m.group(1) == want:
            continue
        drift.append(f"{rel}: {m.group(1)} != VERSION {want}")
        if not check:
            path.write_text(text[: m.start(1)] + want + text[m.end(1) :])
    for line in drift:
        print(line)  # noqa: T201  a CLI whose stdout is its report
    return 1 if check and drift else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
