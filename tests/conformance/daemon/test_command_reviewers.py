"""Command reviewers: a policy reviewer with `run:` is a program the daemon starts when
its turn comes. It reads the request on stdin, answers one JSON verdict, and the daemon
gives that verdict as the reviewer's. A failed run gives the reviewer's `on_failure`.

No test here answers on the review socket for the command reviewer: the daemon applies
the program's answer by itself.
"""

import os
import time

import pytest
from google.protobuf import json_format

from escrow.v1 import escrow_pb2 as pb
from escrow_testkit.reviewers import stand_in

COMMITTED, RETURNED = pb.OUTCOME_STATUS_COMMITTED, pb.OUTCOME_STATUS_RETURNED
DISCARDED = pb.OUTCOME_STATUS_DISCARDED


def policy(*reviewers, rules="reviewers: [lint]", extra="", paths="src/**"):
    """Policy YAML declaring each `(name, program, options)` of `reviewers`, and one rule
    for `src/**`."""
    declared = ""
    for name, program, options in reviewers:
        entries = ", ".join(filter(None, [f"run: ['{program.path}']", options]))
        declared += f"  {name}: {{{entries}}}\n"
    return f"reviewers:\n{declared}review:\n  - {{paths: ['{paths}'], {rules}}}\n{extra}"


def write_and_close(d, c, path="src/util.py", session=""):
    s = c.open_scope(name="t", session=session).scope_id
    f = d.mount / s / path
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text("change\n")
    return s, c.close_scope(s)


def held_scope(d, c, **kw):
    """A scope that wrote under src/, closed and committed: held. Returns it with the
    change set its close returned."""
    s, change_set = write_and_close(d, c, **kw)
    assert c.commit(s).status == pb.OUTCOME_STATUS_HELD
    return s, change_set


def decided(c, scope):
    """The scope's outcome once its reviewers are through."""
    return list(c.await_decision(scope, timeout=30))[-1]


def eventually(fn, timeout=10):
    deadline = time.monotonic() + timeout
    while not (value := fn()):
        assert time.monotonic() < deadline, "condition never held"
        time.sleep(0.02)
    return value


def lines(d, scope, op):
    return [x for x in d.ledger if f" scope={scope} " in x and f" op={op} " in x]


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def test_a_programs_return_with_a_reason_is_the_reviewers_verdict_and_pid_is_ledgered(
    start_daemon, runtime_dir
):
    lint = stand_in(runtime_dir, "lint", verdict="return", reasons=["bad style"])
    d = start_daemon(policy=policy(("lint", lint, "")))
    with d.client() as c:
        s, _ = held_scope(d, c)

        out = decided(c, s)

    assert out.status == RETURNED
    assert list(out.reasons) == ["lint: bad style"]
    (run,) = lint.runs()
    (line,) = lines(d, s, "review-lint")
    assert f"op=review-lint path= pid={run.pid} decision=return" in line
    assert not (d.project / "src").exists()


def test_the_program_reads_the_change_set_the_close_returned_and_the_sessions_history(
    start_daemon, runtime_dir
):
    lint = stand_in(runtime_dir, "lint", verdict="commit")
    d = start_daemon(policy=policy(("lint", lint, "")))
    with d.client() as c:
        before, _ = write_and_close(d, c, "docs/a.txt", session="agent")
        assert c.commit(before).status == COMMITTED
        s, closed = held_scope(d, c, session="agent")
        decided(c, s)

    (run,) = lint.runs()
    request = run.stdin
    assert request["contract"] == "escrow.review.v1"
    assert (request["scope"]["id"], request["scope"]["session"]) == (s, "agent")
    assert (request["reviewer"], request["attempt"]) == ("lint", 1)
    assert (request["verdict_so_far"], request["reviews"]) == ("commit", [])
    assert json_format.ParseDict(request["change_set"], pb.ChangeSet()) == closed
    history = [json_format.ParseDict(h, pb.Decided()) for h in request["session_history"]]
    assert [(h.scope_id, h.outcome.status) for h in history] == [(before, COMMITTED)]
    assert run.cwd.name == "review" and run.cwd.parent.name == s
    assert {"change_set.json", "diff.patch"} <= set(run.cwd_files)
    assert "src/util.py" in run.cwd_files["diff.patch"]
    assert "ESCROW_REVIEW_TOKEN" not in run.env


