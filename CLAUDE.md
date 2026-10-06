# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Phase 3 (held decisions: a post-approval by independent reviewers, negotiated waiting, sessions, `escrow review`; then the protocol freeze) is in progress: `docs/phase-3-held-decisions.md` (3.1 review rules and software write rules built Oct 6, 2026; 3.2 next; a runnable model in `examples/held-decisions/model.py`); phase 4 (TypeScript SDK) is planned in `docs/phase-4-typescript-sdk.md`. Phases renumbered Oct 5, 2026: held decisions moved ahead of the freeze. Phase 2 (hardening) is complete (Oct 5, 2026, PR #25): crash soak (1,000 runs, 0 partial; 100 per Lima host), real-repo speed (express A 1.53× accepted within noise, express B 1.47×, attrs 1.09× and 1.06×; workload C reported with no target), path roots for `$HOME` and `/tmp`, change set diff, unscoped escapes and typed errors, scope token and threat model, conflict policy; known limits are in `LIMITATIONS.md`. Phase 1 (CLI POC) is complete: sub-phases 1.1 to 1.7 are done and the exit criteria are met (10 consecutive CI runs per runner, the Lima host matrix, zero misattributed ledger entries); the Python SDK (`escrow.init`, `escrow.scope`) captures in-process IO and subprocesses; the daemon serves scope views over FUSE (each scope reads its snapshot of the base), the open, close (stop children + freeze + change set), commit (journal, conflict check, rollback), discard, return and settle_unscoped calls, and the exec socket; `escrow run` sandboxes an app with the unscoped mode over the project. Phase 0 is complete (`spikes/results/phase-0-report.md`); its throwaway spikes stay in `spikes/`, a separate Cargo workspace.

- `LIMITATIONS.md`: every known limit of escrowd as built and where it is tracked; update it when a limit is found or lifted.
- `docs/escrowd-proposal.md`: the design (scopes, escrow, FUSE capture, bwrap isolation, threat model, roadmap phases 0–9, renumbered Oct 5, 2026).
- `docs/phase-3-held-decisions.md`: the active plan (draft). Sub-phases 3.1–3.7: review rules, held scopes and sessions, process attribution, the reviewer role and `escrow review`, the Python SDK, the protocol freeze, exit runs; plus open questions. Update it when a decision is made or an open question closes (tick the box, add the date).
- `docs/phase-4-typescript-sdk.md`: the next plan (draft). Sub-phases 4.1–4.5: an nx workspace for the whole repo, the TypeScript SDK (TypeScript 7, ESM, Node 22 and 24, oxlint, oxfmt, Vitest, `@grpc/grpc-js`), the conformance suite ported to TypeScript, exit runs.
- `docs/phase-2-hardening.md`: the completed phase 2 plan (sub-phases 2.1–2.8 with as-built notes, exit runs Oct 5, 2026).
- `docs/phase-1-poc.md`: the completed phase 1 plan (sub-phases 1.1–1.7 with as-built notes, exit criteria met Oct 4, 2026).
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
- **Inode numbers**: `scope idx << 48 | lower st_ino` for lower-backed entries (stable through copy-up and rename); upper-only entries `scope idx << 48 | 1 << 47 | counter`. Inodes that cannot be derived from the base path (upper-only, renamed) are pinned in the scope's store, so they survive a daemon restart; pins of base inodes (renames) are written at once, upper-only pins with the next flush (a killed daemon renumbers those, which no process can see across the dead mount). A path-keyed inode table must keep an inode alive while any hard link names it (git links then unlinks temp objects).
- **Flush before decision = fsync every open file of the scope.** `syncfs` on a plain FUSE mount does not wait for the daemon (measured incomplete in 3 of 10 runs); `fsync` and `close` do.
- **One mount, many scopes**: inode numbers are per scope (scope index << 48 | lower st_ino), or the kernel shares page cache between scopes.
- **Overhead (0.6)**: plain FUSE meets 1.5× on the test suite and agent pipeline; cold metadata/read-heavy operations cost 5–30× per operation in any FUSE (bindfs too). No root helper needed.
- Package stores (pnpm) are writable shared state outside the project; pnpm 12 fails offline with a read-only store.
- **Snapshots (0.5)**: copy/reflink snapshots cost 13–23 µs per entry per scope open (reflink saves space, not time); btrfs subvolume snapshots are 8 ms. Decided: pre-images at commit + per-file version check at commit; btrfs subvolume snapshot as an optional fast path.
- **AgentFS is rejected (0.7)**: its OverlayFS rename leaves inode maps stale (breaks git), 246 crates, Turso beta. escrowd builds its own overlay from the spike 0.3 lineage.
- Minimum kernel 6.8 (Ubuntu 24.04 GA). WSL2 is the dev host only; targets are general Linux, then macOS (phase 8).

