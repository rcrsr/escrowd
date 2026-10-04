"""Project trees shared by the lifecycle and commit checks."""

import hashlib
import os
import subprocess


def fingerprint(root):
    """Every entry with mode, size, mtime and content digest: equal means byte-identical."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            p = os.path.join(dirpath, name)
            st = os.lstat(p)
            digest = ""
            if os.path.isfile(p) and not os.path.islink(p):
                digest = hashlib.sha256(open(p, "rb").read()).hexdigest()
            out.append((os.path.relpath(p, root), st.st_mode, st.st_size, st.st_mtime_ns, digest))
    return sorted(out)


def tree(root):
    """Every entry with mode and content (or link target), without times."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            p = os.path.join(dirpath, name)
            st = os.lstat(p)
            if os.path.islink(p):
                body = os.readlink(p)
            elif os.path.isfile(p):
                body = open(p, "rb").read()
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
    os.symlink("keep.txt", project / "link")


def mutate(root):
    """Writes, a rename, deletes, a no-op rewrite, a chmod and a symlink swap."""
    (root / "edit.txt").write_text("edited\n")
    (root / "gone.txt").unlink()
    os.rename(root / "old.txt", root / "renamed.txt")
    (root / "same.txt").write_text("same\n")  # copied up, content unchanged
    (root / "new" / "deep").mkdir(parents=True)
    (root / "new" / "deep" / "f.txt").write_text("f\n")
    os.chmod(root / "mode.txt", 0o600)
    subprocess.run(["rm", "-r", root / "tree"], check=True)
    (root / "dir" / "a.txt").unlink()
    os.unlink(root / "link")
    os.symlink("edit.txt", root / "link")


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
