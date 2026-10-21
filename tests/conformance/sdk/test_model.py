"""The public client and reviewer hand back model types and take string verdicts and
tiers; values a later daemon adds read as "unknown"."""

from typing import get_args

import pytest
from google.protobuf.message import Message

import escrow
from escrow import _launch, _model
from escrow.v1 import escrow_pb2 as pb

RULES = "review:\n  - {paths: ['src/**'], tier: llm}\n"


def write(d, scope_id, text="change\n", path="src/util.py"):
    f = d.mount / scope_id / path
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text(text)


def closed(d, c, path="src/util.py", **kw):
    sid = c.open_scope("t", **kw).scope_id
    write(d, sid, path=path)
    c.close_scope(sid)
    return sid


def held(d, c, session="agent-1"):
    sid = closed(d, c, session=session)
    assert c.commit(sid).status == "held"
    return sid


def test_unknown_values_are_not_decided_yet():
    cs = _model.ChangeSet.from_proto(
        pb.ChangeSet(
            scope_id="s",
            changes=[pb.Change(kind=99, path="a")],  # ty: ignore[invalid-argument-type]
            review=pb.Review(tiers=[pb.TIER_LLM, 9]),  # ty: ignore[invalid-argument-type]
        )
    )
    assert cs.changes[0].kind == "unknown"
    assert cs.review.tiers == ["llm", "unknown"]
    bad = pb.Outcome(status=42, tiers=[7])  # ty: ignore[invalid-argument-type]
    out = _model.Outcome.from_result(_model.Result.from_proto(bad), cs)
    assert (out.status, out.tiers) == ("held", ["unknown"])
    returned = _model.Result.from_proto(pb.Outcome(status=pb.OUTCOME_STATUS_RETURNED))
    assert _model.Outcome.from_result(returned, cs).status == "returned"


def test_unknown_verdicts_and_tiers_of_a_held_scope_read_unknown():
    h = _model.HeldScope.from_proto(
        pb.HeldScope(
            verdict=9,  # ty: ignore[invalid-argument-type]
            tiers=[8],  # ty: ignore[invalid-argument-type]
            reviews=[pb.TierReview(tier=7, verdict=6)],  # ty: ignore[invalid-argument-type]
        )
    )
    assert (h.verdict, h.tiers) == ("unknown", ["unknown"])
    assert (h.reviews[0].tier, h.reviews[0].verdict) == ("unknown", "unknown")


def test_each_public_call_returns_a_model_type(start_daemon):
    d = start_daemon(policy=RULES)
    with (
        escrow.Client(str(d.socket), tokens=d.tokens) as c,
        escrow.Reviewer(f"{d.socket}.review") as rv,
    ):
        results = []
        info = c.ping()
        opened = c.open_scope("t")
        write(d, opened.scope_id, path="docs/a.md")
        closed_cs = c.close_scope(opened.scope_id)
        fetched = c.get_change_set(opened.scope_id)
        results.append(c.commit(opened.scope_id))
        results.append(c.discard(closed(d, c)))
        sid = held(d, c)
        listed = rv.list_held()
        detail = rv.get_held(sid)
        reviewed = rv.review(sid, "llm", "commit")
        streamed = list(c.await_decision(sid))
        results += [reviewed, *streamed]
        a = c.open_scope("a")
        write(d, a.scope_id)
        decided = c.decide(c.close_scope(a.scope_id).scope_id, "discard")
    assert type(info) is escrow.DaemonInfo and info.protocol_version >= _launch._MIN_PROTOCOL
    assert type(opened) is escrow.OpenedScope and opened.token
    assert type(closed_cs) is type(fetched) is escrow.ChangeSet
    assert type(decided) is escrow.Result
    assert [type(r) for r in results] == [escrow.Result] * len(results)
    assert [type(h) for h in listed] == [escrow.HeldScope]
    assert type(detail) is escrow.HeldDetail and type(detail.changes) is escrow.ChangeSet
    assert detail.held.scope_id == sid and detail.held.tiers == ["llm"]
    assert streamed and all(isinstance(r, escrow.Result) for r in streamed)
    everything = [info, opened, closed_cs, fetched, decided, *results, *listed, detail]
    assert not [x for x in everything if isinstance(x, Message)]


def test_string_verdicts_commit_discard_and_return_work_through_decide(daemon):
    with escrow.Client(str(daemon.socket)) as c:
        got = {}
        for verdict in ("commit", "discard", "return"):
            sid = closed(daemon, c, path=f"{verdict}.txt")
            got[verdict] = c.decide(sid, verdict, reasons=[verdict])  # ty: ignore[invalid-argument-type]
    assert {v: r.status for v, r in got.items()} == {
        "commit": "committed",
        "discard": "discarded",
        "return": "returned",
    }
    assert got["return"].reopened


def test_review_with_string_tier_and_verdict_decides_a_held_scope(start_daemon):
    d = start_daemon(policy=RULES)
    with escrow.Client(str(d.socket), tokens=d.tokens) as c:
        sid = held(d, c)
        with escrow.Reviewer(f"{d.socket}.review") as rv:
            result = rv.review(sid, "llm", "commit", reasons=["fine"])
        (last,) = list(c.await_decision(sid))
    assert result.scope_id == sid and result.status == "committed"
    assert result.reasons == ["llm: fine"]
    assert last.status == "committed"


