"""One passing and one failing fixture per rule (APP-CK-7). Run by the Nx project `tools`."""

import subprocess
from pathlib import Path

import pytest

from checks.model import Repo
from checks.runner import run

PIN = "a" * 40


def make(tmp_path: Path, files: dict[str, str]) -> Repo:
    for rel, text in files.items():
        (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
        (tmp_path / rel).write_text(text)
    return Repo(tmp_path, sorted(files))


def codes(repo: Repo, tree: bool = False) -> set[str]:
    return {f.code for f in run(repo, repo.tracked, tree=tree)}


BASE = {
    "mise.toml": '[tools]\nrust = "1.100.0"\n',
    "rust-toolchain.toml": '[toolchain]\nchannel = "1.100.0"\n',
    "CLAUDE.md": "See `docs/a.md`.\n",
    "docs/a.md": "x\n",
}


@pytest.mark.parametrize(
    ("files", "code", "fires"),
    [
        ({".env": "K=v\n"}, "SEC001", True),
        ({".env.example": "K=\n"}, "SEC001", False),
        ({"certs/server.pem": "x\n"}, "SEC001", True),
        ({".github/workflows/ci.yml": "steps:\n  - uses: actions/checkout@v7\n"}, "CI001", True),
        ({".github/workflows/ci.yml": f"steps:\n  - uses: actions/checkout@{PIN} # v7\n"}, "CI001", False),
        ({".github/workflows/ci.yml": "steps:\n  - uses: ./.github/actions/setup-toolchain\n"}, "CI001", False),
        (
            {".github/workflows/ci.yml": "  - uses: a/b@v1 # check: allow CI001 — vendor ships no tags\n"},
            "CI001",
            False,
        ),
        ({".github/workflows/ci.yml": "  - uses: a/b@v1 # check: " + "allow CI001\n"}, "CHK001", True),
    ],
)
def test_file_rules(tmp_path, files, code, fires):
    assert (code in codes(make(tmp_path, files))) is fires


@pytest.mark.parametrize(
    ("extra", "code", "fires"),
    [
        ({}, "TC001", False),
        ({"mise.toml": '[tools]\nrust = { version = "1.100.0", components = "rustfmt" }\n'}, "TC001", False),
        ({"rust-toolchain.toml": '[toolchain]\nchannel = "1.99.0"\n'}, "TC001", True),
        ({}, "TC002", False),
        ({".python-version": "3.15\n"}, "TC002", True),
        ({}, "DOC001", False),
        ({"CLAUDE.md": "See `docs/gone.md` and `libs/x/`.\n"}, "DOC001", True),
    ],
)
def test_tree_rules(tmp_path, extra, code, fires):
    assert (code in codes(make(tmp_path, BASE | extra), tree=True)) is fires


def test_the_repo_passes():
    root = Path(__file__).resolve().parents[2]
    out = subprocess.run(["uv", "run", "--script", "scripts/check.py", "--tracked"], cwd=root, capture_output=True)
    assert out.returncode == 0, out.stdout.decode() + out.stderr.decode()