FAILURES = [
    ("exit 1", {"exit_code": 1}),
    ("signal 9", {"kill_self": True}),
    ("pending from a reviewer without delegate", {"stdout": '{"verdict": "pending"}'}),
    ("answer over 1 MiB", {"big": True}),
    ("bad answer: ", {"stdout": "not json"}),
    ("timeout after 500ms", {"sleep": 30}),
]


@pytest.mark.parametrize(("cause", "behavior"), FAILURES, ids=[c for c, _ in FAILURES])
def test_a_failed_run_gives_the_on_failure_verdict_with_the_cause(
    start_daemon, runtime_dir, cause, behavior
):
    lint = stand_in(runtime_dir, "lint", **behavior)
    options = "on_failure: return" + (", timeout: 500ms" if "sleep" in behavior else "")
    d = start_daemon(policy=policy(("lint", lint, options)))
    with d.client() as c:
        s, _ = held_scope(d, c)

        out = decided(c, s)

    assert out.status == RETURNED
    (reason,) = out.reasons
    assert reason.startswith(f"lint: failed: {cause}")
    (line,) = lines(d, s, "review-lint")
    assert "decision=return" in line and "cause=" in line


def test_a_program_that_cannot_start_fails_with_the_os_error(start_daemon, runtime_dir):
    lint = stand_in(runtime_dir, "lint")
    d = start_daemon(policy=policy(("lint", lint, "on_failure: return")))
    lint.path.unlink()
    with d.client() as c:
        s, _ = held_scope(d, c)

        out = decided(c, s)

    assert out.status == RETURNED
    assert out.reasons[0].startswith("lint: failed: cannot start: ")


@pytest.mark.parametrize(
    ("on_failure", "status", "files"),
    [("commit", COMMITTED, True), ("return", RETURNED, False), ("discard", DISCARDED, False)],
)
def test_on_failure_commit_applies_the_change_set_and_the_others_do_not(
    start_daemon, runtime_dir, on_failure, status, files
):
    lint = stand_in(runtime_dir, "lint", exit_code=1)
    d = start_daemon(policy=policy(("lint", lint, f"on_failure: {on_failure}")))
    with d.client() as c:
        s, _ = held_scope(d, c)

        out = decided(c, s)

    assert out.status == status
    assert (d.project / "src/util.py").exists() is files


def test_a_programs_discard_removes_the_change_set_from_the_project(start_daemon, runtime_dir):
    lint = stand_in(runtime_dir, "lint", verdict="discard", reasons=["secrets"])
    d = start_daemon(policy=policy(("lint", lint, "")))
    with d.client() as c:
        s, _ = held_scope(d, c)

        out = decided(c, s)

    assert out.status == DISCARDED
    assert list(out.reasons) == ["lint: secrets"]
    assert not (d.project / "src").exists()


def test_reviewers_run_in_order_and_a_later_verdict_never_loosens_an_earlier_one(
    start_daemon, runtime_dir
):
    first = stand_in(runtime_dir, "first", verdict="return", reasons=["fix it"])
    second = stand_in(runtime_dir, "second", verdict="commit")
    d = start_daemon(
        policy=policy(
            ("first", first, ""), ("second", second, ""), rules="reviewers: [first, second]"
        )
    )
    with d.client() as c:
        s, _ = held_scope(d, c)

        out = decided(c, s)

    assert out.status == RETURNED
    assert list(out.reasons) == ["first: fix it"]
    (a,), (b,) = first.runs(), second.runs()
    assert a.ended is not None and b.started >= a.ended
    assert (b.stdin["reviewer"], b.stdin["verdict_so_far"]) == ("second", "return")
    assert [(r["reviewer"], r["verdict"], r["reasons"]) for r in b.stdin["reviews"]] == [
        ("first", "return", ["fix it"])
    ]
    ops = [
        w for x in d.ledger if f" scope={s} " in x for w in x.split() if w.startswith("op=review-")
    ]
    assert ops == ["op=review-first", "op=review-second"]
    assert "decision=commit" in lines(d, s, "review-second")[0]


