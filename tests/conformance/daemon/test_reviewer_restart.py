"""Reviews across a daemon restart. A held scope keeps its reviewers and its run; the
daemon picks the review up when it starts, before it serves: a command reviewer whose run
was cut off runs again (one attempt up), a delegate's token still works and its deadline
still counts, and a reviewer the policy no longer has fails.
"""

import time

from escrow.v1 import escrow_pb2 as pb
from escrow_testkit import write_policy
from escrow_testkit.reviewers import stand_in

COMMIT = pb.VERDICT_COMMIT
COMMITTED, RETURNED = pb.OUTCOME_STATUS_COMMITTED, pb.OUTCOME_STATUS_RETURNED
DISCARDED = pb.OUTCOME_STATUS_DISCARDED
PENDING = '{"verdict": "pending"}'


def policy(program, name="lint", options=""):
    entries = ", ".join(filter(None, [f"run: ['{program.path}']", options]))
    rule = f"  - {{paths: ['src/**'], reviewers: [{name}]}}\n"
    return f"reviewers:\n  {name}: {{{entries}}}\nreview:\n{rule}"


def held_scope(d, c):
    s = c.open_scope(name="t").scope_id
    f = d.mount / s / "src/util.py"
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text("change\n")
    c.close_scope(s)
    assert c.commit(s).status == pb.OUTCOME_STATUS_HELD
    return s


def eventually(fn, timeout=10):
    deadline = time.monotonic() + timeout
    while not (value := fn()):
        assert time.monotonic() < deadline, "condition never held"
        time.sleep(0.02)
    return value


def decided(c, scope):
    return list(c.await_decision(scope, timeout=30))[-1]


def lines(d, scope, op):
    return [x for x in d.ledger if f" scope={scope} " in x and f" op={op} " in x]


def test_a_run_cut_off_by_a_kill_runs_again_as_attempt_two_and_decides_once(
    start_daemon, runtime_dir
):
    lint = stand_in(runtime_dir, "lint", sleep=1.5)
    d = start_daemon(policy=policy(lint))
    with d.client() as c:
        s = held_scope(d, c)
    (first,) = lint.wait_for_runs(1)
    assert first.attempt == 1

    d.restart(kill=True)

    with d.client() as c:
        out = decided(c, s)
    runs = lint.wait_for_runs(2)
    assert [r.attempt for r in runs[:2]] == [1, 2]
    assert out.status == COMMITTED
    assert (d.project / "src/util.py").read_text() == "change\n"
    assert len(lines(d, s, "decide")) == 1


def test_a_delegates_token_still_commits_after_a_kill_without_another_run(
    start_daemon, runtime_dir
):
    oncall = stand_in(runtime_dir, "oncall", stdout=PENDING)
    d = start_daemon(policy=policy(oncall, "oncall", "delegate: true, on_failure: return"))
    with d.client() as c:
        s = held_scope(d, c)
    eventually(lambda: any("decision=pending" in x for x in lines(d, s, "review-oncall")))
    (run,) = oncall.runs()
    token = run.env["ESCROW_REVIEW_TOKEN"]

    d.restart(kill=True)

    with d.reviewer() as rv:
        out = rv.review(s, pb.TIER_UNSPECIFIED, COMMIT, reviewer="oncall", token=token)
    assert out.status == COMMITTED
    assert len(oncall.runs()) == 1
    assert (d.project / "src/util.py").read_text() == "change\n"
    assert len(lines(d, s, "decide")) == 1


def test_a_delegates_deadline_that_passed_while_the_daemon_was_down_fails_at_start(
    start_daemon, runtime_dir
):
    oncall = stand_in(runtime_dir, "oncall", stdout=PENDING)
    options = "delegate: true, timeout: 1s, on_failure: return"
    d = start_daemon(policy=policy(oncall, "oncall", options))
    with d.client() as c:
        s = held_scope(d, c)
    eventually(lambda: any("decision=pending" in x for x in lines(d, s, "review-oncall")))
    d.kill()
    time.sleep(1.3)

    d.restart()

    with d.client() as c:
        out = decided(c, s)
    assert out.status == RETURNED
    assert "oncall: failed: no verdict within 1s" in list(out.reasons)
    assert len(oncall.runs()) == 1
    assert len(lines(d, s, "decide")) == 1


def test_a_reviewer_removed_from_the_policy_fails_at_start(start_daemon, runtime_dir):
    lint = stand_in(runtime_dir, "lint", sleep=1.5)
    d = start_daemon(policy=policy(lint))
    with d.client() as c:
        s = held_scope(d, c)
    lint.wait_for_runs(1)
    d.kill()
    other = "reviewers:\n  other: {}\nreview:\n  - {paths: ['src/**'], reviewers: [other]}\n"
    write_policy(runtime_dir, extra=other)

    d.restart()

    with d.client() as c:
        out = decided(c, s)
    assert out.status == DISCARDED
    assert "lint: failed: not in the policy" in list(out.reasons)
    assert not (d.project / "src/util.py").exists()
    (denied,) = lines(d, s, "review-lint")
    assert " cause=not" in denied and denied.endswith(" decision=discard"), denied
    assert len(lint.runs()) == 1
    assert len(lines(d, s, "decide")) == 1
