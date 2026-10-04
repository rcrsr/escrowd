# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Phase 1 (CLI POC) is in progress: sub-phases 1.1 to 1.6 are done, 1.7 (conformance suite, soak, host matrix) is in review; the Python SDK (`escrow.init`, `escrow.scope`) captures in-process IO and subprocesses; the daemon serves scope views over FUSE (each scope reads its snapshot of the base), the open, close (stop children + freeze + change set), commit (journal, conflict check, rollback), discard, return and settle_unscoped calls, and the exec socket; `escrow run` sandboxes an app with the unscoped mode over the project. Phase 0 is complete (`spikes/results/phase-0-report.md`); its throwaway spikes stay in `spikes/`, a separate Cargo workspace.

- `docs/escrowd-proposal.md`: the design (scopes, escrow, FUSE capture, bwrap isolation, roadmap phases 0–8).
- `docs/phase-1-poc.md`: the active plan (draft). Sub-phases 1.1–1.7 toward the proposal's seven exit tests, plus open questions. Update it when a decision is made or an open question closes (tick the box, add the date).
- `docs/phase-0-spikes.md`: the completed phase 0 plan (sub-phases 0.1–0.7, host matrix, environment rules).

## What escrowd is

A local daemon that captures every filesystem write a host application makes inside a developer-defined **scope**, holds it in escrow, and applies or discards the whole change set atomically when the scope closes. Escrow runs in both directions:

- **Writes, outbound**: held until the scope's single close-time decision; nothing reaches the real filesystem before it.
- **Reads, inbound**: held until the gate releases the data to the caller. The caller is blocked on the result, so each read is decided synchronously, one at a time, and logged for the close-time decision.

Code inside a scope must not be able to tell its writes are pending.

It works with any application, but it is built for LLM agent harnesses: a scope maps to a tool call, a turn or a prompt, and the close-time decision (software policy, LLM auditor, human) can return reasons to the agent so it fixes its own change.

Architecture as planned (proposal "Architecture" and "Isolation"):

- **escrowd** (Rust) owns the FUSE capture layer, scopes, journal, gate and ledger. Language SDKs only mark scopes and talk to it over a Unix socket (gRPC: tonic and grpcio; per-scope state in one SQLite file).
- **Attribution is by path**, not thread: each scope gets a virtual root `/escrow/<scope-id>/`; SDKs rewrite project paths using the language's async context (`contextvars`, `AsyncLocalStorage`); subprocesses run in bwrap with the scope's view mounted over `$PROJECT`.
- **Isolation**: the real project directory is never mounted in the sandbox; the FUSE view is the only way to it.

## Verified constraints (do not re-derive or contradict without new evidence)

