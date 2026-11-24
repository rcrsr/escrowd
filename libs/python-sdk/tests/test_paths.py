"""Property tests for the SDK's path mapping between the project and a scope view (APP-TS-4)."""

from hypothesis import given
from hypothesis import strategies as st

from escrow._scope import _move, _under

segment = st.text(alphabet=st.characters(blacklist_characters="/\0", blacklist_categories=["Cs"]), min_size=1).filter(
    lambda s: s not in (".", "..")
)
relpath = st.lists(segment, min_size=0, max_size=4).map("/".join)
root = st.lists(segment, min_size=1, max_size=3).map(lambda parts: "/" + "/".join(parts))


@given(src=root, dst=root, rel=relpath)
def test_move_maps_under_src_to_under_dst_and_back(src, dst, rel):
    p = f"{src}/{rel}" if rel else src
    moved = _move(p, src, dst)
    assert _under(moved, dst)
    assert _move(moved, dst, src) == p


@given(r=root, rel=relpath)
def test_a_path_is_under_its_root_and_a_sibling_prefix_is_not(r, rel):
    assert _under(f"{r}/{rel}" if rel else r, r)
    assert not _under(r + "x", r)
