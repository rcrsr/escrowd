# Phase 0 go/no-go report: go for phase 1

Oct 3, 2026. Plan: [docs/phase-0-spikes.md](../../docs/phase-0-spikes.md). Every sub-phase reads go or has a decision; phase 1 (CLI POC) can start.

| # | Sub-phase | Verdict | Evidence |
| --- | --- | --- | --- |
| 0.1 | Environment and stack | go | [host matrix](0.1-host-matrix.md): Ubuntu 24.04 / 26.04, Debian 13, Fedora 44 |
| 0.2 | FUSE in bwrap over `$PROJECT` | go | [14 / 14 on every host](0.2-summary.md); Ubuntu needs escrowd's bwrap + AppArmor profile |
| 0.3 | Copy-on-write semantics | go | [14 / 14 on every host](0.3-summary.md); base byte-identical, matches native incl. git |
| 0.4 | Per-scope routing and read gating | go | [17 / 17 on every host](0.4-summary.md); contextvars shim, EACCES gate, fsync flush |
| 0.5 | Snapshot at open | go, mechanism accepted | [timings and isolation](0.5-summary.md): pre-images at commit + per-file version check |
| 0.6 | Overhead | go for plain FUSE | [benchmark](0.6-summary.md): test suite 1.01–1.16×, agent pipeline 1.21–1.25×; no root helper |
| 0.7 | AgentFS embed | rejected | [6 / 14, upstream rename defect](0.7-summary.md); build our own store |

## Constraints phase 1 inherits

1. **Ubuntu**: ship escrowd's own bwrap with the `escrowd_bwrap` AppArmor profile (children stacked with a capability-denying profile); views under `$XDG_RUNTIME_DIR` (Ubuntu 26.04 confines `fusermount3`).
2. **Every sandbox** uses bwrap `--disable-userns`.
3. **Inode numbers**: lower st_ino within a scope, scope index in the high bits once one mount serves several scopes; keep an inode alive while any hard link names it.
4. **Flush before decision** = fsync every open file of the scope; `syncfs` is not a barrier on plain FUSE.
5. **Snapshots**: pre-image layer at commit (not built yet: phase 1 must prove it) + per-file version check; btrfs subvolume snapshot as an optional fast path.
6. **Package stores** (pnpm) are writable shared state outside the project.
7. **Performance targets for phase 2**: cold lookups, opens and reads cost 5–30× per operation in any FUSE; use dirfd syscalls instead of `/proc` path walks, READDIRPLUS, finer locks.

## Carried limits from the spikes

Whiteouts live in memory; copy-up copies whole files; scopes are created by `mkdir` instead of the RPC socket; the read rule is hard-coded; upper-only inode numbers are not stable across remounts.
