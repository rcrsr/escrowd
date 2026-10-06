"""Phase 3.2: held scopes and sessions.

A commit of a change set that needs a tier above software (policy `review:`) holds the
scope: nothing reaches the project, and only its reviewers commit or return it; the
opener can withdraw it (discard). A hold with a wait, required by the rule or asked for
by the client, blocks the next `OpenScope` of the scope's session until the verdict,
up to the client's deadline. The hold survives a daemon restart.
"""

import threading
import time

import grpc
import pytest

import escrow
from escrow.v1 import escrow_pb2 as pb

RULES = """\
review:
  - {paths: ['src/auth/**'], tier: human}
  - {paths: ['src/**'], tier: llm}
  - {paths: ['docs/**'], tier: llm, wait: optional}
"""


def client(d):
    return escrow.connect(str(d.socket))


def held_scope(d, c, path="src/util.py", session="agent-1", wait=False):
    """A scope of `session` that wrote `path`, closed and committed: held."""
    s = c.open_scope(session=session).scope_id
    f = d.mount / s / path
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text("change\n")
    c.close_scope(s)
    return s, c.commit(s, wait=wait)


def code(fn):
    with pytest.raises(grpc.RpcError) as err:
        fn()
    return err.value.code(), err.value.details()


def test_a_commit_that_needs_review_is_held(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s, out = held_scope(d, c, "src/auth/login.py")
        assert out.status == pb.OUTCOME_STATUS_HELD
        assert (list(out.tiers), out.wait) == ([pb.TIER_LLM, pb.TIER_HUMAN], True)
        assert not (d.project / "src").exists()
        # Still readable for review.
        assert "src/auth/login.py" in c.get_change_set(s).diff
    assert any(f" scope={s} op=hold path= decision=llm,human,wait" in line for line in d.ledger)


def test_the_opener_can_only_withdraw_a_held_scope(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s, _ = held_scope(d, c)
        for verdict in (pb.VERDICT_COMMIT, pb.VERDICT_RETURN):
            status, details = code(lambda v=verdict: c.decide(s, v))
            assert status == grpc.StatusCode.PERMISSION_DENIED
            assert f"scope {s} is held for llm" in details
        assert c.discard(s).status == pb.OUTCOME_STATUS_DISCARDED
    assert not (d.project / "src").exists()


def test_a_required_wait_blocks_the_session_until_the_deadline(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s, out = held_scope(d, c)
        assert out.wait  # src/** requires it although the client did not ask
        t0 = time.monotonic()
        status, details = code(lambda: c.open_scope(session="agent-1", timeout=0.6))
        assert status == grpc.StatusCode.FAILED_PRECONDITION
        assert f"scope {s} is held" in details
        assert 0.3 < time.monotonic() - t0 < 0.6  # the daemon answers before the deadline
        # Other sessions and scopes without one are not ordered behind it.
        c.open_scope(session="agent-2", timeout=2)
        c.open_scope(timeout=2)
        # The held scope stays held.
        assert code(lambda: c.commit(s))[0] == grpc.StatusCode.PERMISSION_DENIED


def test_withdrawing_the_held_scope_unblocks_the_session(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s, _ = held_scope(d, c)
        opened = {}

        def open_next():
            with client(d) as c2:
                opened["id"] = c2.open_scope(session="agent-1", timeout=10).scope_id
                opened["at"] = time.monotonic()

        t = threading.Thread(target=open_next)
        t.start()
        time.sleep(0.5)
        assert not opened  # still waiting
        withdrawn = time.monotonic()
        c.discard(s)
        t.join(5)
        assert opened["id"] != s and opened["at"] >= withdrawn


def test_an_optional_wait_blocks_only_when_the_client_waits(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        _, out = held_scope(d, c, "docs/guide.md")
        assert (out.status, out.wait) == (pb.OUTCOME_STATUS_HELD, False)
        c.open_scope(session="agent-1", timeout=2)  # the agent continues
        s, out = held_scope(d, c, "docs/more.md", session="agent-3", wait=True)
        assert out.wait
        assert code(lambda: c.open_scope(session="agent-3", timeout=0.5))[0] == (
            grpc.StatusCode.FAILED_PRECONDITION
        )


def test_a_hold_survives_a_daemon_restart(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s, _ = held_scope(d, c)
        token = c.tokens[s]
    d.stop()
    d = start_daemon(policy=RULES)
    with client(d) as c:
        assert code(lambda: c.open_scope(session="agent-1", timeout=0.5))[0] == (
            grpc.StatusCode.FAILED_PRECONDITION
        )
        assert code(lambda: c.commit(s, token=token))[0] == grpc.StatusCode.PERMISSION_DENIED
        assert c.discard(s, token=token).status == pb.OUTCOME_STATUS_DISCARDED
        c.open_scope(session="agent-1", timeout=2)


def test_the_unscoped_scope_is_held_like_any(start_daemon):
    d = start_daemon(unscoped="implicit", policy=RULES)
    (d.mount / "unscoped" / "src").mkdir()
    (d.mount / "unscoped" / "src" / "u.py").write_text("u\n")
    with client(d) as c:
        assert list(c.settle_unscoped().review.tiers) == [pb.TIER_LLM]
        assert c.commit("unscoped", token="").status == pb.OUTCOME_STATUS_HELD
        assert c.discard("unscoped", token="").status == pb.OUTCOME_STATUS_DISCARDED
    assert not (d.project / "src").exists()


def test_without_review_rules_a_session_never_waits(daemon):
    with client(daemon) as c:
        s = c.open_scope(session="agent-1").scope_id
        (daemon.mount / s / "a.txt").write_text("a\n")
        c.close_scope(s)
        assert c.commit(s, wait=True).status == pb.OUTCOME_STATUS_COMMITTED
        c.open_scope(session="agent-1", timeout=2)
