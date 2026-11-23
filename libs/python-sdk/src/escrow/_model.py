"""Plain data the SDK hands back: decisions, change sets, outcomes and their parsing from
the protocol's messages. With `lowlevel`, the only module that imports the generated
protocol code."""

from __future__ import annotations

import re
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Any, Literal, TypeVar

from escrow.v1 import escrow_pb2 as pb

T = TypeVar("T")

Verdict = Literal["commit", "discard", "return"]
Tier = Literal["software", "llm", "human"]
Status = Literal["committed", "discarded", "returned", "conflict", "held"]
ChangeKind = Literal["create", "modify", "delete", "rename"]
# What a value a later daemon adds reads as.
Unknown = Literal["unknown"]

_VERDICTS: dict[Verdict, pb.Verdict] = {
    "commit": pb.VERDICT_COMMIT,
    "discard": pb.VERDICT_DISCARD,
    "return": pb.VERDICT_RETURN,
}
_VERDICT_NAMES: dict[pb.Verdict, Verdict] = {v: k for k, v in _VERDICTS.items()}
_TIER_VALUES: dict[Tier, pb.Tier] = {
    "software": pb.TIER_SOFTWARE,
    "llm": pb.TIER_LLM,
    "human": pb.TIER_HUMAN,
}
_TIERS: dict[pb.Tier, Tier] = {v: k for k, v in _TIER_VALUES.items()}


def _choice(what: str, value: str, table: dict[Any, T]) -> T:
    if value not in table:
        raise ValueError(f"{what} must be one of: {', '.join(table)} (got {value!r})")
    return table[value]


def verdict_value(verdict: str) -> pb.Verdict:
    """The protocol's verdict for `verdict`; ValueError for any other string."""
    return _choice("verdict", verdict, _VERDICTS)


def tier_value(tier: str) -> pb.Tier:
    """The protocol's tier for `tier`; ValueError for any other string."""
    return _choice("tier", tier, _TIER_VALUES)


# The tier of a review request that names its reviewer.
NO_TIER: pb.Tier = pb.TIER_UNSPECIFIED
_REVIEWER_NAME = re.compile(r"[a-z][a-z0-9-]*")


def reviewer_name(name: str) -> str:
    """`name` when it is a valid reviewer name (lower case letters, digits and `-`, the
    first a letter); ValueError for any other string."""
    if not isinstance(name, str) or not _REVIEWER_NAME.fullmatch(name):
        raise ValueError(f"reviewer must match [a-z][a-z0-9-]* (got {name!r})")
    return name


def _verdict(v: int) -> Verdict | Unknown:
    return _VERDICT_NAMES.get(v, "unknown")


@dataclass(frozen=True)
class Decision:
    verdict: Verdict
    reasons: tuple[str, ...] = ()


def commit() -> Decision:
    """Apply the scope's change set to the project, all or nothing."""
    return Decision("commit")


def discard(*reasons: str) -> Decision:
    """Drop the scope's changes."""
    return Decision("discard", reasons)


def send_back(*reasons: str) -> Decision:
    """Return to agent: the reasons go back, the scope stays open for a fix (`resume=`)."""
    return Decision("return", reasons)


_KINDS: dict[int, ChangeKind] = {
    pb.CHANGE_KIND_CREATE: "create",
    pb.CHANGE_KIND_MODIFY: "modify",
    pb.CHANGE_KIND_DELETE: "delete",
    pb.CHANGE_KIND_RENAME: "rename",
}
# The decided statuses. Any other value, one a later daemon adds included, is not
# decided yet: "held" (docs/protocol.md, "Unknown values").
_DECIDED: dict[int, Status] = {
    pb.OUTCOME_STATUS_COMMITTED: "committed",
    pb.OUTCOME_STATUS_DISCARDED: "discarded",
    pb.OUTCOME_STATUS_RETURNED: "returned",
    pb.OUTCOME_STATUS_CONFLICT: "conflict",
}


@dataclass(frozen=True)
class Change:
    kind: ChangeKind | Unknown  # "unknown" for a kind added later
    path: str  # project-relative; for a rename, the destination
    from_path: str | None = None
    # Ids of the processes that made it (`ChangeSet.processes`).
    writers: tuple[int, ...] = ()


@dataclass(frozen=True)
class Process:
    """A process that changed the scope's files. The program and its device and inode
    are what the kernel ran; the arguments are what the process says about itself."""

    id: int
    pid: int
    program: str
    dev: int
    ino: int
    args: list[str]
    parent: int  # the parent's id in `ChangeSet.processes`; 0 where the chain stops


