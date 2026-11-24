"""escrowd's repo rules. Each message cites the standard element it enforces (APP-CK-3)."""

import re
import tomllib
from collections.abc import Iterable
from pathlib import PurePosixPath

from checks.model import FileRule, Repo, SourceFile, TreeRule

STD = "APP-MONOREPO-STANDARDS"

# SEC001: files that must never be committed (APP-LG-2). Templates are fine.
SECRET_NAMES = re.compile(r"^(\.env(\..+)?|id_(rsa|dsa|ecdsa|ed25519)|credentials\.json|\.netrc|\.npmrc|\.pypirc)$")
SECRET_SUFFIXES = (".pem", ".key", ".p12", ".pfx", ".keystore")
SECRET_TEMPLATES = re.compile(r"\.(example|sample|template)$")


def is_secret_file(rel: str) -> bool:
    name = PurePosixPath(rel).name
    if SECRET_TEMPLATES.search(name):
        return False
    return bool(SECRET_NAMES.match(name)) or name.endswith(SECRET_SUFFIXES)


def check_secret(src: SourceFile) -> Iterable[tuple[int, str]]:
    yield 1, f"secret-bearing file must not be committed ({STD} APP-LG-2)"


# CI001: every `uses:` pins a full commit SHA with a trailing version comment (APP-CI-1).
USES = re.compile(r"^\s*(?:-\s*)?uses:\s*(?P<ref>\S+)(?P<rest>.*)$")
PINNED = re.compile(r"@[0-9a-f]{40}$")


def is_workflow(rel: str) -> bool:
    return rel.startswith(".github/") and rel.endswith((".yml", ".yaml"))


def check_action_pins(src: SourceFile) -> Iterable[tuple[int, str]]:
    for n, line in enumerate(src.lines, 1):
        m = USES.match(line)
        if not m or m.group("ref").startswith(("./", "docker://")):
            continue
        if not PINNED.search(m.group("ref")) or "#" not in m.group("rest"):
            yield n, f"`uses: {m.group('ref')}` must pin a commit SHA with a `# vX` comment ({STD} APP-CI-1)"


# TC001: rust-toolchain.toml's channel equals mise.toml's rust pin (APP-TC-5).
def check_rust_pin(repo: Repo) -> Iterable[tuple[str, int, str]]:
    try:
        mise = tomllib.loads(repo.source("mise.toml").text)
        chan = tomllib.loads(repo.source("rust-toolchain.toml").text)["toolchain"]["channel"]
    except (tomllib.TOMLDecodeError, KeyError):
        yield "rust-toolchain.toml", 1, f"rust-toolchain.toml and mise.toml must parse ({STD} APP-TC-5)"
        return
    pin = mise.get("tools", {}).get("rust")
    if pin != chan:
        yield "rust-toolchain.toml", 1, f"channel {chan!r} differs from mise.toml rust {pin!r} ({STD} APP-TC-5)"


# TC002: no per-tool version files beside mise.toml (APP-TC-4).
VERSION_FILES = {".nvmrc", ".python-version", ".tool-versions", ".node-version"}


def check_version_files(repo: Repo) -> Iterable[tuple[str, int, str]]:
    for rel in repo.tracked:
        if PurePosixPath(rel).name in VERSION_FILES and not rel.startswith("spikes/"):
            yield rel, 1, f"version files are replaced by mise.toml ({STD} APP-TC-4)"


# DOC001: every repo path CLAUDE.md cites in backticks exists (APP-OP-3 citation check).
CITED = re.compile(r"`(?P<path>(?:apps|libs|tests|docs|proto|scripts|packaging|bench|spikes|\.github)/[^`\s*{}<>]*)`")


def check_claude_citations(repo: Repo) -> Iterable[tuple[str, int, str]]:
    tracked = set(repo.tracked)
    dirs = {str(p) for rel in repo.tracked for p in PurePosixPath(rel).parents}
    src = repo.source("CLAUDE.md")
    for n, line in enumerate(src.lines, 1):
        for m in CITED.finditer(line):
            path = m.group("path").rstrip("/")
            if path not in tracked and path not in dirs:
                yield "CLAUDE.md", n, f"cites `{m.group('path')}`, which is not in the tree ({STD} APP-OP-3)"


FILE_RULES = [
    FileRule("SEC001", "APP-LG-2", is_secret_file, check_secret),
    FileRule("CI001", "APP-CI-1", is_workflow, check_action_pins),
]
TREE_RULES = [
    TreeRule("TC001", "APP-TC-5", check_rust_pin),
    TreeRule("TC002", "APP-TC-4", check_version_files),
    TreeRule("DOC001", "APP-OP-3", check_claude_citations),
]
