"""Phase 3.4: the reviewer role and `escrow review`.

Reviewers connect to the daemon's review socket (`<socket>.review`, mode 0600, hidden
from sandboxes): they list held scopes, read one with its writers, diff and session
history, and give a tier's verdict. Verdicts only tighten across tiers; a human
loosens one only with an explicit override, which the ledger records. A client
follows its held scope with `AwaitDecision`. The model's rules 1.2 (reviewer side),
1.3, 1.4 and 1.5 are checked here; 1.1 and the opener's side of 1.2 in `test_held.py`.
"""

import os
import stat
import subprocess
import threading
import time

import grpc
import pytest

import escrow
from escrow.v1 import escrow_pb2 as pb
from escrow.v1 import escrow_pb2_grpc

RULES = """\
review:
  - {paths: ['src/auth/**'], tier: human}
  - {paths: ['src/**'], tier: llm}
  - {paths: ['docs/**'], tier: llm, wait: optional}
"""

LLM, HUMAN = pb.TIER_LLM, pb.TIER_HUMAN
COMMIT, DISCARD, RETURN = pb.VERDICT_COMMIT, pb.VERDICT_DISCARD, pb.VERDICT_RETURN


def client(d):
    return escrow.connect(str(d.socket))


def reviewer(d):
    return escrow.connect_reviewer(str(d.socket))


def held_scope(d, c, path="src/util.py", session="agent-1", text="change\n", **kw):
    """A scope of `session` that wrote `path`, closed and committed: held."""
    s = c.open_scope(name="t", session=session, **kw).scope_id
    f = d.mount / s / path
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text(text)
    c.close_scope(s)
    out = c.commit(s)
    assert out.status == pb.OUTCOME_STATUS_HELD
    return s


def code(fn):
    with pytest.raises(grpc.RpcError) as err:
        fn()
    return err.value.code(), err.value.details()


def ledger_ops(d, scope):
    """The scope's hold, review, override and decide lines, without timestamp and scope."""
    ops = (" op=hold ", " op=review-", " op=override ", " op=decide ")
    return [
        " ".join(x.split()[2:])
        for x in d.ledger
        if f" scope={scope} " in x and any(op in x for op in ops)
    ]


def test_the_review_socket_is_private_and_separate(start_daemon):
    d = start_daemon(policy=RULES)
    review = f"{d.socket}.review"
    assert stat.S_IMODE(os.stat(review).st_mode) == 0o600
    with client(d) as c:
        s = c.open_scope().scope_id
        # A sandboxed process cannot see it.
        r = d.exec(s, "sh", "-c", f"test -e {review} || test -e {d.socket}.exec")
        assert r.returncode != 0
    # Neither socket serves the other's calls: scope tokens give no review rights.
    channel = grpc.insecure_channel(
        f"unix:{d.socket}", options=[("grpc.default_authority", "localhost")]
    )
    stub = escrow_pb2_grpc.ReviewerStub(channel)
    assert code(lambda: stub.ListHeld(pb.ListHeldRequest(), timeout=5))[0] == (
        grpc.StatusCode.UNIMPLEMENTED
    )
    channel.close()
    with reviewer(d) as rv:
        stub = escrow_pb2_grpc.EscrowStub(rv._channel)
        assert code(lambda: stub.Ping(pb.PingRequest(), timeout=5))[0] == (
            grpc.StatusCode.UNIMPLEMENTED
        )