@dataclass(frozen=True)
class Read:
    path: str
    allowed: bool


def _tiers(tiers) -> list[Tier | Unknown]:
    return [_TIERS.get(t, "unknown") for t in tiers]


def _status(status: int) -> Status:
    return _DECIDED.get(status, "held")


@dataclass(frozen=True)
class Review:
    """The daemon's close-time review (policy `write:` and `review:`)."""

    # The software tier's verdict: "discard" when a write rule broke (a commit or a
    # return then becomes a discard), else "commit".
    verdict: Verdict = "commit"
    reasons: list[str] = field(default_factory=list)
    # Tiers above software the change set needs, cheapest first ("llm", "human"); a
    # commit then holds the scope for them.
    tiers: list[Tier | Unknown] = field(default_factory=list)
    # A review rule requires the agent to wait for those tiers.
    wait_required: bool = False
    # Every reviewer the change set needs, in review order (the policy's declared ones,
    # then "llm" and "human"); empty from a daemon older than protocol 8.
    reviewers: list[str] = field(default_factory=list)

    @classmethod
    def from_proto(cls, r: pb.Review) -> Review:
        return cls(
            verdict="discard" if r.verdict == pb.VERDICT_DISCARD else "commit",
            reasons=list(r.reasons),
            tiers=_tiers(r.tiers),
            wait_required=r.wait_required,
            reviewers=list(r.reviewers),
        )


@dataclass(frozen=True)
class ChangeSet:
    """What a closed scope would do to the project, and what it read."""

    scope_id: str
    changes: list[Change]
    reads: list[Read]
    labels: dict[str, str]
    # Content diff against the scope's snapshot, git format; binary and large files
    # summarized (size, SHA-256); capped by the policy's `diff:` sizes.
    diff: str = ""
    # Changes that reached the unscoped mode while the scope was open (0 in passthrough).
    unscoped: int = 0
    review: Review = field(default_factory=Review)
    # Every change's writers and their parents, by id.
    processes: dict[int, Process] = field(default_factory=dict)

    @property
    def paths(self) -> list[str]:
        return [c.path for c in self.changes]

    def writers(self, change: Change) -> list[Process]:
        return [self.processes[w] for w in change.writers if w in self.processes]

    @classmethod
    def from_proto(cls, cs: pb.ChangeSet) -> ChangeSet:
        return cls(
            scope_id=cs.scope_id,
            changes=[
                Change(_KINDS.get(c.kind, "unknown"), c.path, c.from_path or None, tuple(c.writers))
                for c in cs.changes
            ],
            reads=[Read(r.path, r.decision == pb.READ_DECISION_ALLOW) for r in cs.reads],
            labels=dict(cs.labels),
            diff=cs.diff,
            unscoped=cs.unscoped_ops,
            review=Review.from_proto(cs.review),
            processes={
                p.id: Process(p.id, p.pid, p.program, p.dev, p.ino, list(p.args), p.parent)
                for p in cs.processes
            },
        )


@dataclass(frozen=True)
class DaemonInfo:
    daemon_version: str
    protocol_version: int

    @classmethod
    def from_proto(cls, r: pb.PingResponse) -> DaemonInfo:
        return cls(r.daemon_version, r.protocol_version)


@dataclass(frozen=True)
class ScopeRoot:
    """A served root outside the project."""

    path: str  # the host path
    view: str  # where the scope sees it
    direct: tuple[str, ...]  # paths under it that bypass the scope

    @classmethod
    def from_proto(cls, r: pb.ScopeRoot) -> ScopeRoot:
        return cls(r.path, r.view, tuple(r.direct))


@dataclass(frozen=True)
class OpenedScope:
    scope_id: str
    root: str  # the project's view
    roots: list[ScopeRoot]
    # the scope's capability; keep it out of paths, labels, child environments and repr
    token: str = field(repr=False)

    @classmethod
    def from_proto(cls, r: pb.OpenScopeResponse) -> OpenedScope:
        return cls(r.scope_id, r.root, [ScopeRoot.from_proto(x) for x in r.roots], r.token)


@dataclass(frozen=True)
class Result:
    """What the daemon answered to a decision (the protocol's Outcome), before the
    change set joins it as an `Outcome`."""

    scope_id: str
    status: Status
    paths: list[str]
    reasons: list[str]
    reopened: bool = False
    tiers: list[Tier | Unknown] = field(default_factory=list)
    wait: bool = False
    # Held: the reviewers still to review, in order (empty from a daemon older than protocol 8).
    reviewers: list[str] = field(default_factory=list)

    @classmethod
    def from_proto(cls, o: pb.Outcome) -> Result:
        return cls(
            o.scope_id,
            _status(o.status),
            list(o.paths),
            list(o.reasons),
            o.reopened,
            _tiers(o.tiers),
            o.wait,
            list(o.reviewers),
        )


