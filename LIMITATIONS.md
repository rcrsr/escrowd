# Known limitations

Every known limit of escrowd as built, in one place. Each entry says where it is tracked. Update this file when a limit is found or lifted; the plans keep the detail.

Status as of Oct 6, 2026: phase 3, after sub-phase 3.5.

## Capture

| Limit | Status |
| --- | --- |
| Native code and `mmap` doing their own IO fall to the `unscoped` mode. | By design (proposal risk table); run such work in a subprocess for full capture. |
| `os.spawn*`, `os.exec*` and `os.fork` bypass the SDK's scope (`os.system`, `os.posix_spawn*` and `subprocess` run in it since 2.6). | Carried; each scope's outcome counts what reached the unscoped mode (`s.outcome.unscoped`). |
| `os.chdir` into the project moves the whole process's working directory into the scope's view: another task's native code with relative paths follows it until the scope is decided. | By design (2.6): the working directory is per process. |
| `s.outcome.unscoped` counts every change the unscoped mode saw while the scope was open, including other tasks' and threads' (and from daemon start, after a restart); 0 in `passthrough` mode. | By design (2.6). |
| In the host process, an absolute symlink to a project path is followed by the kernel to the project path, which the unscoped mode serves, not the scope (sandboxed children resolve it inside the scope's view). | Found Oct 5, 2026 (proposal scrub); carried. |
| Child file descriptors beyond 0–2 are not passed to `escrow exec` children. | Carried since 1.5. |
| `passthrough` mode IO is unlogged: one `op=passthrough` ledger line at daemon start only. | By design, decided Oct 4, 2026 (logging each operation would route it through FUSE); built in 2.7. `deny` is the mode for agent hosts. |
| Paths outside `$HOME`, `/tmp` and the project are not captured: other host paths are read-only binds (`roots.other.read`) or unescrowed passthrough binds. | By design (2.4). |
| `passthrough` paths (package caches) are shared and unescrowed: one ledger line per bind at sandbox start, the IO itself unlogged. | By design (2.4, #11). |
| In `passthrough` unscoped mode, the app's own `$HOME` and `/tmp` stay empty tmpfs even when the policy serves them; its scopes get their views. | By design, decided Oct 5, 2026: no unscoped scope exists to serve them. |
| The SDK rewrites absolute paths under a served root (`os.path.expanduser` gives one); a literal `~/x` passed to `open` is a relative path, as in Python. | By design. |
| No network capture; `--unshare-net` blocks the network. | Phase 9. |

## Attribution

| Limit | Status |
| --- | --- |
| A process's arguments are what it says about itself: any process can rewrite its own `argv`. A script reports its interpreter as its program (`/usr/bin/dash`, not the script). A process that execs its own binary again with new arguments keeps its first record. | By design (3.3); `write.only_by` matches the binary's device and inode, which the kernel set. |
| A write through a file descriptor counts toward the process that opened the file: a descriptor inherited or passed, an `mmap` write and the kernel's writeback all name the opener. The process that writes and closes it also shows, since the kernel sends the file's times in its name. | By design, decided Oct 6, 2026: no per-write cost, and the writeback cache keeps its speed. |
| In-process SDK IO names the host process: two tool calls in one host differ only by their scopes' labels. | By design (3.3). |
| Reads, lookups and listings name no process; only changes do (create, open for writing, mkdir, symlink, link, rename, unlink, rmdir, setattr). | By design (3.3): attribution costs one `/proc` read per change, none per read. |
| The parent chain stops before the daemon, after a session leader or after 16 parents. A program's path is as its own mount namespace shows it (a sandboxed binary in the project: its project path). | By design (3.3). |
| Command lines can hold secrets (`curl -H "Authorization: …"`): the change set carries them whole, the ledger up to 4 KiB. | By design, decided Oct 6, 2026 (redacting by policy pattern was the alternative). |

## Filesystem semantics

| Limit | Status |
| --- | --- |
| Copy-up copies the whole file. | Carried; block-level copy-up only if a workload needs it. |
| Renaming a base directory fails with EXDEV (as in overlayfs); `RENAME_EXCHANGE` fails with EINVAL. | Carried since phase 0. |
| An editor outside escrowd that changes a base file's size: a scope that already looked the file up keeps the old size until the kernel drops the inode (FUSE writeback cache). | Found in 2.3, present since phase 0; carried. |
| An editor's change to a base file reaches a scope at the next open (pages), but names, attributes and listings a scope has cached stay for up to 60 s. | By design since 2.3 (snapshot scopes). |
| An editor write in the microseconds between apply's re-check and its rename still loses to the commit. | Accepted (2.2, proposal risk table). |

## Durability and recovery

| Limit | Status |
| --- | --- |
| Tested against process kills only, not power loss; the fsync audit covers what kills cannot. | Carried (phase 2). |
| A daemon killed mid-run renumbers new upper-only entries and drops first reads and writers not yet written from the change set; renames of base files and change versions are always written. A change without a writer breaks any `write.only_by` rule on its path. | By design since 2.3 (writers since 3.3); no process sees both numbers, since the old mount is dead. |
| A daemon restart leaves sandboxes' bind mounts stale (ENOTCONN). | Carried since phase 0. |

## Performance

| Limit | Status |
| --- | --- |
| Every file opened costs an OPEN and a RELEASE round trip, even with its pages cached: warm `rg` runs at about 3.6× native, warm `git log -p` at about 1.8×; workload C has no speed target (check 3 amended in 2.8). | Kept, decided Oct 5, 2026: the kernel's no-open mode would drop the per-open gate check, the ledger's open records and the handles close waits on. |
| Escrow mode's speed depends on the host more than native's: each FUSE operation waits on the daemon's threads being scheduled. In 2.8 the same binary ran express A at 1.46× to 1.57× and warm `git log -p` at 1.03× to 1.89× across sessions. | Inherent to FUSE; measured in 2.8 (`bench/results/ab-2.3-vs-2.7.txt`). Benchmarks run on a quiet host and compare binaries in one session. |
| Cold metadata- and read-heavy work costs 5–30× per operation, as in any FUSE file system (`tar` over `node_modules` 8.96×). | Inherent to plain FUSE; kernel passthrough needs `CAP_SYS_ADMIN` (root-helper fallback only). |

## Platform

| Limit | Status |
| --- | --- |
| Linux only, kernel 6.8 or later. | macOS in phase 8. |
| Ubuntu 24.04+ needs escrowd's own bwrap and AppArmor profile (`packaging/ubuntu/install.sh`); never set the userns sysctl to 0. | By design (0.1, 0.2). |
| Ubuntu 26.04 confines `fusermount3` to mountpoints under `$HOME`, `/mnt`, `/run/user/<uid>`, `/media` and `/tmp`; views live under `$XDG_RUNTIME_DIR`. | By design. |

## Interface and operations

| Limit | Status |
| --- | --- |
| The scope token stops callers that know only a scope id; in-process code that reads the SDK's memory can take a token (the proposal's threat model trusts in-process code). | By design (2.7, #15): untrusted code runs as subprocesses, which get no socket and no token. |
| The unscoped scope has no token: any socket client can settle and decide it, as before. | By design (2.7): it is the host's own default scope. |
| `GetChangeSet` (`escrow diff`) needs no token. | By design (2.7): it only reads a closed scope's change set, `read.deny` content withheld. |
| Scopes opened before protocol 6 have no token and take any. | By design (2.7): they predate it. |
| `EscrowUnscopedError` covers project paths only; a write outside a scope to a served `$HOME` or `/tmp` path in `deny` mode is a plain EROFS. Native code and children see raw EROFS and EBADF. | By design (2.6). |
| The diff shows no content for binary files and files over `diff.file_bytes` (size and SHA-256 only) or under `read.deny` (mode only); `git apply` cannot apply those sections. | By design (2.5). |
| A permission change git's modes cannot show (0644 to 0600) is a `# escrow: mode of …` line at the end of the diff, not an `old mode`/`new mode` header. | By design (2.5). |
| `escrow diff` and `GetChangeSet` need a closed, undecided scope; an open scope has no diff yet, and a decided scope's diff lives only in the caller's change set and outcome. | By design (2.5). |
| `escrow run --on-exit` settles the default scope without building a diff. | By design (2.5): nothing reads it. |
| A conflict under `conflict.verdict: return` reopens the scope on its old snapshot: a path that conflicted conflicts again while the scope still touches it (or, with `conflict.reads`, has read it). The agent drops that change or redoes the work in a new scope. | By design (2.7); rebase stays out of phase 2. |
| `conflict.reads` is off by default: two concurrent scopes can each read a file the other changes and both commit (write skew). | By design, decided Oct 5, 2026: sessions that wait (phase 3) serialize one agent's scopes; hosts that need serializable scopes turn it on. |
| `conflict.reads` checks files read through the scope's view; reads not yet written to the store when the daemon was killed drop out of the check, as they drop out of the change set. | By design (2.7, 2.3). |
| One daemon per `escrow run`; nested scopes are not supported. | Carried. |
| No LLM reviewer exists yet: an LLM-tier hold waits for a stand-in on the review socket, or for a human, whose verdict also stands for a pending LLM tier. | Phase 7 (LLM auditor). |
| Anyone who can open the review socket (the daemon's user, mode 0600) reviews as any tier: the socket does not tell an LLM from a human, and `--tier` is the caller's word. | By design, decided Oct 5, 2026: the file mode is the credential; per-tier credentials come with phase 7's reviewers. |
| The history keeps the newest 100 decisions per session (and 100 of scopes without one); a reviewer sees the last 20. `AwaitDecision` on a scope whose decision dropped out fails with `NOT_FOUND`. | By design (3.4). |
| A scope's history entry holds its diff (up to `diff.max_bytes`), so `history.sqlite` grows with the diffs of up to 100 decisions per session. | By design (3.4); decisions of scopes without a session that were never held are not kept. |
| While the unscoped scope (`implicit` mode) is held, IO outside scopes fails as on any closed scope, until its verdict. | By design (3.2): `SettleUnscoped` meets the same review rules, decided Oct 5, 2026. |
| A session's next `OpenScope` waits behind a held scope only up to the client's deadline (`grpc-timeout`); a client without one waits until the verdict. | By design (3.2). |
| The Python SDK waits for a held verdict with no deadline: `escrow.scope(..., wait=True)` and a session's next open block until reviewers decide. `s.wait_decided(timeout=…)` and `await s.decided(timeout=…)` take one. | By design (3.5): the agent's turn is the reviewers' to end. |
| A `deny` rule lets lookups pass: a denied file's name, size and times are visible to `stat`, its contents and listings are not. | By design, decided Oct 5, 2026 (listed paths inside a denied directory must stay reachable). |
| A scope's view of a root the policy stops serving is dropped (with its changes) at the next daemon start. | By design (2.4). |
| `escrow exec` passes argv, cwd and environment as UTF-8 (lossy). | Carried since 1.5. |