- **FUSE kernel passthrough needs `CAP_SYS_ADMIN`** in the initial user namespace (`fuse_backing_open()`). The default design is plain FUSE with unprivileged init flags (writeback cache, async read, parallel dirops, cache symlinks, no opendir), as AgentFS does. Passthrough is a root-helper fallback only.
- **Ubuntu 24.04+ AppArmor userns restriction**: an unconfined bwrap gets a capability-less namespace and fails at uid-map setup (measured, `spikes/results/0.1-host-matrix.md`). Plan: ship our own bwrap at a fixed path with an AppArmor profile, and run its children under a capability-denying child profile (a bare `flags=(unconfined) { userns, }` profile leaks to children). Never recommend setting the sysctl to 0.
- **Ubuntu 26.04 confines `fusermount3`** to mountpoints under `$HOME`, `/mnt`, `/run/user/<uid>`, `/media`, `/tmp`; views live under `$XDG_RUNTIME_DIR`.
- Sandboxes use bwrap `--disable-userns` on every host; on Ubuntu the AppArmor child profile blocks nested namespaces as well (both verified in 0.2).
- The daemon must never access its own FUSE view path (deadlock). A daemon restart leaves the sandbox's bind mount stale (ENOTCONN).
- **Inode numbers**: `scope idx << 48 | lower st_ino` for lower-backed entries (stable through copy-up and rename); upper-only entries `scope idx << 48 | 1 << 47 | counter`. Inodes that cannot be derived from the base path (upper-only, renamed) are pinned in the scope's store, so they survive a daemon restart. A path-keyed inode table must keep an inode alive while any hard link names it (git links then unlinks temp objects).
- **Flush before decision = fsync every open file of the scope.** `syncfs` on a plain FUSE mount does not wait for the daemon (measured incomplete in 3 of 10 runs); `fsync` and `close` do.
- **One mount, many scopes**: inode numbers are per scope (scope index << 48 | lower st_ino), or the kernel shares page cache between scopes.
- **Overhead (0.6)**: plain FUSE meets 1.5× on the test suite and agent pipeline; cold metadata/read-heavy operations cost 5–30× per operation in any FUSE (bindfs too). No root helper needed.
- Package stores (pnpm) are writable shared state outside the project; pnpm 12 fails offline with a read-only store.
- **Snapshots (0.5)**: copy/reflink snapshots cost 13–23 µs per entry per scope open (reflink saves space, not time); btrfs subvolume snapshots are 8 ms. Decided: pre-images at commit + per-file version check at commit; btrfs subvolume snapshot as an optional fast path.
- **AgentFS is rejected (0.7)**: its OverlayFS rename leaves inode maps stale (breaks git), 246 crates, Turso beta. escrowd builds its own overlay from the spike 0.3 lineage.
- Minimum kernel 6.8 (Ubuntu 24.04 GA). WSL2 is the dev host only; targets are general Linux, then macOS (phase 7).

## Layout and commands