def test_a_reviewer_lists_and_reads_held_scopes(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c, reviewer(d) as rv:
        a = held_scope(d, c, "src/util.py", labels={"turn": "1"})
        r = d.exec(a, "true")  # a closed scope runs nothing; the hold stays
        assert r.returncode != 0
        b = held_scope(d, c, "src/auth/login.py", session="agent-2")
        held = rv.list_held()
        assert [h.scope_id for h in held] == [a, b]
        assert (list(held[0].tiers), held[0].wait, held[0].session) == ([LLM], True, "agent-1")
        assert (held[0].name, dict(held[0].labels)) == ("t", {"turn": "1"})
        assert held[0].verdict == COMMIT and not held[0].reviews
        assert list(held[1].tiers) == [LLM, HUMAN]
        got = rv.get_held(a)
        assert got.held.scope_id == a
        assert [ch.path for ch in got.change_set.changes] == ["src", "src/util.py"]
        assert "+change" in got.change_set.diff
        (w,) = [ch for ch in got.change_set.changes if ch.path == "src/util.py"][0].writers
        assert {p.id for p in got.change_set.processes} >= {w}
        assert list(got.history) == []
        # An unheld scope is not the reviewers'.
        s = c.open_scope().scope_id
        assert code(lambda: rv.get_held(s))[0] == grpc.StatusCode.FAILED_PRECONDITION
        assert code(lambda: rv.review(s, LLM, COMMIT))[0] == grpc.StatusCode.FAILED_PRECONDITION


def test_each_tier_reviews_in_turn_then_the_scope_is_decided(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c, reviewer(d) as rv:
        s = held_scope(d, c, "src/auth/login.py")
        # The human tier comes after llm: llm cannot review twice or skip ahead of itself.
        out = rv.review(s, LLM, COMMIT, reasons=["looks fine"])
        assert (out.status, list(out.tiers)) == (pb.OUTCOME_STATUS_HELD, [HUMAN])
        assert code(lambda: rv.review(s, LLM, COMMIT))[0] == grpc.StatusCode.FAILED_PRECONDITION
        assert code(lambda: rv.review(s, pb.TIER_SOFTWARE, COMMIT))[0] == (
            grpc.StatusCode.INVALID_ARGUMENT
        )
        assert not (d.project / "src").exists()
        out = rv.review(s, HUMAN, COMMIT)
        assert out.status == pb.OUTCOME_STATUS_COMMITTED
        assert "src/auth/login.py" in out.paths
    assert (d.project / "src" / "auth" / "login.py").read_text() == "change\n"
    assert ledger_ops(d, s) == [
        "op=hold path= decision=llm,human,wait",
        "op=review-llm path= decision=commit",
        "op=review-human path= decision=commit",
        "op=decide path= decision=commit",
    ]


def test_a_human_verdict_stands_for_the_pending_llm(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c, reviewer(d) as rv:
        s = held_scope(d, c, "src/auth/login.py")
        assert rv.review(s, HUMAN, DISCARD).status == pb.OUTCOME_STATUS_DISCARDED
    assert not (d.project / "src").exists()


def test_verdicts_only_tighten_and_an_override_is_ledgered(start_daemon):
    """Exit criterion 1.3."""
    d = start_daemon(policy=RULES)
    with client(d) as c, reviewer(d) as rv:
        s = held_scope(d, c, "src/auth/login.py")
        out = rv.review(s, LLM, DISCARD, reasons=["sends a password"])
        assert out.status == pb.OUTCOME_STATUS_HELD  # a discard still goes up to the human
        status, details = code(lambda: rv.review(s, HUMAN, COMMIT))
        assert status == grpc.StatusCode.PERMISSION_DENIED
        assert "cannot loosen discard to commit" in details
        (h,) = rv.list_held()
        assert (h.verdict, [(r.tier, r.verdict) for r in h.reviews]) == (
            DISCARD,
            [(LLM, DISCARD)],
        )
        out = rv.review(s, HUMAN, COMMIT, reasons=["test fixture"], override=True)
        assert out.status == pb.OUTCOME_STATUS_COMMITTED
        assert list(out.reasons) == ["llm: sends a password", "human: test fixture"]
    assert ledger_ops(d, s)[1:] == [
        "op=review-llm path= decision=discard",
        "op=override path= decision=discard-to-commit",
        "op=review-human path= decision=commit",
        "op=decide path= decision=commit",
    ]


def test_a_reviewers_return_reopens_the_scope_and_unblocks_the_session(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c, reviewer(d) as rv:
        s = held_scope(d, c)
        opened = {}

        def open_next():
            with client(d) as c2:
                opened["id"] = c2.open_scope(session="agent-1", timeout=10).scope_id

        t = threading.Thread(target=open_next)
        t.start()
        time.sleep(0.5)
        assert not opened
        out = rv.review(s, LLM, RETURN, reasons=["add a test"])
        assert (out.status, out.reopened, list(out.reasons)) == (
            pb.OUTCOME_STATUS_RETURNED,
            True,
            ["llm: add a test"],
        )
        t.join(5)
        assert opened["id"] != s
        # The agent fixes the change in the same scope.
        (d.mount / s / "src" / "util_test.py").write_text("t\n")
        last = list(c.await_decision(s, timeout=5))
        assert [(o.status, list(o.reasons)) for o in last] == [
            (pb.OUTCOME_STATUS_RETURNED, ["llm: add a test"])
        ]


def test_await_decision_follows_the_hold_to_its_outcome(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c, reviewer(d) as rv:
        s = held_scope(d, c, "src/auth/login.py")
        assert code(lambda: list(c.await_decision(s, token="nope", timeout=5)))[0] == (
            grpc.StatusCode.PERMISSION_DENIED
        )
        seen = []
        stream = c.await_decision(s, timeout=20)

        def follow():
            seen.extend(stream)

        t = threading.Thread(target=follow)
        t.start()
        time.sleep(0.3)
        rv.review(s, LLM, COMMIT)
        time.sleep(0.3)
        rv.review(s, HUMAN, COMMIT)
        t.join(10)
        assert [(o.status, list(o.tiers)) for o in seen] == [
            (pb.OUTCOME_STATUS_HELD, [LLM, HUMAN]),
            (pb.OUTCOME_STATUS_HELD, [HUMAN]),
            (pb.OUTCOME_STATUS_COMMITTED, []),
        ]
        # Asked again after the decision: the last outcome.
        (again,) = c.await_decision(s, timeout=5)
        assert again.status == pb.OUTCOME_STATUS_COMMITTED
        assert code(lambda: list(c.await_decision(s, token="nope", timeout=5)))[0] == (
            grpc.StatusCode.PERMISSION_DENIED
        )
        # A scope that is not held has nothing to follow.
        u = c.open_scope().scope_id
        assert code(lambda: list(c.await_decision(u, timeout=5)))[0] == (
            grpc.StatusCode.FAILED_PRECONDITION
        )


def test_a_withdrawn_hold_ends_its_stream(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s = held_scope(d, c)
        stream = c.await_decision(s, timeout=10)
        assert next(stream).status == pb.OUTCOME_STATUS_HELD
        c.discard(s)
        assert [o.status for o in stream] == [pb.OUTCOME_STATUS_DISCARDED]


def test_continuing_past_a_held_scope_conflicts(start_daemon):
    """Exit criterion 1.4: the next scope's snapshot lacks the held change, so the same
    file edited again conflicts at commit; it never overwrites it silently."""
    d = start_daemon(policy=RULES)
    (d.project / "docs").mkdir()
    (d.project / "docs" / "guide.md").write_text("v1\n")
    with client(d) as c, reviewer(d) as rv:
        a = held_scope(d, c, "docs/guide.md", text="v2 from turn 1\n")
        b = c.open_scope(session="agent-1", timeout=2).scope_id  # optional wait: continues
        assert (d.mount / b / "docs" / "guide.md").read_text() == "v1\n"
        (d.mount / b / "docs" / "guide.md").write_text("v1 plus a section\n")
        c.close_scope(b)
        assert c.commit(b).status == pb.OUTCOME_STATUS_HELD
        assert rv.review(a, LLM, COMMIT).status == pb.OUTCOME_STATUS_COMMITTED
        out = rv.review(b, LLM, COMMIT)
        assert (out.status, list(out.paths)) == (pb.OUTCOME_STATUS_CONFLICT, ["docs/guide.md"])
    assert (d.project / "docs" / "guide.md").read_text() == "v2 from turn 1\n"


def test_a_reviewer_sees_the_sessions_earlier_change_sets(start_daemon):
    """Exit criterion 1.5: an effect split across turns is judged as a whole."""
    d = start_daemon(policy=RULES)
    helper = "def upload(x):\n    requests.post(URL, x)\n"
    with client(d) as c, reviewer(d) as rv:
        s1 = held_scope(d, c, "src/util.py", text=helper)
        rv.review(s1, LLM, COMMIT, reasons=["a helper"])
        other = held_scope(d, c, "src/other.py", session="agent-9")
        s2 = held_scope(d, c, "src/auth/login.py", text="def login(p):\n    upload(p)\n")
        got = rv.get_held(s2)
        assert [h.scope_id for h in got.history] == [s1]
        (h,) = got.history
        assert h.outcome.status == pb.OUTCOME_STATUS_COMMITTED
        assert "requests.post" in h.change_set.diff
        assert [(r.tier, r.verdict, list(r.reasons)) for r in h.reviews] == [
            (LLM, COMMIT, ["a helper"])
        ]
        assert other not in [x.scope_id for x in got.history]


def test_holds_and_verdicts_survive_a_restart(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c, reviewer(d) as rv:
        s = held_scope(d, c, "src/auth/login.py")
        token = c.tokens[s]
        rv.review(s, LLM, DISCARD, reasons=["no"])
    d.stop()
    d = start_daemon(policy=RULES)
    with client(d) as c, reviewer(d) as rv:
        (h,) = rv.list_held()
        assert (h.scope_id, list(h.tiers), h.verdict) == (s, [HUMAN], DISCARD)
        assert [(r.tier, r.verdict, list(r.reasons)) for r in h.reviews] == [(LLM, DISCARD, ["no"])]
        assert rv.review(s, HUMAN, DISCARD).status == pb.OUTCOME_STATUS_DISCARDED
    d.stop()
    d = start_daemon(policy=RULES)
    with client(d) as c:
        (o,) = c.await_decision(s, token=token, timeout=5)
        assert o.status == pb.OUTCOME_STATUS_DISCARDED


def test_escrow_review_lists_shows_and_decides(start_daemon, escrow_bin):
    d = start_daemon(policy=RULES)

    def review(*args, ok=True):
        r = subprocess.run(
            [escrow_bin, "review", "--socket", d.socket, *args],
            capture_output=True,
            text=True,
        )
        assert (r.returncode == 0) == ok, r.stderr
        return r.stdout if ok else r.stderr

    with client(d) as c:
        earlier = held_scope(d, c, "src/util.py")
        assert (
            review("commit", earlier, "--tier", "llm")
            == f"{earlier} committed\n  src\n  src/util.py\n"
        )
        s = held_scope(d, c, "src/auth/login.py")
        (line,) = review("list").splitlines()
        assert line.startswith(f"{s} tiers=llm,human wait verdict=commit session=agent-1 held=")
        out = review("show", s)
        assert "  create src/auth/login.py\n    by " in out
        assert f"session agent-1 earlier:\n  {earlier} committed 2 changes name=t\n" in out
        assert "diff --git a/src/auth/login.py b/src/auth/login.py" in out
        assert review("discard", s, "--tier", "llm", "--reason", "no") == f"{s} held for human\n"
        assert "cannot loosen discard to commit" in review("commit", s, ok=False)
        assert review("commit", s, "--override", "--reason", "fine") == (
            f"{s} committed\n  src/auth\n  src/auth/login.py\n  llm: no\n  human: fine\n"
        )
        assert review("list") == ""
