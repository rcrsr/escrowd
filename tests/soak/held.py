"""Held soak (phase 3, check 4): SIGKILL the daemon while reviewers decide held scopes.

    uv run --project sdk/python --frozen python tests/soak/held.py [RUNS]

Each run holds 8 scopes for review (`src/auth/` needs llm and human, `src/` llm, `docs/`
llm with an optional wait; sessions a and b and none), each staging 150 new files. A
reviewer then gives random tiers' verdicts (monotonic, a human override now and then)
and the opener withdraws a scope now and then, until no scope is held; the daemon is
killed at a random time inside that, and a quarter of the runs kill its restart during
recovery. After a clean start every scope must be where the acknowledged calls left it,
or one step on for the call in flight:

- held: the same tiers still to review, the same verdict so far and reviews;
- decided: its outcome from `AwaitDecision`, its files in the project all or none, as
  the outcome says; a returned scope open again with its changes;
- a session with a scope held with a wait still blocks its next scope.

Then the last tier of every scope still held keeps its verdict, which must apply. Any
other state fails the run and keeps its directory. Run 0 calibrates: no kill. Each run
notes whether a call was in flight at the kill and whether it took effect.
"""

import argparse
import os
import random
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field, replace
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "conformance"))
sys.path.insert(0, str(ROOT / "tests" / "soak"))

from conftest import Daemon, make_runtime_dir  # noqa: E402
from crash import leftovers, spawn_daemon, umount  # noqa: E402

import escrow  # noqa: E402
from escrow.v1 import escrow_pb2 as pb  # noqa: E402

POLICY = """\
review:
  - {paths: ['src/auth/**'], tier: human}
  - {paths: ['src/**'], tier: llm}
  - {paths: ['docs/**'], tier: llm, wait: optional}
"""
LLM, HUMAN = pb.TIER_LLM, pb.TIER_HUMAN
COMMIT, RETURN, DISCARD = pb.VERDICT_COMMIT, pb.VERDICT_RETURN, pb.VERDICT_DISCARD
RANK = {COMMIT: 0, RETURN: 1, DISCARD: 2}
FINAL = {COMMIT: "committed", RETURN: "returned", DISCARD: "discarded"}
FILES = 150
# (directory, session, client asks to wait); auth needs llm then human.
SCOPES = [
    ("src/auth/s0", "a", False),
    ("src/s1", "a", False),
    ("docs/s2", "a", True),
    ("src/auth/s3", "b", False),
    ("src/s4", "b", False),
    ("docs/s5", "", False),
    ("src/s6", "", False),
    ("src/auth/s7", "", False),
]


@dataclass(frozen=True)
class State:
    """A scope as the daemon should have it: held with `tiers` left, or `final`."""

    tiers: tuple[int, ...] = ()
    verdict: int = COMMIT
    reviews: int = 0
    wait: bool = False
    final: str = ""  # committed, returned, discarded


@dataclass
class Scope:
    id: str
    dir: str
    session: str
    token: str
    state: State
    # The states a kill may leave: the last acknowledged, and one step on while a call
    # is in flight.
    maybe: list[State] = field(default_factory=list)


def files(d: Path, dir: str) -> dict[str, str]:
    return {f"{dir}/f{j:02}.py": f"{dir} {j}\n" * 8 for j in range(FILES)}


def after_review(s: State, tier: int, verdict: int, over: bool) -> State:
    v = verdict if over else max(s.verdict, verdict, key=RANK.__getitem__)
    # A human's verdict also stands for the llm tier still pending.
    left = () if tier == HUMAN else tuple(t for t in s.tiers if t != tier)
    if left:
        return replace(s, tiers=left, verdict=v, reviews=s.reviews + 1)
    return State(final=FINAL[v])


def hold_all(d: Daemon, c: escrow.Client) -> list[Scope]:
    # Open every scope before holding any: a held scope with a wait blocks its session.
    opened = [(c.open_scope(session=session), dir, session, wait) for dir, session, wait in SCOPES]
    out = []
    for o, dir, session, wait in opened:
        for path, text in files(d.mount, dir).items():
            p = d.mount / o.scope_id / path
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        c.close_scope(o.scope_id)
        r = c.commit(o.scope_id, wait=wait)
        assert r.status == pb.OUTCOME_STATUS_HELD, r
        st = State(tiers=tuple(r.tiers), wait=r.wait)
        out.append(Scope(o.scope_id, dir, session, o.token, st))
    # Some auth scopes start with the llm's commit, waiting for the human.
    rv = escrow.connect_reviewer(str(d.socket))
    with rv:
        for s in out:
            if s.state.tiers == (LLM, HUMAN) and random.random() < 0.5:
                rv.review(s.id, LLM, COMMIT, reasons=["ok"])
                s.state = after_review(s.state, LLM, COMMIT, False)
    return out


