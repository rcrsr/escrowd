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

Phase 4 adds command reviewers. `tools/**` has the reviewer `check`, `ops/**` has `check`
then the delegate `oncall`, both Python stand-ins in the run's work directory. `check`
sleeps 20 to 200 ms and gives a verdict that is a function of the scope id; every run of
either appends `<scope> <reviewer> <attempt>` to `runs.log`; `oncall` answers `pending`
and leaves its token in `tokens/<scope>.oncall`, and the reviewer thread gives the
verdict by token (`review_as`). The expected outcome of each such scope follows from its
id. After the restart the reviews are drained, and then:

- each of these scopes has exactly one `op=decide` in the ledger, with the expected status;
- its files are in the project all or not at all, as that status says;
- a token the ledger showed pending before the kill is accepted after the restart, unless
  the scope was decided;
- the attempts of each (scope, reviewer) in `runs.log` are consecutive. A kill between the
  daemon recording a run and the program starting leaves one number missing per kill.
"""

import argparse
import hashlib
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

import escrow
from escrow import lowlevel
from escrow.v1 import escrow_pb2 as pb
from escrow_testkit import Daemon, isolate_git_env, leftovers, make_runtime_dir, umount

ROOT = Path(__file__).resolve().parents[2]
isolate_git_env()

POLICY = """\
reviewers:
  check: {{run: ['{check}'], timeout: 30s, on_failure: discard}}
  oncall: {{run: ['{oncall}'], delegate: true, timeout: 120s, on_failure: discard}}
review:
  - {{paths: ['src/auth/**'], tier: human}}
  - {{paths: ['src/**'], tier: llm}}
  - {{paths: ['docs/**'], tier: llm, wait: optional}}
  - {{paths: ['tools/**'], reviewers: [check]}}
  - {{paths: ['ops/**'], reviewers: [check, oncall]}}
"""
# Python stand-ins for the command reviewers; @NAME@ marks what `command_reviewers` fills in.
CHECK = """#!@PYTHON@ -I
import hashlib, json, os, sys, time

request = json.load(sys.stdin)
scope, attempt = request["scope"]["id"], request["attempt"]
fd = os.open(@LOG@, os.O_WRONLY | os.O_APPEND | os.O_CREAT)
os.write(fd, f"{scope} check {attempt}\\n".encode())
os.close(fd)
h = int(hashlib.sha256(scope.encode()).hexdigest()[:8], 16)
time.sleep(0.02 + (h >> 4) % 180 / 1000)
verdict = ["commit", "commit", "return", "discard"][h % 4]
print(json.dumps({"verdict": verdict, "reasons": ["check"]}))
"""
ONCALL = """#!@PYTHON@ -I
import json, os, sys

request = json.load(sys.stdin)
scope, attempt = request["scope"]["id"], request["attempt"]
tmp = os.path.join(@TOKENS@, scope + ".tmp")
with open(tmp, "w") as f:
    f.write(os.environ["ESCROW_REVIEW_TOKEN"])
os.replace(tmp, os.path.join(@TOKENS@, scope + ".oncall"))
fd = os.open(@LOG@, os.O_WRONLY | os.O_APPEND | os.O_CREAT)
os.write(fd, f"{scope} oncall {attempt}\\n".encode())
os.close(fd)
print(json.dumps({"verdict": "pending"}))
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


ORDER = {"commit": 0, "return": 1, "discard": 2}
STATUS = {
    "commit": pb.OUTCOME_STATUS_COMMITTED,
    "return": pb.OUTCOME_STATUS_RETURNED,
    "discard": pb.OUTCOME_STATUS_DISCARDED,
}
CMD_FILES = 40
# Command-reviewed scopes: (kind, directory); `tools` has check, `ops` check then oncall.
CMD_SCOPES = [("tools", f"tools/c{i}") for i in range(4)] + [
    ("ops", f"ops/c{i}") for i in range(4, 8)
]
# gRPC codes a killed daemon gives; any other code of a token answer is a failure.
KILLED = {"UNAVAILABLE", "CANCELLED", "DEADLINE_EXCEEDED", "UNKNOWN"}


