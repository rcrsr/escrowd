# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

escrowd has no product code yet: the repo holds the design, the phase 0 plan and throwaway spikes (Cargo workspace in `spikes/`). 0.1 to 0.6 are done; 0.7 remains.

- `docs/escrowd-proposal.md`: the design (scopes, escrow, FUSE capture, bwrap isolation, roadmap phases 0–8).
- `docs/phase-0-spikes.md`: the active plan. Sub-phases 0.1–0.7, each with a go/no-go goal, plus host matrix, environment rules and open questions. Update it when a decision is made or an open question closes (tick the box, add the date).

## What escrowd is

A local daemon that captures every filesystem write a host application makes inside a developer-defined **scope**, holds it in escrow, and applies or discards the whole change set atomically when the scope closes. Escrow runs in both directions:

- **Writes, outbound**: held until the scope's single close-time decision; nothing reaches the real filesystem before it.
- **Reads, inbound**: held until the gate releases the data to the caller. The caller is blocked on the result, so each read is decided synchronously, one at a time, and logged for the close-time decision.

Code inside a scope must not be able to tell its writes are pending.

It works with any application, but it is built for LLM agent harnesses: a scope maps to a tool call, a turn or a prompt, and the close-time decision (software policy, LLM auditor, human) can return reasons to the agent so it fixes its own change.

Architecture as planned (proposal "Architecture" and "Isolation"):

- **escrowd** (Rust) owns the FUSE capture layer, scopes, journal, gate and ledger. Language SDKs only mark scopes and talk to it over a Unix socket (gRPC recommended, not final).
- **Attribution is by path**, not thread: each scope gets a virtual root `/escrow/<scope-id>/`; SDKs rewrite project paths using the language's async context (`contextvars`, `AsyncLocalStorage`); subprocesses run in bwrap with the scope's view mounted over `$PROJECT`.
- **Isolation**: the real project directory is never mounted in the sandbox; the FUSE view is the only way to it.

## Verified constraints (do not re-derive or contradict without new evidence)

- **FUSE kernel passthrough needs `CAP_SYS_ADMIN`** in the initial user namespace (`fuse_backing_open()`). The default design is plain FUSE with unprivileged init flags (writeback cache, async read, parallel dirops, cache symlinks, no opendir), as AgentFS does. Passthrough is a root-helper fallback only.
- **Ubuntu 24.04+ AppArmor userns restriction**: an unconfined bwrap gets a capability-less namespace and fails at uid-map setup (measured, `spikes/results/0.1-host-matrix.md`). Plan: ship our own bwrap at a fixed path with an AppArmor profile, and run its children under a capability-denying child profile (a bare `flags=(unconfined) { userns, }` profile leaks to children). Never recommend setting the sysctl to 0.
- **Ubuntu 26.04 confines `fusermount3`** to mountpoints under `$HOME`, `/mnt`, `/run/user/<uid>`, `/media`, `/tmp`; views live under `$XDG_RUNTIME_DIR`.
- Sandboxes use bwrap `--disable-userns` on every host; on Ubuntu the AppArmor child profile blocks nested namespaces as well (both verified in 0.2).
- The daemon must never access its own FUSE view path (deadlock). A daemon restart leaves the sandbox's bind mount stale (ENOTCONN).
- **Inode numbers**: lower-backed entries use the lower st_ino as the FUSE inode (stable through copy-up and rename); upper-only entries use 2^56 and up. A path-keyed inode table must keep an inode alive while any hard link names it (git links then unlinks temp objects).
- **Flush before decision = fsync every open file of the scope.** `syncfs` on a plain FUSE mount does not wait for the daemon (measured incomplete in 3 of 10 runs); `fsync` and `close` do.
- **One mount, many scopes**: inode numbers are per scope (scope index << 48 | lower st_ino), or the kernel shares page cache between scopes.
- **Overhead (0.6)**: plain FUSE meets 1.5× on the test suite and agent pipeline; cold metadata/read-heavy operations cost 5–30× per operation in any FUSE (bindfs too). No root helper needed.
- Package stores (pnpm) are writable shared state outside the project; pnpm 12 fails offline with a read-only store.
- **Snapshots (0.5)**: copy/reflink snapshots cost 13–23 µs per entry per scope open (reflink saves space, not time); btrfs subvolume snapshots are 8 ms. Decided: pre-images at commit + per-file version check at commit; btrfs subvolume snapshot as an optional fast path.
- Minimum kernel 6.8 (Ubuntu 24.04 GA). WSL2 is the dev host only; targets are general Linux, then macOS (phase 7).

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