def decide_all(d: Daemon, scopes: list[Scope], rng: random.Random, stop: threading.Event):
    """Review or withdraw held scopes at random until none is held or the daemon dies."""
    try:
        with escrow.connect_reviewer(str(d.socket)) as rv, escrow.connect(str(d.socket)) as c:
            while not stop.is_set():
                held = [s for s in scopes if not s.state.final]
                if not held:
                    return
                s = rng.choice(held)
                if rng.random() < 0.1:  # the opener withdraws it
                    s.maybe = [s.state, State(final="discarded")]
                    c.discard(s.id, token=s.token, timeout=60)
                    s.state, s.maybe = State(final="discarded"), []
                    continue
                tier = HUMAN if HUMAN in s.state.tiers and rng.random() < 0.3 else s.state.tiers[0]
                over = tier == HUMAN and s.state.verdict != COMMIT and rng.random() < 0.3
                verdict = COMMIT if over else rng.choice([COMMIT, COMMIT, RETURN, DISCARD])
                if RANK[verdict] < RANK[s.state.verdict] and not over:
                    verdict = s.state.verdict
                nxt = after_review(s.state, tier, verdict, over)
                s.maybe = [s.state, nxt]
                rv.review(
                    s.id, tier, verdict, reasons=[f"r{s.state.reviews}"], override=over, timeout=60
                )
                s.state, s.maybe = nxt, []
    except escrow.EscrowRpcError:
        pass  # the kill


def observe(d: Daemon, scopes: list[Scope]) -> dict[str, State | str]:
    """Each scope's state after the restart, or an error string."""
    seen: dict[str, State | str] = {}
    with escrow.connect_reviewer(str(d.socket)) as rv, escrow.connect(str(d.socket)) as c:
        held = {h.scope_id: h for h in rv.list_held()}
        for s in scopes:
            want = files(d.project, s.dir)
            on_disk = {p: (d.project / p).read_text() for p in want if (d.project / p).exists()}
            if on_disk and on_disk != want:
                seen[s.id] = f"partial: {len(on_disk)} of {len(want)} files"
                continue
            if s.id in held:
                h = held[s.id]
                st = State(tuple(h.tiers), h.verdict, len(h.reviews), h.wait)
                seen[s.id] = st if not on_disk else "held but its files are in the project"
                continue
            try:
                outs = list(c.await_decision(s.id, token=s.token, timeout=10))
            except escrow.EscrowRpcError as e:
                # Decided, or reopened, with no history entry: what the project shows.
                applied = (
                    "committed" if on_disk else "returned" if d.upper(s.id).exists() else "gone"
                )
                seen[s.id] = f"unrecorded {applied}: {e.code().name}"
                continue
            status = {
                pb.OUTCOME_STATUS_COMMITTED: "committed",
                pb.OUTCOME_STATUS_RETURNED: "returned",
                pb.OUTCOME_STATUS_DISCARDED: "discarded",
            }.get(outs[-1].status, f"status {outs[-1].status}")
            if (status == "committed") != bool(on_disk):
                seen[s.id] = f"{status} with {len(on_disk)} files in the project"
            elif status == "returned" and not d.upper(s.id).exists():
                seen[s.id] = "returned but its scope is gone"
            else:
                seen[s.id] = State(final=status)
    return seen


def check(d: Daemon, scopes: list[Scope]) -> tuple[list[str], str]:
    bad = []
    seen = observe(d, scopes)
    flight = next((s for s in scopes if s.maybe), None)
    inflight = "none"
    if flight is not None:
        inflight = "landed" if seen[flight.id] == flight.maybe[1] else "lost"
        # The verdict in flight took effect, but the kill came before its history entry.
        got, after = seen[flight.id], flight.maybe[1].final
        applied = {"committed": "committed", "returned": "returned"}.get(after, "gone")
        if isinstance(got, str) and got.startswith(f"unrecorded {applied}:"):
            inflight = "unrecorded"
            bad.append(f"{flight.id}: its verdict applied, but no decision kept")
            seen[flight.id] = flight.maybe[1]
    for s in scopes:
        ok = s.maybe or [s.state]
        got = seen[s.id]
        if got not in ok:
            bad.append(f"{s.id} {s.dir}: got {got}, want one of {ok}")
    if leftovers(d.project):
        bad.append(f"leftovers {leftovers(d.project)}")
    # A session behind a scope held with a wait still waits.
    with escrow.connect(str(d.socket)) as c:
        for session in ("a", "b"):
            blocking = [
                s
                for s in scopes
                if s.session == session
                and isinstance(seen[s.id], State)
                and not seen[s.id].final
                and seen[s.id].wait  # ty: ignore[possibly-missing-attribute]
            ]
            try:
                o = c.open_scope(session=session, timeout=0.5)
                c.discard(o.scope_id)
                if blocking:
                    bad.append(f"session {session} opened past held {blocking[0].id}")
            except escrow.EscrowRpcError as e:
                if not blocking:
                    bad.append(f"session {session} blocked with nothing held: {e.code().name}")
        # Every scope still held can still be decided: its last tier keeps the verdict.
        with escrow.connect_reviewer(str(d.socket)) as rv:
            for h in rv.list_held():
                o = rv.review(h.scope_id, h.tiers[-1], h.verdict, timeout=60)
                s = next(s for s in scopes if s.id == h.scope_id)
                status = {COMMIT: pb.OUTCOME_STATUS_COMMITTED, RETURN: pb.OUTCOME_STATUS_RETURNED}
                want = status.get(h.verdict, pb.OUTCOME_STATUS_DISCARDED)
                there = (d.project / next(iter(files(d.project, s.dir)))).exists()
                if o.status != want or there != (want == pb.OUTCOME_STATUS_COMMITTED):
                    bad.append(f"{h.scope_id}: the last tier gave {o.status}, want {want}")
    return bad, inflight