## Layout and commands

- `crates/escrowd/`: daemon library. `roots.rs` (path roots: project, `$HOME`, `/tmp`; view names `<id>`, `<id>.home`, `<id>.tmp`; per-path rules and the stubs and hidden paths of each root), `views.rs` (copy-on-write overlay per scope and root, routing by path, inode table, keep-cache per base version, dropped scopes deleted in the background), `fuse.rs` (fuser adapter: 60 s entry, attribute and negative caching and cached listings for snapshot scopes, READDIRPLUS, no FLUSH), `store.rs` (per-scope SQLite behind a read-write lock: whiteouts, opaque dirs, base versions, pinned inodes, the SHA-256 of the scope token; first reads and upper-only pins written behind), `sys.rs` (fd-relative syscalls via rustix; no `/proc/self/fd` paths), `changeset.rs` (net change set of a closed scope, renames from pinned inodes), `diff.rs` (the change set's diff in git's format against the snapshot, built in the RPC close; binary, over-cap and `read.deny` files summarized; policy `diff:` caps), `snapshot.rs` (base generations: pre-images that keep each scope's snapshot), `journal.rs` (`<state>/journal.sqlite`, entries tagged by root), `commit.rs` (one generation over every root; conflict check (reads too with `conflict.reads`), apply with an editor-race re-check: new files linked from the upper, unchanged renames renamed in the base; rollback, recovery at start), `fault.rs` (`ESCROWD_FAULT`, debug builds; `race:<n>` plays an editor), `exec.rs` (exec socket `<socket>.exec`: SCM_RIGHTS stdio, scope children; close sends SIGTERM, waits `close.grace_ms`, then SIGKILLs), `sandbox.rs` (bwrap args: deny-by-default binds applied shallowest first, root views over `$HOME` and `/tmp`, `roots.other.read` read-only and passthrough paths read-write outside escrow, hides state/views/sockets), `gate.rs` (read gate; `Globs`, the policy's path patterns), `review.rs` (close-time review: `write.deny` and `write.deny_content` discard, `review:` gives the tiers a change set needs; Decide only tightens and refuses to commit a change set that needs a tier), `policy.rs` (YAML policy, `read.deny`, `roots:`, `close:`, `diff:`, `conflict:`, `review:`, `write:`), `ledger.rs` (percent-encoded paths), `daemon.rs` (startup), `rpc.rs` (gRPC; stubs generated from `proto/` by `build.rs` with vendored protoc).
- `crates/escrow-cli/`: the `escrow` binary (`escrow run --project P --unscoped passthrough|implicit|deny [--on-exit commit|discard] -- cmd`, `escrow exec --scope ID -- cmd` (token in `ESCROW_SCOPE_TOKEN`), `escrow diff [--socket S | --project P] SCOPE` (a closed, undecided scope), `escrow daemon --socket S --project P [--state D] [--mount M] [--policy FILE] [--unscoped MODE]`, `escrow log --state D [SCOPE]`). A scope is `<state>/scopes/<id>/{upper/,meta.sqlite}` (plus `<id>.home/` and `<id>.tmp/` for served roots); its views are `<mount>/<id>/`, `<mount>/<id>.home/`, `<mount>/<id>.tmp/`; commit pre-images live in `<state>/generations[-home|-tmp]/<n>/`.
- `proto/escrow/v1/escrow.proto`: the one protocol schema (gRPC over a Unix socket, `ESCROW_SOCKET`).
- `sdk/python/`: uv project, package `escrow`: `_client.py` (gRPC client), `_sdk.py` (`init` re-exec under `escrow run`, `scope` with decide callbacks, ContextVar path rewrite of `open`/`os.*`, `Popen`, `os.system` and `os.posix_spawn*` → `escrow exec`, `os.chdir` into the view; `EscrowUnscopedError`, `EscrowStaleHandleError`, `EscrowUnscopedWarning`); generated stubs in `src/escrow/v1/` are committed.
- `bench/`: phase 2 benchmark (`run.sh`: workloads A and B on express and attrs, read-heavy C on CPython, natively, in the sandbox and under escrow; `lima.sh`: in the `escrow-bench` VM, `ESCROW=` and `LOG=` pick the binary and log; `summarize.py`; logs in `bench/results/`).
- `tests/soak/`: `crash.py N` (SIGKILL mid-commit, and mid-recovery; 0 partial commits), `close.py N` (closes under a busy writer; close latency) and `lima.sh <host> [N]` (the crash soak in a host-matrix VM); logs in `tests/soak/results/`.
- `tests/conformance/`: pytest suite run against the built binary (`test_app.py` drives `examples/test-app/app.py` through the exit tests and fails on any misattributed ledger entry); `packaging/ubuntu/`: escrowd's bwrap and AppArmor profile.