@dataclass
class CmdScope:
    """A scope reviewed by command reviewers; its outcome follows from its id."""

    id: str
    kind: str  # tools or ops
    dir: str
    token: str

    @property
    def expected(self) -> str:
        """The verdict it ends with: the strictest of its reviewers' (`check` ends the
        review at once with a discard, `oncall` cannot loosen what `check` said)."""
        h = int(hashlib.sha256(self.id.encode()).hexdigest()[:8], 16)
        verdict = ["commit", "commit", "return", "discard"][h % 4]
        if self.kind == "ops" and verdict != "discard":
            verdict = max(verdict, ["commit", "return", "discard"][(h >> 8) % 3], key=ORDER.get)
        return verdict


def files(d: Path, dir: str, count: int = FILES) -> dict[str, str]:
    return {f"{dir}/f{j:02}.py": f"{dir} {j}\n" * 8 for j in range(count)}


def command_reviewers(work: Path) -> str:
    """Write the stand-ins in `work` (outside the project); the policy that names them."""
    (work / "tokens").mkdir()
    fill = {"@PYTHON@": sys.executable, "@LOG@": repr(str(work / "runs.log"))}
    fill["@TOKENS@"] = repr(str(work / "tokens"))
    paths = {}
    for name, script in (("check", CHECK), ("oncall", ONCALL)):
        for key, value in fill.items():
            script = script.replace(key, value)
        paths[name] = work / name
        paths[name].write_text(script)
        paths[name].chmod(0o755)
    return POLICY.format(**paths)


def after_review(s: State, tier: int, verdict: int, over: bool) -> State:
    v = verdict if over else max(s.verdict, verdict, key=RANK.__getitem__)
    # A human's verdict also stands for the llm tier still pending.
    left = () if tier == HUMAN else tuple(t for t in s.tiers if t != tier)
    if left:
        return replace(s, tiers=left, verdict=v, reviews=s.reviews + 1)
    return State(final=FINAL[v])


def hold_all(d: Daemon, c: lowlevel.Client) -> list[Scope]:
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
    rv = d.reviewer()
    with rv:
        for s in out:
            if s.state.tiers == (LLM, HUMAN) and random.random() < 0.5:
                rv.review(s.id, LLM, COMMIT, reasons=["ok"])
                s.state = after_review(s.state, LLM, COMMIT, False)
    return out


def hold_commands(d: Daemon, c: lowlevel.Client) -> list[CmdScope]:
    out = []
    for kind, dir in CMD_SCOPES:
        o = c.open_scope()
        for path, text in files(d.mount, dir, CMD_FILES).items():
            p = d.mount / o.scope_id / path
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        c.close_scope(o.scope_id)
        r = c.commit(o.scope_id)
        assert r.status == pb.OUTCOME_STATUS_HELD, r
        # check may already have given its verdict: the held list is what is left.
        assert list(r.reviewers) == (["check"] if kind == "tools" else ["check", "oncall"]), r
        out.append(CmdScope(o.scope_id, kind, dir, o.token))
    return out


def token_of(d: Daemon, scope: str) -> str | None:
    """The token of the latest run of `oncall` on `scope`, if it has left one."""
    f = d.work / "tokens" / f"{scope}.oncall"
    return f.read_text() if f.exists() else None