@dataclass(frozen=True)
class TierReview:
    tier: Tier | Unknown
    verdict: Verdict | Unknown
    reasons: list[str]
    override: bool = False
    # The reviewer's name: the daemon's, else the tier's name (a daemon older than
    # protocol 8 names none).
    reviewer: str = ""

    @classmethod
    def from_proto(cls, r: pb.TierReview) -> TierReview:
        tier = _TIERS.get(r.tier, "unknown")
        return cls(tier, _verdict(r.verdict), list(r.reasons), r.override, r.reviewer or tier)


@dataclass(frozen=True)
class HeldScope:
    """A scope held for review."""

    scope_id: str
    name: str
    labels: dict[str, str]
    session: str
    tiers: list[Tier | Unknown]  # still to review, cheapest first
    wait: bool  # the session's next scope waits for the verdict
    verdict: Verdict | Unknown  # the strictest so far; "commit" before any review
    reviews: list[TierReview]
    held_at_ms: int
    # Still to review, in order; empty from a daemon older than protocol 8 (read `tiers`).
    reviewers: list[str] = field(default_factory=list)

    @classmethod
    def from_proto(cls, h: pb.HeldScope) -> HeldScope:
        return cls(
            h.scope_id,
            h.name,
            dict(h.labels),
            h.session,
            _tiers(h.tiers),
            h.wait,
            _verdict(h.verdict),
            [TierReview.from_proto(r) for r in h.reviews],
            h.held_at_ms,
            list(h.reviewers),
        )


@dataclass(frozen=True)
class Decided:
    """A decided scope, kept for its session's reviewers."""

    scope_id: str
    name: str
    outcome: Outcome  # its `changes` hold the change set as decided
    reviews: list[TierReview]
    decided_at_ms: int

    @classmethod
    def from_proto(cls, d: pb.Decided) -> Decided:
        changes = ChangeSet.from_proto(d.change_set)
        return cls(
            d.scope_id,
            d.name,
            Outcome.from_result(Result.from_proto(d.outcome), changes),
            [TierReview.from_proto(r) for r in d.reviews],
            d.decided_at_ms,
        )


@dataclass(frozen=True)
class HeldDetail:
    held: HeldScope
    changes: ChangeSet  # diff and writers included
    history: list[Decided]  # the session's earlier decisions, oldest first

    @classmethod
    def from_proto(cls, r: pb.GetHeldResponse) -> HeldDetail:
        return cls(
            HeldScope.from_proto(r.held),
            ChangeSet.from_proto(r.change_set),
            [Decided.from_proto(d) for d in r.history],
        )


@dataclass(frozen=True)
class Outcome:
    # "held" is not decided yet, a status added later included
    status: Status
    paths: list[str]  # changed paths, or the conflicting ones
    # Why: the decide callback's or the rules' reasons; after a review, each tier's
    # (`llm: …`, `human: …`).
    reasons: list[str]
    changes: ChangeSet
    # The scope is open again with its changes (`resume=` it): status returned, or a
    # conflict under the policy's `conflict.verdict: return`.
    reopened: bool = False
    # Status held: the tiers still to review, cheapest first ("llm", "human").
    tiers: list[Tier | Unknown] = field(default_factory=list)
    # Status held: the session's next scope waits for the verdict (a rule requires it,
    # or the scope asked to wait).
    wait: bool = False
    # Status held: the reviewers still to review, in order.
    reviewers: list[str] = field(default_factory=list)

    @classmethod
    def from_result(
        cls, result: Result, changes: ChangeSet, reasons: tuple[str, ...] | list[str] = ()
    ) -> Outcome:
        """`result` for the change set `changes`; `reasons` stand in when the daemon
        gave none."""
        return cls(
            result.status,
            list(result.paths),
            list(result.reasons) or list(reasons),
            changes,
            result.reopened,
            list(result.tiers),
            result.wait,
            list(result.reviewers),
        )

    @property
    def reads(self) -> list[Read]:
        return self.changes.reads

    @property
    def labels(self) -> dict[str, str]:
        return self.changes.labels

    @property
    def diff(self) -> str:
        return self.changes.diff

    @property
    def unscoped(self) -> int:
        return self.changes.unscoped


Decide = Callable[[ChangeSet], "Decision | None | Awaitable[Decision | None]"]
