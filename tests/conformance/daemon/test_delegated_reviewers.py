"""Delegated reviewers: a command reviewer with `delegate: true` may answer `pending`. Its
verdict then comes later, from whoever holds the run's one-time token, on the review
socket. The token is valid from the run's start until a verdict is applied with it,
a withdrawal or newer run replaces it, or the reviewer's timeout after `pending` passes
(then the reviewer's `on_failure` verdict applies).

The token is only ever hashed: it must not appear in the ledger, the daemon log or any
file under the state directory.
"""

import hashlib
import os
import sqlite3
import subprocess
import time
from functools import partial

import grpc
import pytest

from escrow import Reviewer
from escrow.v1 import escrow_pb2 as pb
from escrow_testkit.reviewers import stand_in

COMMIT, DISCARD = pb.VERDICT_COMMIT, pb.VERDICT_DISCARD
COMMITTED, RETURNED, HELD = (
    pb.OUTCOME_STATUS_COMMITTED,
    pb.OUTCOME_STATUS_RETURNED,
    pb.OUTCOME_STATUS_HELD,
)
PENDING = '{"verdict": "pending"}'
DENIED = grpc.StatusCode.PERMISSION_DENIED


def delegate(work, name="oncall", **kw):
    """A delegated reviewer stand-in: it answers `pending` (after `sleep` seconds)."""
    return stand_in(work, name, stdout=PENDING, **kw)


def policy(reviewers, rules, extra=""):
    """Policy YAML: `reviewers` is `(name, stand-in, options)`; `rules` is `(paths, names)`."""
    declared = "".join(
        f"  {name}: {{{', '.join(filter(None, [f'run: [{str(program.path)!r}]', options]))}}}\n"
        for name, program, options in reviewers
    )
    review = "".join(
        f"  - {{paths: ['{paths}'], reviewers: [{names}]}}\n" for paths, names in rules
    )
    return f"reviewers:\n{declared}review:\n{review}{extra}"


def one_delegate(program, options="delegate: true, on_failure: return", names="oncall"):
    return policy([("oncall", program, options)], [("src/**", names)])


def held_scope(d, c, path="src/util.py"):
    s = c.open_scope(name="t").scope_id
    f = d.mount / s / path
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text("change\n")
    c.close_scope(s)
    assert c.commit(s).status == HELD
    return s


def eventually(fn, timeout=10):
    deadline = time.monotonic() + timeout
    while not (value := fn()):
        assert time.monotonic() < deadline, "condition never held"
        time.sleep(0.02)
    return value


def lines(d, scope, op):
    return [x for x in d.ledger if f" scope={scope} " in x and f" op={op} " in x]


def decided(c, scope):
    return list(c.await_decision(scope, timeout=30))[-1]


def run_of(program, scope):
    """The run of stand-in `program` on `scope` once it has started."""
    return eventually(
        lambda: next((r for r in program.runs() if r.stdin["scope"]["id"] == scope), None)
    )


def token_of(program, scope):
    """The token the run of `program` on `scope` was started with."""
    return run_of(program, scope).env["ESCROW_REVIEW_TOKEN"]


def waiting(d, scope):
    """Wait until the delegate's `pending` is ledgered; the run then waits for its token."""
    eventually(lambda: any("decision=pending" in x for x in lines(d, scope, "review-oncall")))


def give(rv, scope, token, reviewer="oncall", verdict=COMMIT, **kw):
    return rv.review(scope, pb.TIER_UNSPECIFIED, verdict, reviewer=reviewer, token=token, **kw)


def code_of(fn):
    with pytest.raises(grpc.RpcError) as err:
        fn()
    return err.value.code()


def sha256(token):
    return hashlib.sha256(token.encode()).hexdigest()


def denial_lines(d, scope, reviewer="oncall"):
    return [x for x in lines(d, scope, f"review-{reviewer}") if "cause=token decision=deny" in x]


