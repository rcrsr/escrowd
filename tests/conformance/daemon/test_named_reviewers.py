"""Named reviewers: a policy declares reviewers, a change set needs the union of the
ones its paths' rules name, and a reviewer answers on the review socket by name.

A phase-3 client (tiers only) keeps working: the built-in `llm` and `human` reviewers
still show as `tiers`, beside `reviewers`.
"""

import os
import subprocess

import grpc
import pytest

from escrow.v1 import escrow_pb2 as pb
from escrow.v1 import escrow_pb2_grpc
from escrow_testkit import Daemon

LLM, HUMAN = pb.TIER_LLM, pb.TIER_HUMAN
COMMIT, RETURN = pb.VERDICT_COMMIT, pb.VERDICT_RETURN

SECURITY = """\
reviewers:
  security: {can_override: true}
review:
  - {paths: ['src/**'], reviewers: [security]}
"""

TIERS = """\
review:
  - {paths: ['src/auth/**'], tier: human}
  - {paths: ['src/**'], tier: llm}
"""

UNION = """\
reviewers:
  a: {}
  b: {}
review:
  - {paths: ['x/**'], reviewers: [b, a]}
  - {paths: ['y/**'], tier: human}
"""


def held_scope(d, c, *paths):
    """A scope that wrote each of `paths`, closed and committed: held."""
    s = c.open_scope(name="t").scope_id
    for path in paths:
        f = d.mount / s / path
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_text("change\n")
    c.close_scope(s)
    assert c.commit(s).status == pb.OUTCOME_STATUS_HELD
    return s


def review_as(rv, scope, reviewer="", tier=0, verdict=COMMIT, **kw):
    """The raw Review call: `reviewer` and `tier` as the request carries them."""
    stub = escrow_pb2_grpc.ReviewerServiceStub(rv._channel)
    req = pb.ReviewRequest(scope_id=scope, reviewer=reviewer, tier=tier, verdict=verdict, **kw)
    return stub.Review(req, timeout=10).outcome


def code(fn):
    with pytest.raises(grpc.RpcError) as err:
        fn()
    return err.value.code()


def ledger_ops(d, scope):
    """The scope's hold, review and decide lines, without timestamp and scope."""
    ops = (" op=hold ", " op=review-", " op=decide ")
    return [
        " ".join(x.split()[2:])
        for x in d.ledger
        if f" scope={scope} " in x and any(op in x for op in ops)
    ]


def test_a_declared_socket_reviewer_holds_a_scope_and_commits_it_by_name(start_daemon):
    d = start_daemon(policy=SECURITY)
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c, "src/util.py")
        (held,) = rv.list_held()
        assert (list(held.reviewers), list(held.tiers)) == (["security"], [])
        out = review_as(rv, s, reviewer="security")
        assert out.status == pb.OUTCOME_STATUS_COMMITTED
        assert (d.project / "src/util.py").read_text() == "change\n"
    assert ledger_ops(d, s)[1:] == [
        "op=review-security path= decision=commit",
        "op=decide path= decision=commit",
    ]


def test_a_reviewer_the_scope_does_not_wait_for_is_a_state_error(start_daemon):
    d = start_daemon(policy=SECURITY)
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c, "src/util.py")
        assert code(lambda: review_as(rv, s, reviewer="llm")) == grpc.StatusCode.FAILED_PRECONDITION
        assert (
            code(lambda: review_as(rv, s, reviewer="ghost")) == grpc.StatusCode.FAILED_PRECONDITION
        )
        assert [h.scope_id for h in rv.list_held()] == [s]


def test_tier_rules_show_as_reviewers_and_as_tiers(start_daemon):
    d = start_daemon(policy=TIERS)
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c, "src/auth/login.py")
        (held,) = rv.list_held()
        assert list(held.reviewers) == ["llm", "human"]
        assert list(held.tiers) == [LLM, HUMAN]
        # A phase-3 client names the tier only.
        out = rv.review(s, LLM, COMMIT, reasons=["fine"])
        assert out.status == pb.OUTCOME_STATUS_HELD
        assert (list(out.reviewers), list(out.tiers)) == (["human"], [HUMAN])
        (done,) = rv.get_held(s).held.reviews
        assert (done.reviewer, done.tier, done.verdict) == ("llm", LLM, COMMIT)
        out = rv.review(s, HUMAN, COMMIT)
        assert out.status == pb.OUTCOME_STATUS_COMMITTED


def test_the_reviewers_of_a_change_set_are_the_union_in_declaration_order(start_daemon):
    d = start_daemon(policy=UNION)
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c, "y/1", "x/1")
        (held,) = rv.list_held()
        assert list(held.reviewers) == ["a", "b", "llm", "human"]
        assert list(held.tiers) == [LLM, HUMAN]
        # b cannot go before a.
        assert code(lambda: review_as(rv, s, reviewer="b")) == grpc.StatusCode.FAILED_PRECONDITION
        out = review_as(rv, s, reviewer="a", verdict=RETURN, reasons=["no"])
        assert list(out.reviewers) == ["b", "llm", "human"]
        # Only a reviewer that may override answers for those before it: the human does.
        out = review_as(rv, s, reviewer="human", verdict=RETURN)
        assert out.status == pb.OUTCOME_STATUS_RETURNED
        assert list(out.reasons) == ["a: no"]


