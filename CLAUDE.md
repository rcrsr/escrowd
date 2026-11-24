# CLAUDE.md

escrowd captures every filesystem write a program makes inside a **scope**, holds it in escrow, and applies or discards the whole change set when the scope closes. It is built for LLM agent harnesses. Users start at `README.md`; the design is `docs/escrowd-proposal.md`.

## Where things stand
<!-- rule-level: operational -->

- **Active plan:** `docs/phase-5-typescript-sdk.md`. Phases 0 to 4 are complete (protocol 8, tag `protocol-8`); their plans in `docs/phase-*.md` hold the as-built notes.
- When a decision is made or an open question closes, tick it in the active plan with the date.
- `LIMITATIONS.md` lists every known limit. Update it when you find or lift one.
- `docs/protocol.md` specifies the protocol. Update it with every proto change.

## Layout

The repo follows `APP-MONOREPO-STANDARDS.md` (partial scope: Tier 1, Tier 2 and the Rust module; see Standards below).

| Path | What |
| --- | --- |
| `libs/escrowd/` | The daemon library (Rust, crate and Nx project `escrowd`). Each module opens with a `//!` doc comment; read it before changing the module. |
| `libs/proto/` | Crate `escrowd-proto`: only the code tonic and pbjson generate from `proto/`. escrowd re-exports it as `escrowd::proto`. |
| `apps/cli/` | The `escrow` binary (crate and Nx project `escrow-cli`): `run`, `daemon`, `exec`, `diff`, `review`, `log`. |
| `proto/escrow/v1/escrow.proto` | The one protocol schema, gRPC over a Unix socket (Nx project `protocol`). |
| `libs/python-sdk/` | Python SDK (distribution `escrowd`, package `escrow`). Generated stubs in `src/escrow/v1/` are committed. |
| `libs/testkit/` | `escrow_testkit`: the daemon harness, project trees, scenarios, and the SDK app harnesses (`apps.py`). |
| `tests/conformance/` | pytest suite against the built binary. `daemon/` is Python only and drives escrowd through `escrow.lowlevel`; `sdk/` runs per SDK. One shared `conftest.py`. |
| `tests/fixtures/` | Declarative scenarios (`scenarios/*.json`) that the testkit and the test apps apply. |
| `tests/soak/` | Crash, held-decision and close soaks (`crash.py N`, `held.py N`, `close.py N`). |
| `scripts/` | `check.py` (repo rules in `checks/`, Nx project `tools`), `versions.py`, and the shell behind `make`. |
| `bench/` | Real-repo benchmark (`run.sh`, `lima.sh`, `summarize.py`). |
| `packaging/ubuntu/` | escrowd's bwrap copy and AppArmor profile. |
| `nx.json`, `project.json` files | Every project carries `type:*` and `lang:*` tags; Rust and Python commands come from tag-filtered `targetDefaults` in `nx.json`. The root `project.json` is `@escrowd/root` (files outside any project). |
| `pyproject.toml`, `Cargo.toml` | The uv workspace (the only ruff, ty and pytest config) and the Cargo workspace (lints in `[workspace.lints]`). |
| `spikes/` | Phase 0 throwaway code, a separate Cargo workspace. Only `spikes/lima/` (VM templates) is used outside it. |

## Commands

Humans use `make` (`make` lists targets); hooks, CI and agents use the pnpm scripts, which call Nx.

```bash
make bootstrap                                     # mise install --locked, pnpm install, uv sync, hooks
pnpm run check:affected && pnpm run test:affected  # what pre-push runs (NX_BASE=origin/main)
pnpm run check                                     # every check on every project
pnpm run test:conformance                          # daemon and SDK suites; builds the debug binary first (or set ESCROW_BIN)
pnpm run test:coverage                             # suite on an instrumented daemon, then diff-cover at 80%
pnpm run fix:lint && pnpm run fix:format           # apply clippy, ruff, rustfmt, buf and shfmt fixes
pnpm exec nx run escrowd:lint                      # one target; nx show project <name> --json lists them
pnpm run generate                                  # regenerate the Python stubs after editing proto/ (python-sdk:stubs diffs them)
tests/conformance/lima.sh ubuntu-24.04             # suite in one host-matrix VM
cargo build --release -p escrow-cli --features fault-injection  # binary that honors ESCROWD_FAULT (soaks: ESCROW_BIN=target/release/escrow)
```

