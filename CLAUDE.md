# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

escrowd is pre-code. The repo holds the design and the phase 0 plan; no daemon, SDK or Cargo workspace exists yet.

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
- The daemon must never access its own FUSE view path (deadlock). A daemon restart leaves the sandbox's bind mount stale (ENOTCONN).
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

Spike binaries are built on the host and reach VMs via Lima's read-only home mount. Loopback XFS/btrfs volumes for 0.5 live in `~/escrowd-volumes/`, never in the repo.

The 0.6 benchmark is express `v5.2.1`: `git clone`, `pnpm install --frozen-lockfile --offline`, `pnpm test`, with a committed `spikes/06-bench/pnpm-lock.yaml`, a pre-filled store (`pnpm fetch`) and `package-import-method=copy` on every run, native included.