def test_a_pending_delegate_is_ledgered_and_its_token_commits_the_scope(start_daemon, runtime_dir):
    oncall = delegate(runtime_dir)
    d = start_daemon(policy=one_delegate(oncall))
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c)
        waiting(d, s)
        (run,) = oncall.runs()
        token = run.env["ESCROW_REVIEW_TOKEN"]
        assert run.env["ESCROW_REVIEW_SOCKET"] == f"{d.socket}.review"
        assert run.stdin["attempt"] == 1

        (held,) = rv.list_held()
        assert (held.scope_id, list(held.reviewers)) == (s, ["oncall"])
        out = give(rv, s, token, reasons=["looks fine"])

        assert out.status == COMMITTED
    (pending,) = [x for x in lines(d, s, "review-oncall") if "decision=pending" in x]
    assert f" pid={run.pid} " in pending
    assert any(x.endswith(" path= decision=commit") for x in lines(d, s, "review-oncall")), lines(
        d, s, "review-oncall"
    )
    assert (d.project / "src/util.py").read_text() == "change\n"
    assert len(lines(d, s, "decide")) == 1


def test_a_token_is_used_up_by_its_verdict_and_never_stored_in_the_clear(start_daemon, runtime_dir):
    oncall = delegate(runtime_dir)
    d = start_daemon(
        policy=policy([("oncall", oncall, "delegate: true")], [("src/**", "oncall, human")])
    )
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c)
        waiting(d, s)
        token = token_of(oncall, s)
        db = sqlite3.connect(f"file:{d.state / 'scopes' / s / 'meta.sqlite'}?mode=ro", uri=True)
        assert db.execute("SELECT reviewer, state, token_sha256 FROM review_run").fetchall() == [
            ("oncall", "waiting", sha256(token))
        ]
        db.close()

        out = give(rv, s, token)

        assert (out.status, list(out.reviewers)) == (HELD, ["human"])
        assert code_of(lambda: give(rv, s, token)) == DENIED
        assert len(denial_lines(d, s)) == 1
        assert c.discard(s).status == pb.OUTCOME_STATUS_DISCARDED

    for path in [d.work / "daemon.log", *d.state.rglob("*")]:
        if path.is_file():
            assert token.encode() not in path.read_bytes(), path


def test_a_wrong_missing_or_foreign_token_is_denied_and_ledgered(start_daemon, runtime_dir):
    oncall, ops = delegate(runtime_dir), delegate(runtime_dir, "ops")
    lint = stand_in(runtime_dir, "lint", sleep=60)
    d = start_daemon(
        policy=policy(
            [
                ("oncall", oncall, "delegate: true"),
                ("ops", ops, "delegate: true"),
                ("lint", lint, ""),
            ],
            [("a/**", "oncall"), ("b/**", "ops"), ("c/**", "oncall"), ("d/**", "lint")],
        )
    )
    with d.client() as c, d.reviewer() as rv:
        a, b, other, linted = (held_scope(d, c, f"{x}/f.txt") for x in "abcd")
        for s in (a, other):
            eventually(
                lambda s=s: any("decision=pending" in x for x in lines(d, s, "review-oncall"))
            )
        eventually(lambda: any("decision=pending" in x for x in lines(d, b, "review-ops")))
        mine = token_of(oncall, a)
        refused = [
            ("wrong token", a, "oncall", "0" * 64),
            ("no token", a, "oncall", ""),
            ("another scope's token", a, "oncall", token_of(oncall, other)),
            ("another reviewer's token", a, "oncall", token_of(ops, b)),
        ]
        for what, scope, reviewer, token in refused:
            assert code_of(partial(give, rv, scope, token, reviewer)) == DENIED, what
        # A command reviewer is not answered on the socket, with or without a token.
        assert code_of(lambda: give(rv, linted, "", "lint")) == DENIED
        assert code_of(lambda: give(rv, linted, mine, "lint")) == DENIED

        assert len(denial_lines(d, a)) == len(refused)
        assert len(denial_lines(d, linted, "lint")) == 2
        # Refusals do not use the token up.
        assert give(rv, a, mine).status == COMMITTED
        assert code_of(lambda: give(rv, other, "", "oncall")) == DENIED
        assert c.discard(other).status == pb.OUTCOME_STATUS_DISCARDED
        assert c.discard(b).status == pb.OUTCOME_STATUS_DISCARDED
        assert c.discard(linted).status == pb.OUTCOME_STATUS_DISCARDED


def test_a_token_verdict_while_the_run_still_goes_ends_the_run_and_the_run_counts_for_nothing(
    start_daemon, runtime_dir
):
    oncall = delegate(runtime_dir, sleep=60)
    d = start_daemon(policy=one_delegate(oncall))
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c)
        run = run_of(oncall, s)

        out = give(rv, s, run.env["ESCROW_REVIEW_TOKEN"], verdict=COMMIT)

        assert out.status == COMMITTED
        eventually(lambda: run.terminated)
    assert len(lines(d, s, "review-oncall")) == 1
    assert not any("decision=pending" in x for x in lines(d, s, "review-oncall"))
    assert len(lines(d, s, "decide")) == 1


