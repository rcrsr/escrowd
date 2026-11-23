"""Declarative file scenarios from `tests/fixtures/scenarios`, replayed with plain file calls.

A scenario is `{"seed": [Op], "mutate": [Op]}`. `examples/test-app/app.py` carries its own
small interpreter of the same format, so the app never imports this package and the suite
can compare the two.
"""

import json
import os
from pathlib import Path

SCENARIOS = Path(__file__).resolve().parents[2] / "fixtures" / "scenarios"


def load(name: str) -> dict:
    """The scenario `name` (`seed` and `mutate` op lists)."""
    return json.loads((SCENARIOS / f"{name}.json").read_text())


def _fill(dir: str, s: str, path: str = "") -> str:
    return s.replace("{dir}", dir).replace("{path}", path)


def apply(ops: list[dict], root: Path, dir: str) -> None:
    """Run `ops` in order under `root`; `{dir}` becomes `dir` and `{path}` the op's path."""
    for op in ops:
        kind = op["op"]
        match kind:
            case "mkdir":
                (root / _fill(dir, op["path"])).mkdir(parents=True, exist_ok=True)
            case "write":
                path = _fill(dir, op["path"])
                (root / path).write_text(_fill(dir, op["text"], path))
            case "rename":
                os.rename(root / _fill(dir, op["from"]), root / _fill(dir, op["to"]))
            case "remove":
                os.remove(root / _fill(dir, op["path"]))
            case "chmod":
                os.chmod(root / _fill(dir, op["path"]), int(op["mode"], 8))
            case "symlink":
                os.symlink(_fill(dir, op["target"]), root / _fill(dir, op["path"]))
            case _:
                raise ValueError(f"unknown op {kind!r}")
