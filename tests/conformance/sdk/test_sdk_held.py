"""Phase 3.5: held decisions in the Python SDK.

`escrow.scope(..., wait=True)` (the default) returns after the reviewers' verdict;
`wait=False` returns with `s.outcome.status == "held"`, and `s.wait_decided()` or
`await s.decided()` gives the verdict. A reviewer's return reopens the scope, which the
app resumes (`resume=`). A stand-in reviewer thread decides on the review socket of
the app's `escrow run`. The test app's checks (`test_app.py`) cover exit criterion 1.
"""

import pytest

from escrow_testkit.apps import Sdk, StandIn

DENY_PEM = """write:
  deny: ['*.pem']
"""

POLICY = """review:
  - {paths: ['src/**'], tier: llm}
"""


@pytest.fixture
def sdk(escrow_bin, runtime_dir):
    import subprocess

    s = Sdk(escrow_bin, runtime_dir, policy=POLICY)
    yield s
    for m in (runtime_dir / "run" / "escrowd").glob("*/view"):
        subprocess.run(["fusermount3", "-u", "-z", m], capture_output=True)


def test_a_scope_waits_for_its_reviewers_by_default(sdk):
    with StandIn(sdk, [("commit", ["fine"])]):
        out = sdk.run("""
            with escrow.scope("t") as s:
                os.makedirs("src")
                subprocess.run(["sh", "-c", "echo x > src/a.py"], check=True)
            o = s.outcome
            out["outcome"] = [o.status, o.paths, o.reasons, o.tiers]
            (ch,) = [c for c in o.changes.changes if c.path == "src/a.py"]
            out["writers"] = [p.args[:2] for p in o.changes.writers(ch)]
        """)
    assert out["outcome"] == ["committed", ["src", "src/a.py"], ["llm: fine"], []]
    assert out["writers"] == [["sh", "-c"]]
    assert (sdk.project / "src" / "a.py").read_text() == "x\n"


def test_without_wait_the_outcome_is_held_until_asked(sdk):
    with StandIn(sdk, [("discard", ["no"]), ("commit", [])]):
        out = sdk.run("""
            with escrow.scope("t", wait=False) as s:
                os.makedirs("src")
                Path("src/a.py").write_text("a\\n")
            out["held"] = [s.outcome.status, s.outcome.tiers, s.outcome.wait]
            out["final"] = s.wait_decided(timeout=20).status
            out["again"] = s.wait_decided().status

            async def main():
                async with escrow.scope("u", wait=False) as u:
                    Path("src").mkdir()
                    Path("src/b.py").write_text("b\\n")
                out["async_held"] = u.outcome.status
                out["async_final"] = (await u.decided(timeout=20)).status

            asyncio.run(main())
        """)
    # A required rule (src/**) sets the outcome's wait although the scope did not.
    assert out["held"] == ["held", ["llm"], True]
    assert (out["final"], out["again"]) == ("discarded", "discarded")
    assert (out["async_held"], out["async_final"]) == ("held", "committed")
    assert not (sdk.project / "src" / "a.py").exists()
    assert (sdk.project / "src" / "b.py").read_text() == "b\n"


def test_an_async_scope_waits_without_blocking_the_loop(sdk):
    with StandIn(sdk, [("commit", [])], delay=0.5):
        out = sdk.run("""
            ticks = []

            async def tick():
                while True:
                    ticks.append(1)
                    await asyncio.sleep(0.02)

            async def main():
                t = asyncio.create_task(tick())
                async with escrow.scope("t") as s:
                    Path("src").mkdir()
                    Path("src/a.py").write_text("a\\n")
                t.cancel()
                return s

            s = asyncio.run(main())
            out["status"], out["ticks"] = s.outcome.status, len(ticks)
        """)
    assert out["status"] == "committed"
    assert out["ticks"] > 10  # the loop ran while the scope waited for its reviewer


def test_a_reviewers_return_reopens_the_scope_for_a_fix(sdk):
    with StandIn(sdk, [("return", ["add a test"]), ("commit", [])]):
        out = sdk.run("""
            with escrow.scope("t", session="agent") as s:
                Path("src").mkdir()
                Path("src/a.py").write_text("a\\n")
            out["first"] = [s.outcome.status, s.outcome.reasons, s.outcome.reopened]
            with escrow.scope(resume=s) as fix:
                Path("src/a_test.py").write_text("t\\n")
            out["fix"] = [fix.id == s.id, fix.session, fix.outcome.status]
        """)
    assert out["first"] == ["returned", ["llm: add a test"], True]
    assert out["fix"] == [True, "agent", "committed"]
    assert (sdk.project / "src" / "a_test.py").exists()


def test_settling_unscoped_io_waits_for_the_reviewers(sdk):
    with StandIn(sdk, [("commit", [])]):
        out = sdk.run(
            """
            Path("src").mkdir()
            Path("src/u.py").write_text("u\\n")
            out["status"] = escrow.settle_unscoped().status
            """,
            mode="implicit",
        )
    assert out["status"] == "committed"
    assert (sdk.project / "src" / "u.py").read_text() == "u\n"


def test_the_sdk_sees_the_review_and_the_tightened_outcome(escrow_bin, runtime_dir):
    sdk = Sdk(escrow_bin, runtime_dir, policy=DENY_PEM)
    out = sdk.run(
        """
        seen = {}

        def look(cs):
            seen["review"] = [cs.review.verdict, cs.review.reasons, cs.review.tiers]
            return escrow.commit()

        with escrow.scope("t", decide=look) as s:
            (P / "a.pem").write_text("k\\n")
        out["review"] = seen["review"]
        out["outcome"] = [s.outcome.status, s.outcome.reasons]
        """
    )
    assert out["review"] == ["discard", ["write.deny: a.pem"], []]
    assert out["outcome"] == ["discarded", ["write.deny: a.pem"]]
    assert not (sdk.project / "a.pem").exists()
