"""Held decisions: a runnable model of the post-approval protocol (proposed phase 3).

    python3 examples/held-decisions/model.py

Today an agent host blocks on a *pre*-approval: a permission prompt before a tool
runs, judged on a description of the effect. Escrow turns it into a *post*-approval:
the work runs in a scope, and independent reviewers judge the staged change set. This
model plays the protocol between three roles, in memory, with no daemon:

- the **client** (the agent's SDK) holds a scope's token: it closes the scope and
  *proposes* a verdict, and says whether it can wait for a held decision;
- **escrowd** holds the change set, applies the policy's review rules (which tiers
  review which paths, and whether the agent must wait) and enforces the outcome;
- **reviewers** (software in the daemon; LLM and human outside it) hold a reviewer
  credential: only they decide a held scope. A change set goes through every tier its
  paths need, cheapest first.

Rules the scenarios show:

1. Blocking is negotiated: the policy can require it, the client can ask for it, and
   the client continues only when both allow. escrowd enforces a required wait itself:
   the session's next scope does not open until the held one is decided.
2. Independence: the opener's token cannot commit a held scope.
3. Monotonic verdicts: commit < return < discard. Each tier can only tighten the
   verdict so far; loosening it takes an explicit human override, which the ledger
   records. The opener may still tighten (withdraw its own held scope).
4. Not waiting has a cost: the next scope's snapshot lacks the held changes, so the
   same file edited again conflicts at commit.
5. Cross-turn review: a reviewer given the session's earlier change sets judges an
   effect split across turns as a whole.
"""

from __future__ import annotations

import asyncio
import fnmatch
import itertools
import secrets
import time
from dataclasses import dataclass, field

COMMIT, RETURN, DISCARD = "commit", "return", "discard"
STRICTNESS = {COMMIT: 0, RETURN: 1, DISCARD: 2}
TIERS = ("software", "llm", "human")  # cheapest first
T0 = time.monotonic()


def log(who: str, msg: str) -> None:
    print(f"  [{time.monotonic() - T0:4.2f}s] {who:<8} {msg}")


def tighter(a: str, b: str) -> str:
    return a if STRICTNESS[a] >= STRICTNESS[b] else b


class PermissionDenied(Exception):
    pass


# ---- policy ----


@dataclass(frozen=True)
class ReviewRule:
    pattern: str  # glob over project paths; the first matching rule applies
    tier: str  # the most expensive tier the path needs: "software", "llm" or "human"
    wait: str  # "required", or "optional" (the client may continue)


@dataclass
class Policy:
    review: list[ReviewRule]
    forbidden: tuple[str, ...] = ("BEGIN PRIVATE KEY",)  # a software-tier write rule

    def needs(self, paths: list[str]) -> tuple[list[str], bool]:
        """The tiers above software that the change set needs, cheapest first, and
        whether the agent must wait."""
        top, required = 0, False
        for p in paths:
            rule = next(r for r in self.review if fnmatch.fnmatch(p, r.pattern))
            top = max(top, TIERS.index(rule.tier))
            required |= rule.wait == "required"
        return list(TIERS[1 : top + 1]), required


# ---- escrowd ----


@dataclass
class Scope:
    id: str
    session: str
    token: str
    snapshot: dict[str, tuple[str, int]]  # path -> (content, version) at open
    outcome: asyncio.Future
    writes: dict[str, str] = field(default_factory=dict)
    state: str = "open"  # open, held, decided
    verdict: str = COMMIT  # the strictest verdict so far
    pending: list[str] = field(default_factory=list)  # tiers still to review
    must_wait: bool = False