- `crates/escrowd/`: daemon library. `views.rs` (copy-on-write overlay per scope, routing by path, inode table), `fuse.rs` (fuser adapter), `store.rs` (per-scope SQLite: whiteouts, opaque dirs, base versions, pinned inodes), `sys.rs` (fd-relative syscalls via rustix; no `/proc/self/fd` paths), `changeset.rs` (net change set of a closed scope, renames from pinned inodes), `snapshot.rs` (base generations: pre-images that keep each scope's snapshot), `journal.rs` (`<state>/journal.sqlite`), `commit.rs` (conflict check, apply, rollback, recovery at start), `fault.rs` (`ESCROWD_FAULT`, debug builds), `exec.rs` (exec socket `<socket>.exec`: SCM_RIGHTS stdio, scope children, close stops them), `sandbox.rs` (bwrap args: deny-by-default binds, hides state/views/sockets), `gate.rs`, `policy.rs` (YAML policy, `read.deny`), `ledger.rs` (percent-encoded paths), `daemon.rs` (startup), `rpc.rs` (gRPC; stubs generated from `proto/` by `build.rs` with vendored protoc).
- `crates/escrow-cli/`: the `escrow` binary (`escrow run --project P --unscoped passthrough|implicit|deny [--on-exit commit|discard] -- cmd`, `escrow exec --scope ID -- cmd`, `escrow daemon --socket S --project P [--state D] [--mount M] [--policy FILE] [--unscoped MODE]`, `escrow log --state D [SCOPE]`). A scope is `<state>/scopes/<id>/{upper/,meta.sqlite}`; its view is `<mount>/<id>/`; commit pre-images live in `<state>/generations/<n>/`.
- `proto/escrow/v1/escrow.proto`: the one protocol schema (gRPC over a Unix socket, `ESCROW_SOCKET`).
- `sdk/python/`: uv project, package `escrow`: `_client.py` (gRPC client), `_sdk.py` (`init` re-exec under `escrow run`, `scope` with decide callbacks, ContextVar path rewrite of `open`/`os.*`, `Popen` → `escrow exec`); generated stubs in `src/escrow/v1/` are committed.
- `tests/conformance/`: pytest suite run against the built binary (`test_app.py` drives `examples/test-app/app.py` through the exit tests and fails on any misattributed ledger entry); `packaging/ubuntu/`: escrowd's bwrap and AppArmor profile.

```bash
cargo build && cargo clippy --all-targets -- -D warnings && cargo fmt --check
sdk/python/gen.sh                                                  # after editing proto/; CI diffs the stubs
uv run --project sdk/python --frozen ruff check && uv run --project sdk/python --frozen ruff format --check
uv run --project sdk/python --frozen ty check --project sdk/python
uv run --project sdk/python --frozen pytest -q tests/conformance  # needs target/debug/escrow (or ESCROW_BIN)
tests/conformance/lima.sh ubuntu-24.04                             # check 9, one host; log in tests/conformance/results/
```

grpcio clients must set `grpc.default_authority` (the SDK uses `localhost`): grpcio sends the socket path as `:authority` and tonic rejects it with RST_STREAM. On Ubuntu, run `packaging/ubuntu/install.sh` once before the suite. CI follows the conventions of the user's other repos (`../celscale`): `.github/workflows/ci.yml` runs on pull requests only (not pushes to main), with a paths-filter `changes` job, Rust, Python, Conformance (`ubuntu-24.04` and `ubuntu-26.04`) and Lockfiles jobs, and a `CI Gate` job to require. `pr-labels.yml` applies `area:*` labels from `.github/labeler.yml`; Dependabot updates actions, cargo and pip weekly, grouped. Rust is pinned in `rust-toolchain.toml` (keep `mise.toml` in sync), Python in `.python-version`; ruff config is the root `ruff.toml`, limited to `*.py` because ruff also formats Python blocks in Markdown. Lint workflows with `actionlint` (pinned in `mise.toml`).

Git hooks: lefthook (`lefthook.yml`, pinned in `mise.toml`); run `lefthook install` once per clone. pre-commit fixes staged files (rustfmt, ruff) and regenerates Python stubs when a `.proto` is staged; pre-push runs cargo fmt, clippy, ruff, ty, the lockfile checks and actionlint. The conformance suite runs only in CI.

## Environment

All tools are pinned in `mise.toml` (Rust, Node, pnpm, Python, uv, Lima). Run `mise install` to set up; use pnpm (not npm) and uv (not pip). Pin exact versions; the user prefers latest stable / latest LTS when choosing them.

## Spikes

Phase 0 spike code goes in `spikes/`, one Cargo workspace member per sub-phase (`02-fuse-bwrap/` … `07-agentfs/`), results in `spikes/results/`. It is throwaway; nothing outside `spikes/` may depend on it.

Host-matrix VMs (Lima, KVM inside WSL2; templates validated with `limactl tmpl validate spikes/lima/*.yaml`):

```bash
limactl start --name=escrow-ubuntu-24.04 spikes/lima/ubuntu-24.04.yaml   # also ubuntu-26.04, debian-13, fedora-44
limactl start --name=escrow-bench spikes/lima/bench-ubuntu-24.04.yaml    # 0.6 benchmark VM
limactl shell escrow-ubuntu-24.04
```

Build spikes with `cd spikes && cargo build --release`; each spike has a `run.sh` that prints PASS/FAIL per check (`spikes/02-fuse-bwrap/run.sh`, with `BWRAP=/usr/lib/escrowd/bwrap` on Ubuntu after `install-ubuntu.sh`). Run in a VM with `limactl shell escrow-<host> $PWD/spikes/<spike>/run.sh` and save output to `spikes/results/`.

Spike binaries are built on the host and reach VMs via Lima's read-only home mount. Lima home mounts (9p `cache=loose` on Ubuntu 26.04, sshfs elsewhere) serve stale or half-updated files after host edits: run `limactl shell escrow-<host> sudo sysctl -q vm.drop_caches=3` before every run. Loopback XFS/btrfs volumes for 0.5 live in the benchmark VM (`/var/escrowd-volumes/`, mounted at `/mnt/escrow-{xfs,btrfs}` by `spikes/05-snapshot/setup-volumes.sh`), never in the repo.

The 0.6 benchmark: `TOOLS_PATH="$(dirname $(mise which node)):$(dirname $(mise which pnpm))" RUNS=5 MODES="native fuse" spikes/06-bench/run.sh > log; python3 spikes/06-bench/summarize.py < log`. Run long benchmarks with `nohup … &` and wait on the PID (`kill -0`), not `pgrep -f`, which matches its own command line.