def test_a_tier_that_disagrees_with_the_reviewer_or_names_none_is_invalid(start_daemon):
    d = start_daemon(policy=TIERS)
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c, "src/auth/login.py")
        invalid = grpc.StatusCode.INVALID_ARGUMENT
        assert code(lambda: review_as(rv, s, reviewer="llm", tier=HUMAN)) == invalid
        assert code(lambda: review_as(rv, s, reviewer="Not Valid")) == invalid
        assert code(lambda: review_as(rv, s, tier=pb.TIER_SOFTWARE)) == invalid
        assert code(lambda: review_as(rv, s)) == invalid
        assert [h.scope_id for h in rv.list_held()] == [s]
        # An agreeing tier is fine.
        assert review_as(rv, s, reviewer="llm", tier=LLM).status == pb.OUTCOME_STATUS_HELD


@pytest.mark.parametrize(
    "policy, message",
    [
        ("reviewers: {Bad: {}}\n", "reviewers.Bad: invalid name"),
        ("reviewers: {llm: {}}\n", "llm is built in"),
        ("reviewers: {a: {delegate: true}}\n", "reviewers.a: delegate needs run"),
        ("reviewers: {a: {run: []}}\n", "reviewers.a: run is empty"),
        ("reviewers: {a: {run: [/no/such/program]}}\n", "reviewers.a: /no/such/program"),
        ("review: [{paths: [a]}]\n", "review[0]: give tier or reviewers"),
        ("review: [{paths: [a], reviewers: [ghost]}]\n", "review[0]: unknown reviewer ghost"),
        ("reviewers_parallel: 0\n", "reviewers_parallel"),
    ],
)
def test_a_bad_reviewer_policy_refuses_to_start(escrow_bin, runtime_dir, policy, message):
    with pytest.raises(RuntimeError, match=message.replace("[", r"\[").replace("]", r"\]")):
        Daemon(escrow_bin, runtime_dir, policy=policy)


def escrow(escrow_bin, *args, env=None):
    return subprocess.run([escrow_bin, *args], capture_output=True, text=True, env=env)


def test_escrow_review_lists_reviewers_and_commits_by_name_with_as(start_daemon, escrow_bin):
    d = start_daemon(policy=SECURITY)
    with d.client() as c:
        s = held_scope(d, c, "src/util.py")
        listed = escrow(escrow_bin, "review", "--socket", d.socket, "list")
        (line,) = listed.stdout.splitlines()
        assert " reviewers=security name=t" in line and " held=" in line
        shown = escrow(escrow_bin, "review", "--socket", d.socket, "show", s)
        assert shown.returncode == 0 and " reviewers=security " in shown.stdout.splitlines()[0]
        done = escrow(escrow_bin, "review", "--socket", d.socket, "commit", s, "--as", "security")
        assert done.returncode == 0, done.stderr
        assert done.stdout == f"{s} committed\n  src\n  src/util.py\n"
    assert (d.project / "src/util.py").read_text() == "change\n"


def test_escrow_review_prints_reviewer_names_when_a_scope_stays_held(start_daemon, escrow_bin):
    d = start_daemon(policy=UNION)
    with d.client() as c:
        s = held_scope(d, c, "x/a.txt")
        out = escrow(escrow_bin, "review", "--socket", d.socket, "commit", s, "--as", "a")
        assert out.returncode == 0, out.stderr
        assert out.stdout == f"{s} held for b\n"
        shown = escrow(escrow_bin, "review", "--socket", d.socket, "show", s).stdout
        assert "review a: commit\n" in shown


def test_escrow_review_as_conflicts_with_tier(start_daemon, escrow_bin):
    d = start_daemon(policy=SECURITY)
    with d.client() as c:
        s = held_scope(d, c, "src/util.py")
        r = escrow(
            escrow_bin,
            "review",
            "--socket",
            d.socket,
            "commit",
            s,
            "--as",
            "security",
            "--tier",
            "llm",
        )
        assert r.returncode == 2 and "cannot be used with" in r.stderr
        assert [h.scope_id for h in d.reviewer().list_held()] == [s]


def test_escrow_review_socket_wins_over_socket_and_project(start_daemon, escrow_bin, runtime_dir):
    d = start_daemon(policy=SECURITY)
    wrong = str(runtime_dir / "wrong.sock")
    with d.client() as c:
        s = held_scope(d, c, "src/util.py")
        (line,) = escrow(
            escrow_bin, "review", "--socket", wrong, "--review-socket", f"{d.socket}.review", "list"
        ).stdout.splitlines()
        assert line.startswith(f"{s} ")
        env = {**os.environ, "ESCROW_REVIEW_SOCKET": f"{d.socket}.review"}
        env.pop("ESCROW_SOCKET", None)
        r = escrow(escrow_bin, "review", "commit", s, "--as", "security", env=env)
        assert r.returncode == 0, r.stderr
        assert r.stdout.startswith(f"{s} committed")


def test_escrow_review_never_echoes_the_token_from_its_environment(escrow_bin):
    env = {**os.environ, "ESCROW_REVIEW_TOKEN": "s3cret-token-value"}
    for args in (
        ["review", "--help"],
        ["review", "commit", "--help"],
        ["review", "discard", "--help"],
    ):
        r = escrow(escrow_bin, *args, env=env)
        assert r.returncode == 0
        assert "s3cret-token-value" not in r.stdout + r.stderr
