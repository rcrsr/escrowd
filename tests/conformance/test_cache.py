"""Phase 2.3: kernel caching.

A scope's view changes only through its own requests, so the kernel caches its
entries, attributes and absent names for 60 s, keeps its listings, and keeps a file's
pages across opens while the base file's version is unchanged. What must still reach the scope: an
editor's change to a base file (at the next open), and the base again after the
unscoped root resets.

Not covered: with the writeback cache the kernel keeps its own size for a file it
has an inode for, so an editor's change to a file's size reaches a scope that already
looked the file up only once the kernel drops the inode (a limit since phase 0).
"""

import os

import escrow


def test_an_outside_edit_reaches_the_next_open(daemon):
    d = daemon
    (d.project / "f.txt").write_text("a\n")
    with escrow.connect(str(d.socket)) as c:
        sid = c.open_scope().scope_id
    view = d.mount / sid / "f.txt"
    assert view.read_text() == "a\n"
    assert view.read_text() == "a\n"  # kept pages
    (d.project / "f.txt").write_text("b\n")  # same size: see the module doc
    assert view.read_text() == "b\n"


def test_names_come_and_go_through_the_cache(daemon):
    d = daemon
    with escrow.connect(str(d.socket)) as c:
        sid = c.open_scope().scope_id
    root = d.mount / sid
    assert os.listdir(root) == []  # a cached listing
    assert not (root / "n.txt").exists()  # cached as absent
    (root / "n.txt").write_text("n\n")
    assert (root / "n.txt").read_text() == "n\n"
    assert os.listdir(root) == ["n.txt"]
    os.unlink(root / "n.txt")
    assert not (root / "n.txt").exists()
    assert os.listdir(root) == []


def test_a_reset_root_shows_what_the_discarded_scope_deleted(start_daemon):
    d = start_daemon(unscoped="implicit")
    (d.project / "g.txt").write_text("g\n")
    root = d.mount / "unscoped"
    os.unlink(root / "g.txt")
    assert not (root / "g.txt").exists()
    assert os.listdir(root) == []
    with escrow.connect(str(d.socket)) as c:
        c.settle_unscoped()
        c.discard("unscoped")
    assert (root / "g.txt").read_text() == "g\n"
    assert os.listdir(root) == ["g.txt"]


def test_a_large_listing_is_whole_with_attributes(daemon):
    """Over many reply buffers: base entries, the scope's new ones, none it deleted,
    and each entry's attributes (READDIRPLUS)."""
    d = daemon
    (d.project / "big").mkdir()
    for i in range(1500):
        (d.project / "big" / f"base-{i:04}-{'x' * 40}").write_text("b" * (i % 7))
    with escrow.connect(str(d.socket)) as c:
        sid = c.open_scope().scope_id
    big = d.mount / sid / "big"
    for i in range(0, 1500, 3):
        os.unlink(big / f"base-{i:04}-{'x' * 40}")
    for i in range(500):
        (big / f"new-{i:04}").write_text("n" * (i % 5))
    want = {f"base-{i:04}-{'x' * 40}": i % 7 for i in range(1500) if i % 3}
    want |= {f"new-{i:04}": i % 5 for i in range(500)}
    with os.scandir(big) as it:
        got = {e.name: e.stat(follow_symlinks=False).st_size for e in it}
    assert got == want


def test_base_files_read_whole_small_and_large(daemon):
    """A first open stores a small file's pages ahead of the reads; a large one is read
    on demand. Both read back byte for byte, twice."""
    d = daemon
    blobs = {
        name: os.urandom(size)
        for name, size in [("small", 100_000), ("large", 300_000), ("empty", 0)]
    }
    for name, data in blobs.items():
        (d.project / name).write_bytes(data)
    with escrow.connect(str(d.socket)) as c:
        sid = c.open_scope().scope_id
    for _ in range(2):
        for name, data in blobs.items():
            assert (d.mount / sid / name).read_bytes() == data