def test_a_commit_that_is_rolled_back_does_not_use_up_the_token(start_daemon, runtime_dir):
    oncall = delegate(runtime_dir)
    d = start_daemon(policy=one_delegate(oncall), env={"ESCROWD_FAULT": "journal:0"})
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c)
        waiting(d, s)
        token = token_of(oncall, s)

        first = code_of(lambda: give(rv, s, token))
        if first != grpc.StatusCode.ABORTED:
            pytest.skip("this daemon build ignores ESCROWD_FAULT")
        # Still the token's: the retry gets as far as the commit again, not a denial.
        assert code_of(lambda: give(rv, s, token)) == grpc.StatusCode.ABORTED
        assert [h.scope_id for h in rv.list_held()] == [s]
        assert denial_lines(d, s) == []


def test_no_verdict_within_the_timeout_gives_the_on_failure_verdict(start_daemon, runtime_dir):
    oncall = delegate(runtime_dir)
    d = start_daemon(
        policy=one_delegate(oncall, "delegate: true, timeout: 500ms, on_failure: return")
    )
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c)
        waiting(d, s)
        token = token_of(oncall, s)

        out = decided(c, s)

        assert out.status == RETURNED
        assert list(out.reasons) == ["oncall: failed: no verdict within 500ms"]
        assert code_of(lambda: give(rv, s, token)) == DENIED
    review = lines(d, s, "review-oncall")
    assert any("decision=pending" in x for x in review)
    assert any(" cause=no%20verdict%20within%20500ms decision=return" in x for x in review), review
    assert len(lines(d, s, "decide")) == 1


def test_a_verdict_before_the_timeout_leaves_nothing_for_the_timer_to_apply(
    start_daemon, runtime_dir
):
    oncall = delegate(runtime_dir)
    d = start_daemon(
        policy=one_delegate(oncall, "delegate: true, timeout: 1s, on_failure: discard")
    )
    with d.client() as c, d.reviewer() as rv:
        s = held_scope(d, c)
        waiting(d, s)

        assert give(rv, s, token_of(oncall, s)).status == COMMITTED
        time.sleep(1.5)

    assert len(lines(d, s, "review-oncall")) == 2  # pending, commit
    assert len(lines(d, s, "decide")) == 1
    assert (d.project / "src/util.py").exists()


def test_escrow_review_gives_the_verdict_with_the_token_and_socket_from_the_environment(
    start_daemon, runtime_dir, escrow_bin
):
    oncall = delegate(runtime_dir)
    d = start_daemon(policy=one_delegate(oncall))
    with d.client() as c:
        s = held_scope(d, c)
        waiting(d, s)
        token = token_of(oncall, s)
        env = {
            **os.environ,
            "ESCROW_REVIEW_SOCKET": f"{d.socket}.review",
            "ESCROW_REVIEW_TOKEN": token,
        }
        env.pop("ESCROW_SOCKET", None)

        done = subprocess.run(
            [escrow_bin, "review", "commit", s, "--as", "oncall"],
            capture_output=True,
            text=True,
            env=env,
        )

        assert done.returncode == 0, done.stderr
        assert done.stdout.startswith(f"{s} committed")
        assert token not in done.stdout + done.stderr
    assert (d.project / "src/util.py").read_text() == "change\n"


def test_the_sdk_reviewer_gives_a_delegates_verdict_with_review_as_and_a_token(
    start_daemon, runtime_dir
):
    oncall = delegate(runtime_dir)
    d = start_daemon(policy=one_delegate(oncall))
    with d.client() as c, Reviewer(f"{d.socket}.review") as rv:
        s = held_scope(d, c)
        waiting(d, s)
        token = token_of(oncall, s)

        with pytest.raises(grpc.RpcError) as err:
            rv.review_as(s, "oncall", "commit", token="0" * 64)
        assert err.value.code() == DENIED
        result = rv.review_as(s, "oncall", "commit", reasons=["ok"], token=token)

        assert (result.scope_id, result.status) == (s, "committed")
    assert (d.project / "src/util.py").exists()