def answer_tokens(
    rv: escrow.Reviewer, d: Daemon, cmd: list[CmdScope], errors: list[str], old: dict[str, str]
) -> int:
    """Give the verdict of `oncall` by token for each `ops` scope waiting for it (the
    token pending before a kill, from `old`, first); returns how many command scopes are
    still held."""
    held = {h.scope_id: h for h in rv.list_held()}
    for c in cmd:
        if c.id not in held or c.kind != "ops" or held[c.id].reviewers[:1] != ["oncall"]:
            continue
        fresh = c.id not in old
        token = token_of(d, c.id) if fresh else old.pop(c.id)
        if token is None:
            continue
        try:
            rv.review_as(c.id, "oncall", c.expected, reasons=["oncall"], token=token, timeout=60)
        except escrow.EscrowRpcError as e:
            if e.code().name in KILLED:
                raise
            # A token read from the file may be one a newer run replaced; try again.
            if not fresh:
                errors.append(
                    f"{c.id}: a token pending before the kill was refused: {e.code().name}"
                )
    return sum(1 for c in cmd if c.id in held)


def decide_all(
    d: Daemon,
    scopes: list[Scope],
    cmd: list[CmdScope],
    errors: list[str],
    rng: random.Random,
    stop: threading.Event,
    marks: list[float],
):
    """Review or withdraw held scopes at random until none is held or the daemon dies;
    give the delegates' verdicts by token. `marks` gets the time the tiers' scopes are done."""
    try:
        with d.reviewer() as rv, d.client() as c, escrow.Reviewer(f"{d.socket}.review") as sdk:
            while not stop.is_set():
                held = [s for s in scopes if not s.state.final]
                if not held and not marks:
                    marks.append(time.monotonic())
                if answer_tokens(sdk, d, cmd, errors, {}) == 0 and not held:
                    return
                if not held:
                    time.sleep(0.02)
                    continue
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
    with d.reviewer() as rv, d.client() as c:
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
    with d.client() as c:
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
        with d.reviewer() as rv:
            ours = {s.id for s in scopes}  # the command-reviewed scopes are drained apart
            for h in (h for h in rv.list_held() if h.scope_id in ours):
                o = rv.review(h.scope_id, h.tiers[-1], h.verdict, timeout=60)
                s = next(s for s in scopes if s.id == h.scope_id)
                status = {COMMIT: pb.OUTCOME_STATUS_COMMITTED, RETURN: pb.OUTCOME_STATUS_RETURNED}
                want = status.get(h.verdict, pb.OUTCOME_STATUS_DISCARDED)
                there = (d.project / next(iter(files(d.project, s.dir)))).exists()
                if o.status != want or there != (want == pb.OUTCOME_STATUS_COMMITTED):
                    bad.append(f"{h.scope_id}: the last tier gave {o.status}, want {want}")
    return bad, inflight


def pending_tokens(d: Daemon, cmd: list[CmdScope]) -> dict[str, str]:
    """The token of each scope whose delegate the ledger shows pending and that is not
    decided yet (read after the daemon is dead)."""
    ledger = d.ledger
    out = {}
    for c in cmd:
        mine = [x for x in ledger if f" scope={c.id} " in x]
        pending = any(" op=review-oncall " in x and "decision=pending" in x for x in mine)
        if pending and not any(" op=decide " in x for x in mine):
            out[c.id] = token_of(d, c.id) or ""
    return out


def check_commands(
    d: Daemon, cmd: list[CmdScope], before: dict[str, str], errors: list[str], kills: int
) -> list[str]:
    """Drain the command reviews after the restart, then check what they left."""
    bad = list(errors)
    old = dict(before)
    deadline = time.monotonic() + 90
    with escrow.Reviewer(f"{d.socket}.review") as rv:
        while answer_tokens(rv, d, cmd, bad, old):
            if time.monotonic() > deadline:
                bad.append("command scopes still held after 90 s")
                return bad
            time.sleep(0.05)
    ledger = d.ledger
    with d.client() as c:
        for s in cmd:
            decides = [x for x in ledger if f" scope={s.id} " in x and " op=decide " in x]
            if len(decides) != 1:
                bad.append(f"{s.id} {s.dir}: {len(decides)} op=decide lines, want 1")
            try:
                got = list(c.await_decision(s.id, token=s.token, timeout=10))[-1].status
            except escrow.EscrowRpcError as e:
                bad.append(f"{s.id} {s.dir}: no decision kept: {e.code().name}")
                continue
            if got != STATUS[s.expected]:
                bad.append(f"{s.id} {s.dir}: status {got}, want {s.expected}")
            want = files(d.project, s.dir, CMD_FILES)
            on_disk = {p for p in want if (d.project / p).exists()}
            if on_disk != (set(want) if s.expected == "commit" else set()):
                bad.append(f"{s.id} {s.dir}: {len(on_disk)} of {len(want)} files, {s.expected}")
    bad += attempt_gaps(d, cmd, kills)
    return bad