On Ubuntu, run `packaging/ubuntu/install.sh` once before the suite.

## Rules that are easy to break

These were measured or decided; do not reopen them without new evidence. Phase 0's evidence is in `spikes/results/phase-0-report.md`.

- **Protocol is additive only.** Frozen at v1 (tag `protocol-v1`): add fields and calls, never change or remove them. A break is `escrow.v2`.
- **No FUSE passthrough.** It needs `CAP_SYS_ADMIN`. escrowd runs plain unprivileged FUSE.
- **Never suggest setting `kernel.apparmor_restrict_unprivileged_userns` to 0.** Ubuntu 24.04+ uses escrowd's own bwrap and AppArmor profile instead. Sandboxes always pass `--disable-userns`.
- **The daemon must never touch its own FUSE view.** It deadlocks.
- **Views live under `$XDG_RUNTIME_DIR`.** Ubuntu 26.04 confines `fusermount3` to a few roots.
- **Flush with `fsync`, not `syncfs`.** On FUSE, `syncfs` does not wait for the daemon.
- **Inode numbers are per scope** (`views/mod.rs` explains the scheme). Sharing them between scopes shares page cache.
- **grpcio clients must set `grpc.default_authority`** (the SDK uses `localhost`). Otherwise tonic rejects the call.
- **The Python SDK supports 3.11.** No newer syntax or APIs.
- **Decided against:** AgentFS (stale inode maps break git), copy snapshots at scope open (pre-images at commit instead).
- **Kernel 6.8 minimum.** WSL2 is the dev host only; targets are Linux, then macOS.
- **Library errors are typed.** Return `crate::error::Result<T>`, never `io::ErrorKind` as a status code; `anyhow` is only for policy parsing, the CLI and daemon start-up, bind and serve.
- **Locks never poison.** Use `parking_lot::Mutex` for sync locks; never `std::sync::Mutex` or `.lock().unwrap()`.
- **Every `unsafe` block in `libs/escrowd` has a one-line `// SAFETY:` comment** above it stating its precondition.
- **No blocking calls on the runtime.** Filesystem and SQLite work in an async fn goes through the `blocking` helper in `decision.rs`.
- **Calls go inward:** `rpc.rs`, then `decision.rs`, then `views/`, then storage, then `sys.rs`. New code adds no upward `use crate::` import, and `rpc.rs` holds no ordering, lock or policy logic.
- **All SQLite goes through `db::open` and `db::migrate`.** Append a new migration step to change a schema; never edit an existing step or call `Connection::open` elsewhere.
- **Commit is all or nothing.** A closed scope's change set applies through `commit/`; never write the project directly or best-effort.
- **Base and upper access is fd-relative.** Use `sys.rs` directory-fd calls, never `std::fs` on a joined absolute path.
- **FUSE ops only forward.** A `fuse.rs` method calls the same-named `Views` method in `views/overlay.rs`; no file operations in `fuse.rs`.
- **Reviewer calls go on `ReviewerService`** (the separate 0600 review socket), never `EscrowService`.
- **Policy keys parse only in `policy.rs`.** Enforce `read:` in `gate.rs`, `write:`/`review:` in `review.rs`, `roots:` in `roots.rs`.
- **Diagnostics use `eprintln!("escrowd: <area>: {e}")`.** No `println!`, `dbg!` or logging crate in the daemon.
- **Only `#[allow(clippy::too_many_arguments)]` is permitted** in hand-written code, under the workspace lints (clippy `all` deny, `pedantic`, `unwrap_used`). Generated code lives only in `escrowd-proto`, whose own `[lints]` turn clippy off. Fix a pedantic finding; never allow it. New dependencies go in `[workspace.dependencies]` with `dep.workspace = true`.
- **`unsafe_code` is denied workspace-wide.** `escrowd`, `escrow-cli` and escrowd-proto's `build.rs` opt out at the crate root with a reason.

## CI and pull requests
<!-- rule-level: operational -->

- `main` takes squash-merged PRs only. CI (`.github/workflows/ci.yml`) runs on PRs, not on pushes to `main`; the `CI Gate` job is the required check.
- Add the `ci:repeat` label (10 consecutive suite runs plus the crash and held soaks) when a PR touches the commit path, FUSE, the sandbox or concurrency, or ends a sub-phase whose exit needs 10 passes. Re-add it after each new push. Never on docs-only or tooling PRs.
- CI has no path filters: Nx's affected graph (`NX_BASE=origin/main`) picks the projects, including the conformance suites.
- `daemon-frozen.yml` fails a PR labeled `phase-5` that changes `libs/escrowd/`, `libs/proto/` or `proto/`.
- Pin every `uses:` to a commit SHA with a `# vX.Y.Z` comment (`check.py` rule CI001); `actionlint` and `zizmor` run in the hooks and CI.