def test_every_verdict_and_tier_string_round_trips():
    assert set(get_args(escrow.Verdict)) == {"commit", "discard", "return"}
    assert set(get_args(escrow.Tier)) == {"software", "llm", "human"}
    for name in get_args(escrow.Verdict):
        assert _model._verdict(_model.verdict_value(name)) == name
    for name in get_args(escrow.Tier):
        assert _model._tiers([_model.tier_value(name)]) == [name]


def test_an_invalid_string_raises_before_any_rpc(runtime_dir):
    absent = str(runtime_dir / "absent.sock")  # a call would raise EscrowUnavailableError
    with escrow.Client(absent) as c, escrow.Reviewer(absent) as rv:
        with pytest.raises(ValueError) as err:
            c.decide("s", "x")  # ty: ignore[invalid-argument-type]
        assert str(err.value) == "verdict must be one of: commit, discard, return (got 'x')"
        with pytest.raises(ValueError) as err:
            rv.review("s", "x", "commit")  # ty: ignore[invalid-argument-type]
        assert str(err.value) == "tier must be one of: software, llm, human (got 'x')"
        with pytest.raises(ValueError, match="verdict must be one of"):
            rv.review("s", "llm", "x")  # ty: ignore[invalid-argument-type]


def test_review_as_rejects_an_invalid_name_or_verdict_before_any_rpc(runtime_dir):
    absent = str(runtime_dir / "absent.sock")
    with escrow.Reviewer(absent) as rv:
        for bad in ("", "Security", "1a", "a_b", "a b"):
            with pytest.raises(ValueError, match="reviewer must match"):
                rv.review_as("s", bad, "commit")
        with pytest.raises(ValueError, match="verdict must be one of"):
            rv.review_as("s", "llm", "x")  # ty: ignore[invalid-argument-type]


def test_review_keeps_rejecting_a_reviewer_name_as_a_tier(runtime_dir):
    absent = str(runtime_dir / "absent.sock")
    with escrow.Reviewer(absent) as rv, pytest.raises(ValueError, match="tier must be one of"):
        rv.review("s", "security", "commit")  # ty: ignore[invalid-argument-type]


def test_review_as_sends_the_name_the_token_and_no_tier():
    sent = []

    class Low:
        def review(self, *args, **kw):
            sent.append((args, kw))
            return pb.Outcome(scope_id="s", status=pb.OUTCOME_STATUS_HELD, reviewers=["human"])

        def close(self):
            pass

    rv = escrow.Reviewer.__new__(escrow.Reviewer)
    rv._low = Low()
    result = rv.review_as("s", "oncall", "return", reasons=["no"], override=True, token="tok")
    (args, kw) = sent[0]
    assert args[0] == "s" and args[1] == pb.TIER_UNSPECIFIED and args[2] == pb.VERDICT_RETURN
    assert args[3:5] == (["no"], True)
    assert kw == {"reviewer": "oncall", "token": "tok"}
    assert result.reviewers == ["human"]
    rv.review_as("s", "oncall", "commit")
    assert sent[1][1]["token"] == ""


def test_new_fields_map_from_the_protocol_messages():
    r = _model.Review.from_proto(pb.Review(reviewers=["lint", "llm"], tiers=[pb.TIER_LLM]))
    assert r.reviewers == ["lint", "llm"] and r.tiers == ["llm"]
    out = pb.Outcome(status=pb.OUTCOME_STATUS_HELD, reviewers=["lint"])
    res = _model.Result.from_proto(out)
    assert res.reviewers == ["lint"]
    cs = _model.ChangeSet.from_proto(pb.ChangeSet(scope_id="s"))
    assert _model.Outcome.from_result(res, cs).reviewers == ["lint"]
    h = _model.HeldScope.from_proto(
        pb.HeldScope(
            reviewers=["lint", "human"],
            reviews=[
                pb.TierReview(reviewer="lint", verdict=pb.VERDICT_COMMIT),
                pb.TierReview(tier=pb.TIER_LLM, verdict=pb.VERDICT_COMMIT),
            ],
        )
    )
    assert h.reviewers == ["lint", "human"]
    assert [x.reviewer for x in h.reviews] == ["lint", "llm"]


def test_a_protocol_7_message_with_no_reviewers_reads_empty_and_falls_back_to_tiers():
    h = _model.HeldScope.from_proto(
        pb.HeldScope(
            tiers=[pb.TIER_LLM, pb.TIER_HUMAN], reviews=[pb.TierReview(tier=pb.TIER_HUMAN)]
        )
    )
    assert h.reviewers == [] and h.tiers == ["llm", "human"]
    assert h.reviews[0].reviewer == "human"
    assert _model.Result.from_proto(pb.Outcome(tiers=[pb.TIER_LLM])).reviewers == []
    assert _model.Review.from_proto(pb.Review()).reviewers == []
