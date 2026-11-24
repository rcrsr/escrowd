#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Repo checks (APP-MONOREPO-STANDARDS §8). Runs before `uv sync`: stdlib only.

    uv run --script scripts/check.py --staged FILE...   # lefthook pre-commit
    uv run --script scripts/check.py --tracked          # CI and the @escrowd/root lint target
    uv run --script scripts/check.py PATH...            # editors and agents

Suppress a finding on its line with `# check: allow CODE — <reason>`; a marker with no reason is
itself a finding. Legacy findings live in scripts/checks/baseline.json, which only shrinks.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from checks.runner import main

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
