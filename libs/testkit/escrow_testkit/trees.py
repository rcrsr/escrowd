"""Project trees shared by the conformance checks and the soaks."""

import hashlib
import os
import subprocess
from pathlib import Path


def leftovers(root):
    """Temporary `.escrow-*` entries a commit or its recovery left under `root`."""
    return [n for _, dns, fns in os.walk(root) for n in dns + fns if n.startswith(".escrow-")]


def fingerprint(root):
    """Every entry with mode, size, mtime and content digest: equal means byte-identical."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            p = Path(dirpath, name)
            st = p.lstat()
            digest = ""
            if p.is_file() and not p.is_symlink():
                digest = hashlib.sha256(p.read_bytes()).hexdigest()
            out.append((os.path.relpath(p, root), st.st_mode, st.st_size, st.st_mtime_ns, digest))
    return sorted(out)


def tree(root):
    """Every entry with mode and content (or link target), without times."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            p = Path(dirpath, name)
            st = p.lstat()
            if p.is_symlink():
                body = os.readlink(p)  # noqa: PTH115  Path.readlink normalises the target; compare it raw
            elif p.is_file():
                body = p.read_bytes()
            else:
                body = b""
            out.append((os.path.relpath(p, root), st.st_mode, body))
    return sorted(out)


def seed(project):
    files = {
        "keep.txt": "keep\n",
        "edit.txt": "edit\n",
        "gone.txt": "gone\n",
        "old.txt": "old\n",
        "same.txt": "same\n",
        "mode.txt": "mode\n",
        "tree/x.txt": "x\n",
        "dir/a.txt": "a\n",
        "dir/b.txt": "b\n",
        ".env": "SECRET=1\n",
    }
    for rel, text in files.items():
        (project / rel).parent.mkdir(parents=True, exist_ok=True)
        (project / rel).write_text(text)
    (project / "link").symlink_to("keep.txt")


def mutate(root):
    """Writes, a rename, deletes, a no-op rewrite, a chmod and a symlink swap."""
    (root / "edit.txt").write_text("edited\n")
    (root / "gone.txt").unlink()
    (root / "old.txt").rename(root / "renamed.txt")
    (root / "same.txt").write_text("same\n")  # copied up, content unchanged
    (root / "new" / "deep").mkdir(parents=True)
    (root / "new" / "deep" / "f.txt").write_text("f\n")
    (root / "mode.txt").chmod(0o600)
    subprocess.run(["rm", "-r", root / "tree"], check=True)
    (root / "dir" / "a.txt").unlink()
    (root / "link").unlink()
    (root / "link").symlink_to("edit.txt")


EXPECTED = [
    ("delete", "dir/a.txt"),
    ("modify", "edit.txt"),
    ("delete", "gone.txt"),
    ("modify", "link"),
    ("modify", "mode.txt"),
    ("create", "new"),
    ("create", "new/deep"),
    ("create", "new/deep/f.txt"),
    ("rename", "renamed.txt", "old.txt"),
    ("delete", "tree"),
    ("delete", "tree/x.txt"),
]