class Escrowd:
    def __init__(self, policy: Policy, base: dict[str, str]):
        self.policy = policy
        self.base = {p: (c, 1) for p, c in base.items()}
        self.scopes: dict[str, Scope] = {}
        self.history: dict[str, list[dict[str, str]]] = {}  # session -> committed change sets
        self.ids = itertools.count(1)
        self.queues = {"llm": asyncio.Queue(), "human": asyncio.Queue()}
        self.ledger: list[str] = []

    # -- client calls (need the scope's token) --

    async def open_scope(self, session: str) -> Scope:
        for s in list(self.scopes.values()):  # a required wait is enforced here
            if s.session == session and s.state == "held" and s.must_wait:
                log("escrowd", f"open in {session} waits: {s.id} is held for {s.pending[0]}")
                await asyncio.shield(s.outcome)
        loop = asyncio.get_running_loop()
        s = Scope(
            f"s{next(self.ids)}",
            session,
            secrets.token_hex(8),
            dict(self.base),
            loop.create_future(),
        )
        self.scopes[s.id] = s
        log("escrowd", f"opened {s.id} in {session}")
        return s

    def write(self, s: Scope, path: str, content: str) -> None:
        s.writes[path] = content

    def read(self, s: Scope, path: str) -> str:
        return s.writes.get(path, s.snapshot.get(path, ("", 0))[0])

    async def close(self, s: Scope, token: str, proposal: str, can_wait: bool) -> str:
        """Close with the client's proposed verdict and whether it can wait. Returns
        committed, discarded, returned, conflict, or held."""
        self._check(s, token)
        if proposal != COMMIT:  # the agent tightened its own scope: nothing to review
            return self._decide(s, proposal, "proposed by the agent")
        if any(f in c for c in s.writes.values() for f in self.policy.forbidden):
            return self._decide(s, DISCARD, "software rule: forbidden content")
        s.pending, required = self.policy.needs(sorted(s.writes))
        if not s.pending:
            return self._decide(s, COMMIT, "software rules pass, no review needed")
        s.state, s.must_wait = "held", required or can_wait
        how = "agent must wait" if required else "agent waits" if can_wait else "agent continues"
        log("escrowd", f"{s.id} held for {' then '.join(s.pending)}; {how}")
        self.ledger.append(f"scope={s.id} op=hold tiers={','.join(s.pending)} wait={s.must_wait}")
        await self._queue(s)
        return "held"

    async def await_decision(self, s: Scope, token: str) -> str:
        self._check(s, token)
        return await asyncio.shield(s.outcome)

    def decide(self, s: Scope, token: str, verdict: str) -> str:
        """The opener may tighten a held scope (withdraw it), never commit it."""
        self._check(s, token)
        if s.state == "held" and verdict != DISCARD:
            raise PermissionDenied(f"{s.id} is held for {s.pending[0]}: only a reviewer decides")
        return self._decide(s, verdict, "withdrawn by the agent")

    # -- reviewer calls (need the reviewer credential; modelled by the tier name) --

    async def review(self, s: Scope, tier: str, verdict: str, why: str, override=False) -> None:
        if s.state != "held" or s.pending[0] != tier:
            raise PermissionDenied(f"{s.id} is not waiting for {tier}")
        if STRICTNESS[verdict] < STRICTNESS[s.verdict]:
            if not (override and tier == "human"):
                raise PermissionDenied(f"{tier} cannot loosen {s.verdict} to {verdict}")
            self.ledger.append(f"scope={s.id} op=override tier={tier} {s.verdict}->{verdict}")
            s.verdict = verdict
        else:
            s.verdict = tighter(s.verdict, verdict)
        log(tier, f"{s.id}: {verdict} ({why}); verdict so far {s.verdict}")
        s.pending.pop(0)
        if s.pending:  # a discard still goes up: only a human may override it
            await self._queue(s)
        else:
            self._decide(s, s.verdict, f"after {tier} review")

    # -- internals --

    async def _queue(self, s: Scope) -> None:
        await self.queues[s.pending[0]].put((s, dict(s.writes), self.history.get(s.session, [])))

    def _check(self, s: Scope, token: str) -> None:
        if token != s.token:
            raise PermissionDenied(f"{s.id}: missing or wrong token")

    def _decide(self, s: Scope, verdict: str, why: str) -> str:
        if s.state == "decided":
            raise PermissionDenied(f"{s.id} is already decided")
        status = {COMMIT: "committed", RETURN: "returned", DISCARD: "discarded"}[verdict]
        if verdict == COMMIT:
            stale = [
                p for p in s.writes if self.base.get(p, ("", 0))[1] != s.snapshot.get(p, ("", 0))[1]
            ]
            if stale:
                status, why = "conflict", f"{', '.join(stale)} changed since {s.id} opened"
            else:
                for p, c in s.writes.items():
                    self.base[p] = (c, self.base.get(p, ("", 0))[1] + 1)
                self.history.setdefault(s.session, []).append(dict(s.writes))
        s.state = "decided"
        self.ledger.append(f"scope={s.id} op=decide decision={status}")
        log("escrowd", f"{s.id} {status} ({why})")
        s.outcome.set_result(status)
        return status


# ---- reviewers ----


def llm_judges(writes: dict[str, str], history: list[dict[str, str]]) -> tuple[str, str]:
    """A stand-in auditor: flags a password passed to a helper that posts to the
    network, also when the helper came in an earlier change set of the session."""
    helpers = {
        line[4 : line.index("(")]
        for cs in [*history, writes]
        for content in cs.values()
        if "requests.post" in content
        for line in content.splitlines()
        if line.startswith("def ")
    }
    if any(f"{h}(password" in c for h in helpers for c in writes.values()):
        return DISCARD, "sends the password through a network helper"
    return COMMIT, "looks fine"


async def llm_reviewer(d: Escrowd, with_history: bool) -> None:
    while True:
        s, writes, history = await d.queues["llm"].get()
        await asyncio.sleep(0.2)
        seen = history if with_history else []
        verdict, why = llm_judges(writes, seen)
        await d.review(s, "llm", verdict, f"{why}; saw {len(seen)} earlier change sets")


