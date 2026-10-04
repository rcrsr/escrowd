# Phase 1: CLI POC Plan

Oct 3, 2026 · Andre Bremer · Draft

**Status, Oct 3, 2026: 1.1 done** (12 / 12 checks on the dev host and in CI on `ubuntu-24.04` and `ubuntu-26.04`, [PR #1](https://github.com/rcrsr/escrowd/pull/1)). **1.2 done**, Oct 4, 2026: 52 / 52 checks on the dev host (5 repeat runs clean) and in CI on both runners, including the 0.3 (14) and 0.4 (17) spike checks ported to the daemon ([PR #3](https://github.com/rcrsr/escrowd/pull/3)). **1.3 done**, Oct 4, 2026: 66 / 66 checks on the dev host (5 repeat runs clean) and in CI on both runners ([PR #5](https://github.com/rcrsr/escrowd/pull/5)). **1.4 done**, Oct 4, 2026: 78 / 78 checks on the dev host (5 repeat runs clean) and in CI on both runners ([PR #6](https://github.com/rcrsr/escrowd/pull/6)). **1.5 done**, Oct 4, 2026: 96 / 96 checks on the dev host (5 repeat runs clean) and in CI on both runners ([PR #7](https://github.com/rcrsr/escrowd/pull/7)). **1.6 done**, Oct 4, 2026: 107 / 107 checks on the dev host (5 repeat runs clean) and in CI on both runners ([PR #8](https://github.com/rcrsr/escrowd/pull/8)). **1.7 done**, Oct 4, 2026: 121 / 121 checks in 10 consecutive runs on the dev host and on each CI runner, and once on each Lima host ([PR #9](https://github.com/rcrsr/escrowd/pull/9)). **Phase 1 exit criteria met.**

Phase 1 delivers escrowd, a minimal Python SDK and a CLI test app. Together they prove the scope model from the [escrowd proposal](escrowd-proposal.md) on Linux. Phase 0 cleared every mechanic this phase relies on ([go/no-go report](../spikes/results/phase-0-report.md)). Phase 1 turns the spikes into product code and adds the parts phase 0 never built: the RPC socket, the journal, commit, conflicts, the launcher and unscoped modes.

## Exit criteria

From the proposal: **all seven exit tests pass in CI on Linux (`ubuntu-24.04` and `ubuntu-26.04` runners) in 10 consecutive runs, with zero misattributed operations in the ledger.** The tests become the shared conformance suite that phases 3 and 5 port to other SDKs.

1. Writes, reads, renames and deletes inside a scope leave the real project unchanged until the scope commits.
2. A write by a subprocess (`bash -c "echo x > f"`) is captured in the scope that started it.
3. Two concurrent async scopes on one thread each write a file and run `bash -c` to write another; all four writes land in the right scopes.
4. A discard leaves the project exactly as it was; a commit applies every change or none.
5. Two scopes write the same path: the second to commit hits the conflict policy.
6. A denied read (`.env`) fails with EACCES and appears in the ledger.
7. Each `unscoped` mode behaves as specified.

This plan adds two checks, as phase 0 added 0.4 and 0.5 (accepted Oct 3, 2026):

8. **Snapshot at open.** A scope opened before another scope commits keeps reading the base as it was when it opened. The 0.5 decision made the pre-image layer a phase 1 deliverable.
9. **Host matrix.** The suite passes once on each phase 0 Lima host (Ubuntu 24.04, Ubuntu 26.04, Debian 13, Fedora 44), since CI covers only Ubuntu.

"Zero misattributed operations" is checked mechanically. Every test operation writes a path and content tagged with its scope; the suite fails if any ledger entry names a scope that did not perform it.

## Scope shift from the proposal

The proposal lists stale-handle errors, unscoped-mode errors, writeback flush and snapshot at open under phase 2. Exit tests 4, 5 and 7 and check 8 need a minimal version of each now; phase 2 hardens them:

| Item | Phase 1 | Phase 2 |
| --- | --- | --- |
| Unscoped modes | All three modes work; `deny` surfaces raw EROFS | `EscrowUnscopedError` pointing at the caller |
| Flush before decision | fsync of the SDK's open files, then wait for the scope's sandboxes to exit | Same, measured under crash and load |
| Snapshot at open | Pre-image layer plus per-file version check | Optional btrfs subvolume fast path; performance |
| Stale handles | Daemon refuses writes on handles of closed scopes (EBADF) | `EscrowStaleHandleError` in each SDK |
| Crash recovery | Journal written and replayed on daemon start; tested by hand | 1,000 killed runs, zero partial commits |

## Sub-phases

```mermaid
flowchart LR
    S1["1.1 Skeleton,<br/>protocol, CI"] --> S2["1.2 Overlay store<br/>and router"]
    S2 --> S3["1.3 Scope lifecycle,<br/>gate, ledger"]
    S3 --> S4["1.4 Journal, commit,<br/>conflicts, pre-images"]
    S1 --> S5["1.5 Launcher, sandbox,<br/>unscoped modes"]
    S2 --> S5
    S3 --> S6["1.6 Python SDK"]
    S5 --> S6
    S4 --> S7["1.7 Conformance suite<br/>and soak"]
    S6 --> S7
```

1.5 starts once 1.2 is done and runs in parallel with 1.3 and 1.4.

| # | Sub-phase | Goal (done when…) | Exit tests |
| --- | --- | --- | --- |
| 1.1 | Skeleton, protocol and CI (**done**, Oct 3, 2026) | Workspace builds; the protocol schema exists; a Python client completes a `ping` round trip to the daemon over the Unix socket; CI on `ubuntu-24.04` and `ubuntu-26.04` installs escrowd's bwrap and AppArmor profile and runs an empty suite | None |
| 1.2 | Overlay store and router (**done**, Oct 4, 2026) | One FUSE mount serves many scopes, created and dropped over RPC instead of `mkdir`; whiteouts, opaque directories and the per-scope version table persist in each scope's store; the 0.3 (14) and 0.4 (17) spike checks pass when ported to the new daemon | 1 (staging half) |
| 1.3 | Scope lifecycle, gate and ledger (**done**, Oct 4, 2026) | `open_scope`, `close_scope` and `decide` work end to end: close flushes and returns the change set (diff, reads, labels); discard drops the store; read rules load from a policy file; every operation lands in the ledger with its scope | 1, 6 |
| 1.4 | Journal, commit, conflicts, pre-images (**done**, Oct 4, 2026) | Commit applies the whole change set through the journal, or none of it if any step fails; a changed base file triggers the conflict policy (default discard); open scopes keep their snapshot through the pre-image layer; a fault injected at each apply step leaves the base byte-identical; the daemon rolls back an unfinished journal on start | 4, 5, 8 |
| 1.5 | Launcher, sandbox, unscoped modes (**done**, Oct 4, 2026) | `escrow run --project P -- cmd` starts the daemon and runs `cmd` in bwrap with the unscoped view over `$PROJECT`; the spawn helper starts a scope's child in its own sandbox with ordinary `Popen` semantics; `passthrough`, `implicit` and `deny` behave as the proposal's table says; sandboxes hide the state directory, other scopes' views, the socket (from scope children) and everything outside the bind list | 2, 7 |
| 1.6 | Python SDK (**done**, Oct 4, 2026) | `escrow.init`, `escrow.scope(...)` as an async context manager and decide callbacks work; scopes live in `contextvars`; wrapped `open`, `os.*`, `pathlib` and `subprocess` attribute IO by path; `s.outcome` reports status, paths and reasons | 3, 6 |
| 1.7 | Conformance suite and soak | The test app and the nine checks above run as one pytest suite; CI passes it 10 consecutive times; each Lima host passes it once | All |

## Design for each sub-phase

### 1.1 Skeleton, protocol and CI

Layout, all outside `spikes/` (which nothing may depend on):

```
Cargo.toml                 # root workspace; exclude = ["spikes"]
crates/escrowd/            # library + daemon: fuse, router, store, journal, gate, ledger, rpc
crates/escrow-cli/         # `escrow` binary: run, daemon, exec (spawn helper), log, diff
proto/escrow.proto         # the one protocol schema (gRPC)
packaging/ubuntu/          # bwrap copy + escrowd_bwrap AppArmor profile, from spike 0.2
sdk/python/                # uv project, package `escrow`
tests/conformance/         # pytest suite (exit tests 1–7, checks 8–9)
examples/test-app/         # CLI test app doing regular IO
```

The protocol is gRPC over the Unix socket (tonic in the daemon, grpcio in Python) and carries six calls: `ping`, `open_scope`, `close_scope` (flush, return the change set), `decide` (commit, discard or return to agent), `spawn` and `settle_unscoped`. The daemon socket path reaches children in `ESCROW_SOCKET`.

### 1.2 Overlay store and router

The store ports spike 0.4's router (itself built on 0.3's overlay), keeping every verified constraint: lower st_ino within a scope with the scope index + 1 in bits 48 and up, hard-link-safe inode table, pre-opened base fd, and the unprivileged init flags. New in phase 1:

- Each scope gets a directory under `$XDG_STATE_HOME/escrowd/<project-id>/scopes/<scope-id>/` holding its upper tree and a SQLite metadata store (rusqlite).
- The metadata store records whiteouts, opaque directories and the base version (inode, size, mtime, ctime) of each path the scope read or changed. The version table feeds the conflict check in 1.4.
- Upper-only inode numbers are allocated from a counter kept in the store, so they survive a daemon restart (a carried limit of the spikes).
- The base is reached through dirfd syscalls (`openat`, `fstatat`) instead of `/proc/self/fd/<fd>/<rel>` paths. Phase 0 named this a phase 2 speed fix; doing it now avoids porting the `/proc` code twice.

### 1.3 Scope lifecycle, gate and ledger

`close_scope` runs these steps in order:

1. The SDK fsyncs the scope's open files, then calls `close_scope` (`syncfs` is not a barrier on plain FUSE).
2. The daemon stops the scope's sandboxes and waits for them to exit; `close` on exit flushes their writes.
3. The daemon freezes the scope: it rejects new opens and writes until the decision.
4. The daemon builds the change set from the scope's store and ledger, then returns it.

`decide` then commits (1.4), discards (drop the store) or returns reasons to the caller and reopens the scope. Hold needs escalation and waits for phase 6. The read gate takes deny globs from a YAML policy file instead of the spike's hard-coded `.env` rule. Ledger lines keep the spike format (`scope=… op=… path=… decision=…`) in an append-only file next to the scope stores, readable by `escrow log`.

As built (Oct 4, 2026):

- **Freeze**: a closed scope answers new opens, creates and changes with EROFS; a handle opened before close gets EBADF when its data reaches the daemon (with the writeback cache, at fsync or close). Metadata reads (lookup, getattr, readdir) still work. The closed state persists across a daemon restart. Close is idempotent; discard works on open and closed scopes; return needs a closed scope (FAILED_PRECONDITION otherwise).
- **Change set**: the net effect on the base, from the upper tree, whiteouts and opaque directories. An unchanged copy-up is no change; a deleted directory lists its base descendants as deletes; a create or modify that carries a pinned base inode, matched by the delete of the file that had it, is a rename. Reads are the base files the scope read (allowed) plus the gate's denials; labels come from `open_scope`.
- **Policy file** (`--policy`): `version: 1`, `read.deny` globs; unknown keys or versions stop the daemon. Parsed with serde-saphyr (serde_yaml is deprecated).
- **Ledger**: paths are percent-encoded (non-printable bytes, space, `=`, `%`), so lines split on spaces; `open`, `close`, `decide`, `list` and `readlink` are logged besides reads and changes.

### 1.4 Journal, commit, conflicts, pre-images

Commits are serialized per project: one commit lock, so two scopes never pass the conflict check together. Commit order for one scope:

1. **Conflict check**: compare each recorded base version with the current base file; any mismatch applies the conflict policy (default: discard, with the paths as reasons).
2. **Journal**: write the intent (every path, its new content location and its pre-image) to the journal, then fsync it.
3. **Pre-images**: before each base file is overwritten or deleted, copy its old version into the base generation layer; a path the commit creates gets an absent marker. Earlier-opened scopes read through the generation, and rollback restores from it.
4. **Apply**: write each new file to a temporary name beside its target (recorded in the journal), fsync, then `rename` over the target; apply deletes and directory renames.
5. **Mark done**: record completion in the journal, then drop the scope's store.

If a step fails, the journal rolls the applied part back from the pre-images and removes leftover temporary files. On start, the daemon finds unfinished journals and rolls them back the same way. A generation is dropped once no open scope still reads through it.

The version check cannot stop an editor outside escrowd from writing a file between the check and the rename; phase 1 accepts that window.

As built (Oct 4, 2026):

- **Generations**: each commit is a numbered generation. Each scope records the generation it opened at (`since`). It resolves a path through the first later generation that recorded it, else through the live base. A commit records every path it touches, including each descendant of a deleted directory, so resolving one path at a time is complete. A pre-image keeps the original's inode number, mode, size and times, so inode numbers and recorded versions stay stable. Generations live in `<state>/generations/<n>/`. A generation is dropped once no open scope opened before it.
- **Journal**: `<state>/journal.sqlite` with `synchronous = FULL`. A generation moves from `journaled` (paths, original metadata, temporary names, parent directory times) to `prepared` (pre-images copied, `syncfs`) to `done`. Commit then drops the scope. Start-up rolls back every generation not marked `done`. It finishes a `done` generation whose scope still exists, then clears the scope id so that a later scope reusing the id is not dropped.
- **Conflict check**: it runs on every path the commit touches, under one commit lock per daemon. The expected version is the one the scope recorded, else its snapshot's version (absent for a create). A directory being deleted must not have gained entries, and the parent of each new entry must still be a directory. Directories compare full versions, so a scope that changes a directory's mode conflicts with any commit that adds or removes entries in it. On conflict, nothing is written, the scope is dropped, and the outcome lists the paths as reasons. The policy file has no conflict setting yet.
- **Apply**: deletes run deepest first; this includes entries replaced by a different type. New entries follow, parents first. A file or symlink is written to `.escrow-<gen>-<i>.tmp` beside its target, fsynced and renamed over it. Content is copied, not renamed, so a renamed file gets a new inode in the project.
- **Rollback**: it removes what the commit put in place and keeps originals the commit never reached (same version). It restores the rest from the pre-images, removes the temporary files and resets directory times. A restored copy has a new inode and ctime, so the journal records an alias (old version → new version). Scopes that recorded the original then don't conflict. A failed commit returns ABORTED and leaves the scope closed for a retry or a discard.
- **Fault injection** (debug builds): `ESCROWD_FAULT=<point>:<n>[:abort]` fails at step `n` of `journal`, `preimage`, `apply`, `done` or `committed`. The suite hits every step of a 13-path commit, both in place and as a daemon crash. Each time it checks that the project is byte-identical (content, mode, mtimes), that no temporary files remain, and that a retry after restart applies the commit.
- Also fixed: copies keep the exact mode (the process umask no longer applies). A file or symlink that replaces a base directory now lists the directory's base contents as deletes.

### 1.5 Launcher, sandbox and unscoped modes

`escrow run` starts one daemon for its project and stops it on exit. The daemon mounts its FUSE view under `$XDG_RUNTIME_DIR/escrowd/` (Ubuntu 26.04 confines `fusermount3`). The app runs in bwrap with `--disable-userns`, `--unshare-pid` and `--die-with-parent`. On Ubuntu it uses escrowd's bwrap and AppArmor profile from spike 0.2.

The app's sandbox sees two bind mounts from the view:

| Mount inside the sandbox | Serves |
| --- | --- |
| `$PROJECT` | The unscoped root: real IO for `passthrough` (logged as unscoped), the default scope for `implicit` (decided at exit or by `settle_unscoped`), read-only (EROFS) for `deny` |
| `/escrow` | One directory per open scope, the targets of the SDK's path rewrite |

Subprocesses need a second sandbox per scope, but `--disable-userns` stops the app from running bwrap itself. The wrapped `Popen` therefore runs `escrow exec --scope <id> -- cmd` as the real child. `escrow exec` sends its stdin, stdout and stderr over `SCM_RIGHTS`, plus its cwd and environment, to the daemon. The daemon starts `cmd` in bwrap with the scope's view over `$PROJECT`. `escrow exec` relays signals and exits with the child's status; the daemon kills the child if `escrow exec` dies. Pipes, exit codes and `kill` keep working because the caller holds a real child process.

**Sandbox containment** (added Oct 4, 2026). The test sandboxes use `--ro-bind / /`, which hides only the project. Everything else stays readable, including escrowd's own state. The 1.5 sandboxes close four gaps:

| Gap with `--ro-bind / /` | Requirement | Conformance check |
| --- | --- | --- |
| The state directory is readable: other scopes' upper trees, pre-images, journal and ledger | A `--tmpfs` covers the state directory in every sandbox | A file staged in scope A cannot be found from scope B's child, by any path |
| The whole view mount is reachable: every scope's view | A `--tmpfs` covers `$XDG_RUNTIME_DIR/escrowd`. A scope child gets only its own view, over `$PROJECT`. The app gets the unscoped root and `/escrow` | Scope B's child cannot list or open `<mount>/<A>/`. Writing there fails |
| The daemon socket is reachable, so a child could call `Decide(COMMIT)` on its own scope | Only the app's sandbox binds the socket. `escrow exec` runs in the app's sandbox and reaches the daemon; the command the daemon starts never gets the socket | A scope child's connect to the socket path fails with ENOENT. `Decide` from a child is impossible |
| Reads outside the project (`~/.ssh`, other repositories) bypass the gate | Binds are deny-by-default. System directories (`/usr`, `/etc`, `/bin`, `/lib*`, `/opt`) are bound read-only, plus the paths listed in the policy file's `sandbox.read` (toolchains, package stores). `$HOME` and everything else is absent | A canary file outside the project and outside the bind list fails with ENOENT, from both the app and a scope child. A path listed in `sandbox.read` is readable |

In-process code shares the harness's process, and with it the socket. Only subprocesses can be kept from the socket. A tool that must not reach the decision runs as a subprocess.

`--unshare-net` stays opt-in; network capture is phase 8.

As built (Oct 4, 2026):

- **Unscoped root**: for `implicit` and `deny`, the daemon serves a default scope with id `unscoped` at `<mount>/unscoped/`. Its root has a fixed inode number (2), so the app's bind mount stays valid when the scope is replaced. Commit, discard or conflict open a fresh `unscoped` scope, and kernel notifications drop the cached entries. `implicit` is an ordinary scope with a snapshot. `deny` is a read-only scope over the live base: reads go through the gate and the ledger, and every change gets EROFS. `passthrough` binds the real project read-write: not captured, gated or logged ("optionally logged" stays open). `escrow daemon --unscoped` defaults to `passthrough`; `escrow run --unscoped` has no default.
- **`escrow run`**: it hosts the daemon in-process, runs the app in bwrap and settles at exit. At exit, the implicit scope is discarded unless `--on-exit commit` is given; the outcome goes to stderr. The run exits with the app's code. The app sees:
  - the project, as its unscoped mode says;
  - `/escrow` (every scope's view);
  - the sockets at `/run/escrowd/escrow.sock{,.exec}`, with `ESCROW_SOCKET` set;
  - the `escrow` binary, read-only at its host path.

  SIGTERM and SIGHUP go to bwrap. Ctrl-C reaches the app through the terminal while the launcher waits.
- **Exec socket**: gRPC cannot carry file descriptors, so `Spawn` left the gRPC service (protocol version 2). It lives on `<socket>.exec` instead: length-prefixed protobuf frames, with stdio passed by `SCM_RIGHTS`. `escrow exec --scope ID -- cmd` relays SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGUSR1 and SIGUSR2. It exits with the child's code (128 + signal if killed), or 125 if the daemon refuses: unknown or closed scope. Relayed signals go to the command's processes, not to bwrap: bwrap dies of SIGTERM, and `--die-with-parent` would then SIGKILL the command before its handler runs. A dropped connection kills the whole sandbox. A cwd in the project or under `/escrow/<id>/` maps to the same place in the sandbox; anything else maps to the project root. The child gets the caller's environment without `ESCROW_SOCKET`.
- **Close stops children**: a closing scope refuses new children. Close then SIGKILLs its running sandboxes and waits up to 10 s, then freezes the scope. Writes the children made before the kill are flushed as they exit, so they land in the change set. Phase 1 has no grace period before SIGKILL. Discard stops children the same way, and return accepts them again.
- **Containment**: built as the table above says. State and views are covered by a tmpfs, and the sockets by `/dev/null`, whenever a system or `sandbox.read` path contains them. `$HOME` is an empty tmpfs. Checked from both the app and a scope child; a `find /` from scope B finds none of scope A's files.
- Limits: argv, cwd and environment travel as UTF-8 (lossy). `escrow run` takes `--state`, `--mount` and `--socket` for tests; their defaults are under `$XDG_STATE_HOME` and `$XDG_RUNTIME_DIR/escrowd/<project-id>/`.

### 1.6 Python SDK

The SDK grows from spike 0.4's `escrow_shim.py`: the same `contextvars` scope, the same wrappers for `builtins.open`, `io.open`, `os.*` and pathlib, and `subprocess.Popen` rewritten to call `escrow exec`. New in phase 1:

- `escrow.init(project, unscoped, policy)` re-execs the program under `escrow run` when `ESCROW_SOCKET` is unset.
- `escrow.scope(name, decide=...)` opens the scope over RPC, runs the body, closes the scope and calls the decide callback with the change set.
- `s.outcome` reports status, paths, diff, reads and reasons.
- Paths returned by `os.path.realpath` and `os.getcwd` map back from `/escrow/<scope>/…` to the project path.

The SDK stays stdlib-only except for the protocol's client library, and targets the pinned Python (3.14.8).

As built (Oct 4, 2026):

- **`escrow.init(project, unscoped, policy=None, *, on_exit=None, read=())`**: without `ESCROW_SOCKET` it re-executes the program under `escrow run`. The new `--read` flag binds the interpreter, `sys.prefix`, `sys.base_prefix` and every absolute `sys.path` entry into the sandbox. It also binds every symlink on the way to the interpreter: uv's venv `python` points through a version-alias directory, and binding only the resolved path fails `execvp`. With `ESCROW_SOCKET` set, it checks the launcher's project and mode (`ESCROW_PROJECT`, `ESCROW_UNSCOPED`) and installs the wrappers. Against a daemon started outside `escrow run`, it only installs the wrappers. The routing checks run that way.
- **`escrow.scope(name, *, decide, labels, resume)`**: works with both `with` and `async with`. At exit, the SDK fsyncs the scope's open files, closes the scope and calls `decide(change_set)`. `decide` may be sync or async (async only under `async with`); its default is commit. `decide` returns `escrow.commit()`, `escrow.discard(*reasons)` or `escrow.send_back(*reasons)` (return to agent; `scope(resume=s)` continues the same scope). An exception in the body discards the scope with the exception as reason, then propagates. `s.outcome` has `status`, `paths`, `reasons`, `changes`, `reads` and `labels`. `escrow.settle_unscoped(decide)` settles the implicit default scope.
- **Wrappers**:
  - `open`/`io.open` and the `os` path functions are wrapped, which also covers pathlib, shutil and `os.walk`.
  - Calls with `dir_fd` are not rewritten. `symlink` rewrites only the link's path, never its target, which is content.
  - `os.path.realpath` and `os.getcwd` map `/escrow/<id>/…` back to the project.
  - `subprocess.Popen` becomes `escrow exec --scope <id> --socket … -- argv`, which covers `subprocess.run` and asyncio subprocesses. Its cwd is rewritten into the scope's view, so it may be a directory the scope created; the daemon maps it back. `escrow run` sets `ESCROW_VIEWS=/escrow` and `ESCROW_EXE`.
- **Not covered**: these fall to the unscoped mode — `os.system`, `os.posix_spawn`, `os.chdir` into the project (the cwd is per process), native code, and child file descriptors beyond 0–2. `EscrowUnscopedError` and `EscrowStaleHandleError` stay in phase 2.
- **Diff deferred**: `s.outcome` has no content diff yet. A closed scope refuses new opens, so the SDK cannot read the staged files at decide time. A daemon-side diff in the change set is a phase 2 item.
- The routing checks (spike 0.4's 17) now run on the SDK; the test-only `routing_shim.py` is gone.

### 1.7 Conformance suite and soak

`examples/test-app/` is a small CLI that does each exit test's IO with ordinary Python file and subprocess calls; the pytest suite drives it and checks the base, the change sets and the ledger. CI runs the suite 10 times in a row in one job on each of `ubuntu-24.04` and `ubuntu-26.04`; the job fails on the first failed run. Each Lima host runs it once with `limactl shell escrow-<host>` (after `vm.drop_caches=3`), and its log goes to `tests/conformance/results/`. Matrix VMs have no toolchain: they use the host's mise-installed uv and Python through the read-only home mount, as the 0.6 benchmark used Node, with the virtualenv under `/var/tmp`.

As built (Oct 4, 2026):

- `examples/test-app/app.py` runs one check per call: `escrowed` (exit test 1), `child` (2), `concurrent` (3), `discarded` and `atomic` (4), `conflict` (5), `denied-read` (6), `unscoped` (7, once per mode) and `snapshot` (check 8). It uses only `open`, pathlib, `os` and `subprocess`/asyncio under `escrow.init` and `escrow.scope`.
- `tests/conformance/test_app.py` drives it: 17 checks. Exit test 1 and the commit check compare the project with the tree from replaying the app's `mutate` natively on a copy. The atomic check commits once cleanly and once per injected fault (`journal`, `preimage`, two `apply` steps, `done`); each fault leaves the project byte-identical.
- **Misattribution check**: the app reports the path prefixes each scope id used, and every scope keeps its IO and content under its own prefix. After each run the suite fails on any ledger entry that names an unknown scope or a path outside that scope's prefixes. A unit check proves it flags foreign entries. Its first run flagged the decide callback's unscoped listing, which the app now declares.
- `test_sdk.py` drops its copies of exit tests 1, 3 and 6; it keeps the rest of the SDK's API. Suite: 121 checks.
- CI runs the suite 10 times in a row in one job per runner and fails on the first failed run (about 4 min per runner).
- `tests/conformance/lima.sh <host>` runs check 9: drop caches, run `lima-guest.sh` in the VM (Ubuntu: `packaging/ubuntu/install.sh` first), log to `tests/conformance/results/<host>.log`. uv and Python go by resolved paths, since Debian 13's sshfs home mount fails `readlink` with EPERM.

## Carried limits

Out of phase 1, by design:

- Whole-file copy-up; block-level copy-up only if large files demand it.
- Native code and `mmap` doing their own IO fall to the `unscoped` mode.
- No network capture (`--unshare-net` blocks the network; the proxy is phase 8).
- Nested scopes stay deferred.
- One daemon per `escrow run`; a long-lived per-user daemon waits until a harness needs one.

## Open questions

- [x] Protocol: gRPC over the Unix socket (tonic, grpcio), decided Oct 3, 2026. One schema generates every SDK in phases 3 and 5.
- [x] Per-scope metadata store: one SQLite file per scope (rusqlite), decided Oct 3, 2026 (proposal lesson 5: easy diffs, one file to discard, crash-safe for phase 2).
- [x] Checks 8 (snapshot at open) and 9 (Lima host matrix) join the phase 1 exit criteria, decided Oct 3, 2026.
- [x] Conflict check on reads: deferred, decided Oct 3, 2026. Phase 1 checks writes only and records each read's base version for a later decision.
- [x] GitHub Actions runner `ubuntu-26.04`: GA (x64 and arm64), checked Oct 3, 2026 ([runner-images](https://github.com/actions/runner-images)); CI runs on it and `ubuntu-24.04`. `ubuntu-latest` still means 24.04.
