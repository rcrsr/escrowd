"""Phase 3.1: the policy's review rules and the software tier's write rules.

At close the daemon runs `write:` over the change set: a path under `write.deny`
(created, changed, deleted or the source of a rename) or a written file holding a
`write.deny_content` string discards it, and a commit or a return then turns into a
discard (verdicts only tighten). `review:` gives each path a tier; the change set
needs every tier from llm up to the highest its paths need, and until held decisions
the daemon refuses to commit one that needs any.
"""

import grpc
import pytest
from test_sdk import Sdk

import escrow
from escrow.v1 import escrow_pb2 as pb

RULES = """\
review:
  - {paths: ['src/auth/**'], tier: human}
  - {paths: ['src/**', 'docs/**'], tier: llm, wait: optional}
write:
  deny: ['*.pem', 'secrets/**']
  deny_content: ['BEGIN PRIVATE KEY']
"""


def client(d):
    return escrow.connect(str(d.socket))


def decision_lines(d, scope):
    """The scope's write-rule and decide lines, without the timestamp and scope."""
    ops = (" op=write-deny ", " op=write-content ", " op=decide ")
    return [
        " ".join(line.split()[2:])
        for line in d.ledger
        if f" scope={scope} " in line and any(op in line for op in ops)
    ]


def test_without_rules_nothing_needs_review(daemon):
    with client(daemon) as c:
        s = c.open_scope().scope_id
        (daemon.mount / s / "a.txt").write_text("a\n")
        r = c.close_scope(s).review
        assert (r.verdict, list(r.tiers), r.wait_required) == (pb.VERDICT_COMMIT, [], False)
        assert c.commit(s).status == pb.OUTCOME_STATUS_COMMITTED


def test_a_denied_path_discards_the_commit(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s = c.open_scope().scope_id
        (d.mount / s / "keys").mkdir()
        (d.mount / s / "keys" / "a.pem").write_text("k\n")
        (d.mount / s / "ok.txt").write_text("ok\n")
        r = c.close_scope(s).review
        assert r.verdict == pb.VERDICT_DISCARD
        assert list(r.reasons) == ["write.deny: keys/a.pem"]
        assert list(r.tiers) == []  # discarded at once: no tier above software reviews it
        out = c.commit(s)
        assert out.status == pb.OUTCOME_STATUS_DISCARDED
        assert list(out.reasons) == ["write.deny: keys/a.pem"]
    assert not (d.project / "ok.txt").exists()
    assert decision_lines(d, s) == [
        "op=write-deny path=keys/a.pem decision=deny",
        "op=decide path= decision=discard",
    ]


def test_deny_covers_deletes_and_rename_sources(start_daemon):
    d = start_daemon(policy=RULES)
    (d.project / "secrets").mkdir()
    (d.project / "secrets" / "token").write_text("t\n")
    (d.project / "old.pem").write_text("p\n")
    with client(d) as c:
        a, b = c.open_scope().scope_id, c.open_scope().scope_id
        (d.mount / a / "secrets" / "token").unlink()
        (d.mount / b / "old.pem").rename(d.mount / b / "new.txt")
        assert list(c.close_scope(a).review.reasons) == ["write.deny: secrets/token"]
        assert list(c.close_scope(b).review.reasons) == ["write.deny: old.pem"]
        assert c.commit(a).status == pb.OUTCOME_STATUS_DISCARDED
        assert c.commit(b).status == pb.OUTCOME_STATUS_DISCARDED
    assert (d.project / "secrets" / "token").exists() and (d.project / "old.pem").exists()


def test_denied_content_turns_a_return_into_a_discard(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s = c.open_scope().scope_id
        # The string straddles the daemon's 64 KiB read chunks.
        data = b"x" * (64 * 1024 - 5) + b"-----BEGIN PRIVATE KEY-----\n"
        (d.mount / s / "notes.bin").write_bytes(data)
        r = c.close_scope(s).review
        assert list(r.reasons) == ['write.deny_content: notes.bin contains "BEGIN PRIVATE KEY"']
        out = c.decide(s, pb.VERDICT_RETURN, reasons=["try again"])
        assert (out.status, out.reopened) == (pb.OUTCOME_STATUS_DISCARDED, False)
        with pytest.raises(grpc.RpcError) as err:
            c.close_scope(s)
    assert err.value.code() == grpc.StatusCode.NOT_FOUND
    assert "op=write-content path=notes.bin decision=deny" in decision_lines(d, s)


def test_tiers_go_up_to_the_highest_path_needs(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        docs, auth, plain = (c.open_scope().scope_id for _ in range(3))
        (d.mount / docs / "docs").mkdir()
        (d.mount / docs / "docs" / "guide.md").write_text("g\n")
        (d.mount / auth / "src" / "auth").mkdir(parents=True)
        (d.mount / auth / "src" / "auth" / "login.py").write_text("l\n")
        (d.mount / plain / "README.md").write_text("r\n")
        r = c.close_scope(docs).review
        assert (list(r.tiers), r.wait_required) == ([pb.TIER_LLM], False)
        r = c.close_scope(auth).review
        assert (list(r.tiers), r.wait_required) == ([pb.TIER_LLM, pb.TIER_HUMAN], True)
        assert list(c.close_scope(plain).review.tiers) == []
        assert c.commit(plain).status == pb.OUTCOME_STATUS_COMMITTED


def test_the_opener_cannot_commit_a_change_set_that_needs_review(start_daemon):
    d = start_daemon(policy=RULES)
    with client(d) as c:
        s = c.open_scope().scope_id
        (d.mount / s / "src").mkdir()
        (d.mount / s / "src" / "util.py").write_text("u\n")
        c.close_scope(s)
        with pytest.raises(grpc.RpcError) as err:
            c.commit(s)
        assert err.value.code() == grpc.StatusCode.FAILED_PRECONDITION
        assert "needs review by llm;" in err.value.details()
        # Still closed and undecided: a return or a discard decides it.
        assert c.get_change_set(s).changes[0].path == "src"
        out = c.decide(s, pb.VERDICT_RETURN, reasons=["split it"])
        assert (out.status, out.reopened) == (pb.OUTCOME_STATUS_RETURNED, True)
        c.close_scope(s)
        assert c.discard(s).status == pb.OUTCOME_STATUS_DISCARDED
    assert not (d.project / "src").exists()


def test_settling_the_unscoped_scope_runs_the_rules(start_daemon):
    d = start_daemon(unscoped="implicit", policy=RULES)
    (d.mount / "unscoped" / "id.pem").write_text("k\n")
    with client(d) as c:
        r = c.settle_unscoped().review
        assert list(r.reasons) == ["write.deny: id.pem"]
        assert c.commit("unscoped", token="").status == pb.OUTCOME_STATUS_DISCARDED
    assert not (d.project / "id.pem").exists()


def test_the_sdk_sees_the_review_and_the_tightened_outcome(escrow_bin, runtime_dir):
    sdk = Sdk(escrow_bin, runtime_dir, policy=RULES)
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