async def human_reviewer(d: Escrowd, wants: dict[str, str]) -> None:
    """Approves, unless `wants` names another verdict for a scope; overrides when the
    verdict it wants is looser than the one so far."""
    while True:
        s, _, _ = await d.queues["human"].get()
        await asyncio.sleep(0.5)
        verdict = wants.get(s.id, COMMIT)
        try:
            await d.review(s, "human", verdict, "reviewed")
        except PermissionDenied as e:
            log("human", f"refused: {e}; overrides explicitly")
            await d.review(s, "human", verdict, "override", override=True)


# ---- scenarios ----

POLICY = Policy(
    review=[
        ReviewRule("src/auth/*", "human", "required"),
        ReviewRule("src/*", "llm", "required"),
        ReviewRule("docs/*", "llm", "optional"),
        ReviewRule("*", "software", "optional"),
    ]
)
BASE = {
    "README.md": "# demo\n",
    "docs/guide.md": "v1\n",
    "src/auth/login.py": "",
    "src/util.py": "",
}
HELPER = "def upload(x):\n    requests.post(URL, x)\n"
CALLER = "def login(password):\n    upload(password)\n"


async def negotiated_wait(d: Escrowd) -> None:
    s = await d.open_scope("agent-1")
    d.write(s, "src/auth/login.py", "def login(): ...\n")
    status = await d.close(s, s.token, COMMIT, can_wait=False)  # the client offers to continue
    log("client", f"proposed commit, got {status}; opens the next scope anyway")
    nxt = await d.open_scope("agent-1")
    log("client", f"{nxt.id} sees login.py = {d.read(nxt, 'src/auth/login.py')!r}")


async def independence(d: Escrowd) -> None:
    s = await d.open_scope("agent-1")
    d.write(s, "src/auth/login.py", "def login(): return True  # skip the check\n")
    await d.close(s, s.token, COMMIT, can_wait=True)
    try:
        d.decide(s, s.token, COMMIT)
    except PermissionDenied as e:
        log("client", f"commit with its own token refused: {e}")
    log("client", f"waits, gets {await d.await_decision(s, s.token)}")


async def monotonic(d: Escrowd) -> None:
    s = await d.open_scope("agent-1")
    d.write(s, "src/util.py", HELPER)
    d.write(s, "src/auth/login.py", CALLER)
    await d.close(s, s.token, COMMIT, can_wait=True)
    await d.await_decision(s, s.token)
    print("  ledger overrides:", [x for x in d.ledger if "override" in x] or "none")


async def continuing_costs(d: Escrowd) -> None:
    a = await d.open_scope("agent-1")
    d.write(a, "docs/guide.md", "v2 from turn 1\n")
    status = await d.close(a, a.token, COMMIT, can_wait=False)
    log("client", f"turn 1: {status}; docs/ may continue, so the agent does")
    b = await d.open_scope("agent-1")
    log("client", f"turn 2 reads guide.md = {d.read(b, 'docs/guide.md')!r}, not turn 1's")
    d.write(b, "docs/guide.md", "v1 plus a section\n")
    await d.close(b, b.token, COMMIT, can_wait=True)
    log("client", f"turn 2: {await d.await_decision(b, b.token)}")


async def split_across_turns(d: Escrowd) -> None:
    for turn, (path, content) in enumerate(
        [("src/util.py", HELPER), ("src/auth/login.py", CALLER)], 1
    ):
        s = await d.open_scope("agent-1")
        d.write(s, path, content)
        await d.close(s, s.token, COMMIT, can_wait=True)
        log("client", f"turn {turn}: {await d.await_decision(s, s.token)}")


async def scenario(title: str, body, *, human=None, history=True) -> None:
    global T0
    T0 = time.monotonic()
    print(f"\n== {title}")
    d = Escrowd(POLICY, BASE)
    workers = [
        asyncio.create_task(llm_reviewer(d, history)),
        asyncio.create_task(human_reviewer(d, human or {})),
    ]
    try:
        await body(d)
    finally:
        for w in workers:
            w.cancel()


async def main() -> None:
    await scenario(
        "1. Negotiated wait: the policy requires it, escrowd enforces it", negotiated_wait
    )
    await scenario("2. Independence: the agent cannot approve its own held scope", independence)
    await scenario(
        "3a. Monotonic: the LLM discards, the human confirms", monotonic, human={"s1": DISCARD}
    )
    await scenario("3b. Monotonic: loosening the LLM's discard is a recorded override", monotonic)
    await scenario("4. Not waiting has a cost: turn 2 works on a stale snapshot", continuing_costs)
    await scenario(
        "5a. Split across turns, reviewer sees one change set: passes",
        split_across_turns,
        history=False,
    )
    await scenario(
        "5b. Same turns, reviewer sees the session's history: caught",
        split_across_turns,
        human={"s2": DISCARD},
    )


if __name__ == "__main__":
    asyncio.run(main())
