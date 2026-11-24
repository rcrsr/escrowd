"""Findings, rules and the per-file source cache (APP-CK-2: each file is read once)."""

import re
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from functools import cached_property
from pathlib import Path

ALLOW = re.compile(r"check: allow (?P<code>[A-Z]+\d{3})(?P<reason>\s*[—-]+\s*\S.*)?")


@dataclass(frozen=True, order=True)
class Finding:
    path: str
    line: int
    code: str
    message: str

    def key(self) -> str:
        """Baseline key: line numbers drift, so a finding is its path, code and message."""
        return f"{self.path}|{self.code}|{self.message}"

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: {self.code} {self.message}"


@dataclass
class SourceFile:
    root: Path
    rel: str

    @cached_property
    def text(self) -> str:
        try:
            return (self.root / self.rel).read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
            return ""

    @cached_property
    def lines(self) -> list[str]:
        return self.text.splitlines()

    def allowed(self, line: int, code: str) -> bool:
        if not 1 <= line <= len(self.lines):
            return False
        m = ALLOW.search(self.lines[line - 1])
        return bool(m and m.group("code") == code and m.group("reason"))


@dataclass
class Repo:
    """The tree a run sees: tracked files plus the subset being checked."""

    root: Path
    tracked: list[str]
    files: dict[str, SourceFile] = field(default_factory=dict)

    def source(self, rel: str) -> SourceFile:
        if rel not in self.files:
            self.files[rel] = SourceFile(self.root, rel)
        return self.files[rel]


@dataclass(frozen=True)
class FileRule:
    """Reads one file's text; imports no project code (§8 step 2)."""

    code: str
    policy: str
    applies: Callable[[str], bool]
    check: Callable[[SourceFile], Iterable[tuple[int, str]]]


@dataclass(frozen=True)
class TreeRule:
    """Needs the whole tracked tree; runs only with --tracked (§8 step 3)."""

    code: str
    policy: str
    check: Callable[[Repo], Iterable[tuple[str, int, str]]]