```bash
cargo build && cargo clippy --all-targets -- -D warnings && cargo fmt --check
sdk/python/gen.sh                                                  # after editing proto/; CI diffs the stubs
uv run --project sdk/python --frozen ruff check && uv run --project sdk/python --frozen ruff format --check
uv run --project sdk/python --frozen ty check --project sdk/python
uv run --project sdk/python --frozen pytest -q tests/conformance  # needs target/debug/escrow (or ESCROW_BIN)
tests/conformance/lima.sh ubuntu-24.04                             # check 9, one host; log in tests/conformance/results/
```

grpcio clients must set `grpc.default_authority` (the SDK uses `localhost`): grpcio sends the socket path as `:authority` and tonic rejects it with RST_STREAM. On Ubuntu, run `packaging/ubuntu/install.sh` once before the suite. CI follows the conventions of the user's other repos (`../celscale`): `.github/workflows/ci.yml` runs on pull requests only (not pushes to main), with a paths-filter `changes` job (a change to `ci.yml` runs every job, a change to `conformance.yml` the suite; other `.github/` files run none), Rust, Python, Conformance (`ubuntu-24.04` and `ubuntu-26.04` on the pinned Python, plus `ubuntu-24.04` on 3.11, the SDK's oldest; from the reusable `conformance.yml`) and Lockfiles jobs, and a `CI Gate` job, required by the `main` ruleset (PR, squash only, linear history, no force push). CI runs the suite once per push (about 3 min a runner); 10 consecutive runs plus the 50-kill crash soak run only on demand, outside the gate (`ci-repeat.yml`, about 13 min a runner). Add the `ci:repeat` label (`gh pr edit N --add-label ci:repeat`; remove and re-add it after a new push) when a PR touches the commit path, FUSE, the sandbox or anything concurrent where a flaky failure would hide, and on the final commit of a sub-phase whose exit criteria need 10 consecutive passes (2.8, check 8); `gh workflow run ci-repeat.yml --ref <branch> [-f runs=N]` does the same without a PR. Do not add it to docs-only or tooling PRs. `pr-labels.yml` applies `area:*` labels from `.github/labeler.yml`; Dependabot raises security alerts only; it opens no PRs (no `dependabot.yml`, security updates off). Rust is pinned in `rust-toolchain.toml` (keep `mise.toml` in sync), Python in `.python-version` (the SDK supports 3.11+: `requires-python`, ruff `target-version` and ty's `python-version` say 3.11, so code must not use newer syntax or APIs); ruff config is the root `ruff.toml`, limited to `*.py` because ruff also formats Python blocks in Markdown. Lint workflows with `actionlint` (pinned in `mise.toml`).

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