def attempt_gaps(d: Daemon, cmd: list[CmdScope], kills: int) -> list[str]:
    """Attempts per (scope, reviewer) in `runs.log` count up from 1; each kill may cost
    one number, for a run the daemon recorded but the program never started."""
    seen: dict[tuple[str, str], list[int]] = {}
    for line in (d.work / "runs.log").read_text().splitlines():
        scope, reviewer, attempt = line.split()
        seen.setdefault((scope, reviewer), []).append(int(attempt))
    bad = []
    for s in cmd:
        for reviewer in ("check", "oncall") if s.kind == "ops" else ("check",):
            attempts = sorted(seen.get((s.id, reviewer), []))
            if reviewer == "oncall" and not attempts and s.expected == "discard":
                continue  # `check` ended the review
            missing = (attempts[-1] if attempts else 1) - len(set(attempts))
            if not attempts or len(set(attempts)) != len(attempts) or missing > kills:
                bad.append(f"{s.id} {reviewer}: attempts {attempts}, {kills} kills")
    return bad


def one(bin: Path, kill_after: float | None, double: bool, rng_seed: int):
    work = make_runtime_dir()
    d = Daemon(bin, work, policy=command_reviewers(work))
    for top in ("src/auth", "docs", "tools", "ops"):
        (d.project / top).mkdir(parents=True)
        (d.project / top / "base.txt").write_text("base\n")
    with d.client() as c:
        scopes = hold_all(d, c)
        cmd = hold_commands(d, c)
    stop = threading.Event()
    errors: list[str] = []
    marks: list[float] = []
    t = threading.Thread(
        target=decide_all, args=(d, scopes, cmd, errors, random.Random(rng_seed), stop, marks)
    )
    before: dict[str, str] = {}
    start = time.monotonic()
    t.start()
    if kill_after is not None:
        time.sleep(kill_after)
        d.kill()
        t.join()
        before = pending_tokens(d, cmd)
        if double:
            p = d.spawn()
            time.sleep(random.uniform(0, 0.05))
            p.send_signal(signal.SIGKILL)
            p.wait()
            umount(d)
        d.restart()
    else:
        t.join()
    took = time.monotonic() - start
    # The tiers' scopes are done long before the command reviewers are: kill in either span.
    took_tiers = marks[0] - start if marks else took
    stop.set()
    held = sum(1 for s in scopes if not s.state.final)
    inflight = "none"
    try:
        # First: `check` opens scopes of its own, which reuse the ids of decided ones.
        bad = check_commands(d, cmd, before, errors, 0 if kill_after is None else 1 + double)
        more, inflight = check(d, scopes)
        bad += more
    except Exception as e:  # noqa: BLE001
        bad = [f"check failed: {e!r}"]
    d.stop()
    if not bad:
        shutil.rmtree(work, ignore_errors=True)
    return bad, (took, took_tiers), held, inflight, work


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
    bad, (took, took_tiers), _, _, work = one(bin, None, False, random.randrange(1 << 30))
    if bad:
        print(f"calibration failed, kept {work}:", *bad, sep="\n  ")
        return 1
    print(f"calibration: the reviews took {took:.3f} s, the tiers' {took_tiers:.3f} s")
    fails = held_at_kill = 0
    flights = {"none": 0, "landed": 0, "lost": 0, "unrecorded": 0}
    for i in range(1, a.runs + 1):
        delay = random.uniform(0, (took if i % 2 else took_tiers) * 1.1)
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
