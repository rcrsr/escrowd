"""Run the rules over staged, tracked or named files and ratchet against the baseline."""

import argparse
import json
import subprocess
from pathlib import Path

from checks.model import ALLOW, Finding, Repo
from checks.rules import FILE_RULES, TREE_RULES

ROOT = Path(__file__).resolve().parents[2]
BASELINE = Path(__file__).resolve().parent / "baseline.json"


def tracked_files(root: Path) -> list[str]:
    out = subprocess.run(["git", "ls-files", "-z"], cwd=root, check=True, capture_output=True).stdout
    return [p for p in out.decode().split("\0") if p]


def run(repo: Repo, paths: list[str], tree: bool) -> list[Finding]:
    found: list[Finding] = []
    for rel in paths:
        src = repo.source(rel)
        for rule in FILE_RULES:
            if rule.applies(rel):
                found += [
                    Finding(rel, n, rule.code, msg) for n, msg in rule.check(src) if not src.allowed(n, rule.code)
                ]
        # APP-CK-5: a suppression marker without a reason is a finding.
        for n, line in enumerate(src.lines, 1):
            m = ALLOW.search(line)
            if m and not m.group("reason"):
                found.append(Finding(rel, n, "CHK001", f"`check: allow {m.group('code')}` needs `— <reason>`"))
    if tree:
        for rule in TREE_RULES:
            for rel, n, msg in rule.check(repo):
                if not repo.source(rel).allowed(n, rule.code):
                    found.append(Finding(rel, n, rule.code, msg))
    return sorted(set(found))


def load_baseline() -> list[str]:
    return json.loads(BASELINE.read_text()) if BASELINE.is_file() else []


def main(argv: list[str], root: Path = ROOT) -> int:
    ap = argparse.ArgumentParser(prog="check.py")
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--staged", action="store_true", help="check the given staged files (lefthook)")
    mode.add_argument("--tracked", action="store_true", help="check every tracked file, plus tree rules (CI)")
    ap.add_argument("paths", nargs="*")
    args = ap.parse_args(argv)

    tracked = tracked_files(root)
    repo = Repo(root, tracked)
    paths = tracked if args.tracked else [str(Path(p)) for p in args.paths]
    findings = run(repo, paths, tree=args.tracked)

    baseline = load_baseline()
    keys = {f.key() for f in findings}
    new = [f for f in findings if f.key() not in baseline]
    # APP-CK-6: the baseline only shrinks; an entry that no longer fires must be deleted.
    stale = [k for k in baseline if k not in keys] if args.tracked else []
    for f in new:
        print(f)  # noqa: T201  a CLI whose stdout is its report
    for k in stale:
        print(f"{BASELINE.name}: stale entry, delete it: {k}")  # noqa: T201  same CLI report
    return 1 if new or stale else 0