## Environment
<!-- rule-level: operational -->

- Every tool is pinned in `mise.toml` and locked with checksums in `mise.lock` (refresh with `make toolchain-lock`). `rust-toolchain.toml` equals the mise `rust` pin. Python is uv-managed through `UV_PYTHON`; there is no `.python-version`. Set `ESCROWD_PYTHON` to run on another Python.
- A shell opened before a toolchain bump keeps stale `RUSTUP_TOOLCHAIN`/`UV_PYTHON`; run `eval "$(mise env -s bash)"` or open a new shell. `make doctor` reports it.
- Use pnpm (not npm) and uv (not pip). `pnpm install` also installs the lefthook hooks.
- Pin exact versions (lockfiles hold them). New npm and PyPI releases wait 7 days (`minimumReleaseAge`, `exclude-newer`); an exemption names one release and its expiry date.
- Tests leave no temp files: Rust tests use `db::temp_dir` (a `tempfile` guard), Python tests use `tmp_path` or `make_runtime_dir`.

## Standards
<!-- rule-level: operational -->

Not applicable (APP-MONOREPO-STANDARDS N/A conditions):

- No TypeScript yet (until phase 5.2): APP-SQ-1 to 4, APP-SQ-8, APP-TS-2, APP-CK-9. No Playwright: APP-TS-6.
- No Dockerfile or compose: APP-SQ-11, APP-SUP-7, APP-IF-2 to 4. No CDK: APP-IF-1, APP-TMP-6, 7. No HTTP service, SPA or Alembic: APP-STK-2, 3, APP-BE-*, APP-FE-*.
- Never deploys (built from source, no prebuilt binaries): APP-RL-3. No preview builds: APP-RL-4.

Exceptions (decided 2026-11-23):

- APP-STK-1: protobuf with buf (`proto/`, project `protocol`), and Lima for the host-matrix VMs.
- APP-WS-3, §5.9: Python floor and ruff/ty targets stay 3.11 (the SDK supports 3.11); workspace members are listed, since `libs/` also holds Rust crates.
- §7.2: public identifiers keep their names (PyPI `escrowd`, import `escrow`, crates `escrowd` and `escrow-cli`, binary `escrow`); renaming breaks users.
- APP-SQ-9: `escrowd-proto` (generator output only) sets its own `[lints]` instead of the workspace's.
- APP-OR-2: project-specific targets carry their command in `project.json` (`stubs`, `conformance`, `build:debug`, `dev`, the `protocol` targets); no tag default fits them.
- APP-TS-1: the conformance suites run with `-n 0`. xdist workers share the host mount namespace, so one worker's bwrap binds another's FUSE mount and fails.
- APP-TMP-5: the testkit's runtime dirs use `mkdtemp` under `$XDG_RUNTIME_DIR`, since pytest's `tmp_path` exceeds the 108-byte socket path limit. They carry the `escrowd-conformance-` prefix and are swept after a day.
- APP-CI-5: the required check is `CI Gate` (branch protection), which needs `check`, `coverage` and `conformance`.
- APP-OP-1: no Sentry. escrowd is a local daemon that handles private file paths; it reports nothing off the host.
- `<shared-pkg>` does not exist yet: `scripts/check.py` stands in for `check-standards` and `check-secret-files` (rule SEC001), `scripts/versions.py` for `sync-version`, `scripts/coverage.sh` for `diff-coverage`.

## Lima VMs
<!-- rule-level: operational -->

Host-matrix VMs come from `spikes/lima/*.yaml` (`limactl start --name=escrow-ubuntu-24.04 spikes/lima/ubuntu-24.04.yaml`; also `ubuntu-26.04`, `debian-13`, `fedora-44`, `bench-ubuntu-24.04`).

- VMs see host binaries through Lima's read-only home mount, which serves stale files after host edits. Run `limactl shell escrow-<host> sudo sysctl -q vm.drop_caches=3` before every run.
- Run long jobs with `nohup … &` and wait on the PID with `kill -0`. `pgrep -f` matches its own command line.