def one(bin: Path, kill_after: float | None, double: bool, rng_seed: int):
    work = make_runtime_dir()
    d = Daemon(bin, work, policy=POLICY)
    for top in ("src/auth", "docs"):
        (d.project / top).mkdir(parents=True)
        (d.project / top / "base.txt").write_text("base\n")
    with escrow.connect(str(d.socket)) as c:
        scopes = hold_all(d, c)
    stop = threading.Event()
    t = threading.Thread(target=decide_all, args=(d, scopes, random.Random(rng_seed), stop))
    start = time.monotonic()
    t.start()
    if kill_after is not None:
        time.sleep(kill_after)
        d.proc.send_signal(signal.SIGKILL)
        d.proc.wait()
        d.log.close()
        umount(d)
        t.join()
        if double:
            p = spawn_daemon(d)
            time.sleep(random.uniform(0, 0.05))
            p.send_signal(signal.SIGKILL)
            p.wait()
            umount(d)
        d = Daemon(bin, work, policy=POLICY)
    else:
        t.join()
    took = time.monotonic() - start
    stop.set()
    held = sum(1 for s in scopes if not s.state.final)
    inflight = "none"
    try:
        bad, inflight = check(d, scopes)
    except Exception as e:  # noqa: BLE001
        bad = [f"check failed: {e!r}"]
    d.stop()
    if not bad:
        shutil.rmtree(work, ignore_errors=True)
    return bad, took, held, inflight, work


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("runs", type=int, nargs="?", default=100)
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument(
        "--bin", default=os.environ.get("ESCROW_BIN", ROOT / "target" / "debug" / "escrow")
    )
    a = ap.parse_args()
    seed_ = a.seed if a.seed is not None else random.randrange(1 << 30)
    random.seed(seed_)
    bin = Path(a.bin)
    host = subprocess.run(
        ["sh", "-c", ". /etc/os-release; echo $PRETTY_NAME"], capture_output=True, text=True
    )
    where = f"{host.stdout.strip()}, kernel {os.uname().release}"
    print(f"held soak: {a.runs} runs, {len(SCOPES)} scopes, seed {seed_}, {bin}, {where}")
    bad, took, _, _, work = one(bin, None, False, random.randrange(1 << 30))
    if bad:
        print(f"calibration failed, kept {work}:", *bad, sep="\n  ")
        return 1
    print(f"calibration: the reviews took {took:.3f} s")
    fails = held_at_kill = 0
    flights = {"none": 0, "landed": 0, "lost": 0, "unrecorded": 0}
    for i in range(1, a.runs + 1):
        delay = random.uniform(0, took * 1.1)
        double = random.random() < 0.25
        bad, _, held, inflight, work = one(bin, delay, double, random.randrange(1 << 30))
        held_at_kill += held
        flights[inflight] += 1
        fails += bool(bad)
        how = f"kill_after={delay * 1000:.0f}ms double={'yes' if double else 'no'}"
        how += f" held={held} in_flight={inflight}"
        print(f"run {i}: {'fail' if bad else 'ok'} {how}", flush=True)
        for b in bad:
            print(f"  {b}")
        if bad:
            print(f"  kept {work}")
    print(
        f"result: {a.runs} runs, {held_at_kill} scopes held at a kill, a call in flight at"
        f" {sum(flights.values()) - flights['none']} kills ({flights['landed']} took effect,"
        f" {flights['lost']} did not, {flights['unrecorded']} took effect with no history"
        f" entry), {fails} failed"
    )
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