def test_a_discard_nobody_later_can_override_ends_the_review_without_running_the_rest(
    start_daemon, runtime_dir
):
    first = stand_in(runtime_dir, "first", verdict="discard")
    second = stand_in(runtime_dir, "second", verdict="commit")
    d = start_daemon(
        policy=policy(
            ("first", first, ""), ("second", second, ""), rules="reviewers: [first, second]"
        )
    )
    with d.client() as c:
        s, _ = held_scope(d, c)

        out = decided(c, s)

    assert out.status == DISCARDED
    assert len(first.runs()) == 1
    assert second.runs() == []
    assert lines(d, s, "review-second") == []


def test_a_discard_followed_by_a_reviewer_that_may_override_stays_held(start_daemon, runtime_dir):
    first = stand_in(runtime_dir, "first", verdict="discard")
    d = start_daemon(policy=policy(("first", first, ""), rules="reviewers: [first, human]"))
    with d.client() as c, d.reviewer() as rv:
        s, _ = held_scope(d, c)
        first.wait_for_runs(1)

        held = eventually(lambda: [h for h in rv.list_held() if list(h.reviewers) == ["human"]])

        assert (held[0].scope_id, held[0].verdict) == (s, pb.VERDICT_DISCARD)
        assert c.discard(s).status == DISCARDED


def test_withdrawing_a_held_scope_terminates_the_running_program_and_decides_once(
    start_daemon, runtime_dir
):
    lint = stand_in(runtime_dir, "lint", sleep=60)
    d = start_daemon(policy=policy(("lint", lint, "")))
    with d.client() as c:
        s, _ = held_scope(d, c)
        (run,) = lint.wait_for_runs(1)

        assert c.discard(s).status == DISCARDED

        eventually(lambda: run.terminated)
        eventually(lambda: not alive(run.pid))
    assert len(lines(d, s, "decide")) == 1
    assert lines(d, s, "review-lint") == []


def test_a_program_gets_only_the_allowlisted_environment(start_daemon, runtime_dir):
    lint = stand_in(runtime_dir, "lint")
    d = start_daemon(
        policy=policy(("lint", lint, "env: [KEEP_ME]")),
        env={"KEEP_ME": "kept", "SECRET_KEY": "hidden", "ESCROW_REVIEW_TOKEN": "x"},
    )
    with d.client() as c:
        s, _ = held_scope(d, c)
        decided(c, s)

    (run,) = lint.runs()
    assert run.env["KEEP_ME"] == "kept"
    assert set(run.env) <= {"PATH", "HOME", "LANG", "KEEP_ME"}


def test_reviewers_parallel_1_runs_one_program_at_a_time(start_daemon, runtime_dir):
    lint = stand_in(runtime_dir, "lint", sleep=0.5)
    d = start_daemon(
        policy=policy(("lint", lint, ""), extra="reviewers_parallel: 1\n", paths="*.txt")
    )
    with d.client() as c:
        one, _ = held_scope(d, c, path="one.txt")
        two, _ = held_scope(d, c, path="two.txt")

        assert decided(c, one).status == COMMITTED
        assert decided(c, two).status == COMMITTED

    first, second = lint.runs()
    assert first.ended is not None and second.started >= first.ended


def test_a_programs_stderr_goes_to_the_daemon_log_with_a_prefix(start_daemon, runtime_dir):
    lint = stand_in(runtime_dir, "lint", stderr="looked at util.py")
    d = start_daemon(policy=policy(("lint", lint, "")))
    with d.client() as c:
        s, _ = held_scope(d, c)
        decided(c, s)

    log = d.work / "daemon.log"
    eventually(lambda: f"escrowd: reviewer: lint {s}: looked at util.py" in log.read_text())
