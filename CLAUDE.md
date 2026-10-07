# CLAUDE.md

escrowd captures every filesystem write a program makes inside a **scope**, holds it in escrow, and applies or discards the whole change set when the scope closes. It is built for LLM agent harnesses. Users start at `README.md`; the design is `docs/escrowd-proposal.md`.

## Where things stand

- **Active plan:** `docs/phase-4-reviewers.md` (policy reviewers). Next: `docs/phase-5-typescript-sdk.md`. Phases 0 to 3 are complete; their plans in `docs/phase-*.md` hold the as-built notes.
- When a decision is made or an open question closes, tick it in the active plan with the date.
- `LIMITATIONS.md` lists every known limit. Update it when you find or lift one.
- `docs/protocol.md` specifies the protocol. Update it with every proto change.

## Layout

| Path | What |
| --- | --- |
| `crates/escrowd/` | The daemon library (Rust). Each module opens with a `//!` doc comment; read it before changing the module. |
| `crates/escrow-cli/` | The `escrow` binary: `run`, `daemon`, `exec`, `diff`, `review`, `log`. |
| `proto/escrow/v1/escrow.proto` | The one protocol schema, gRPC over a Unix socket. |
| `sdk/python/` | Python SDK (uv project, package `escrow`). Generated stubs in `src/escrow/v1/` are committed. |
| `tests/conformance/` | pytest suite against the built binary. Every SDK must pass it. |
| `tests/soak/` | Crash, held-decision and close soaks (`crash.py N`, `held.py N`, `close.py N`). |
| `bench/` | Real-repo benchmark (`run.sh`, `lima.sh`, `summarize.py`). |
| `packaging/ubuntu/` | escrowd's bwrap copy and AppArmor profile. |
| `spikes/` | Phase 0 throwaway code, a separate Cargo workspace. Only `spikes/lima/` (VM templates) is used outside it. |

## Commands

```bash
cargo build && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cargo test
sdk/python/gen.sh                                                  # after editing proto/; CI diffs the stubs
buf lint && buf format --diff --exit-code                          # CI also runs buf breaking against protocol-v1
uv run --project sdk/python --frozen ruff check && uv run --project sdk/python --frozen ruff format --check
uv run --project sdk/python --frozen ty check --project sdk/python
uv run --project sdk/python --frozen pytest -q tests/conformance  # needs target/debug/escrow (or ESCROW_BIN)
tests/conformance/lima.sh ubuntu-24.04                             # suite in one host-matrix VM
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
- **Inode numbers are per scope** (`views.rs` explains the scheme). Sharing them between scopes shares page cache.
- **grpcio clients must set `grpc.default_authority`** (the SDK uses `localhost`). Otherwise tonic rejects the call.
- **The Python SDK supports 3.11.** No newer syntax or APIs.
- **Decided against:** AgentFS (stale inode maps break git), copy snapshots at scope open (pre-images at commit instead).
- **Kernel 6.8 minimum.** WSL2 is the dev host only; targets are Linux, then macOS.

## CI and pull requests

- `main` takes squash-merged PRs only. CI (`.github/workflows/ci.yml`) runs on PRs, not on pushes to `main`; the `CI Gate` job is the required check.
- Add the `ci:repeat` label (10 consecutive suite runs plus the crash and held soaks) when a PR touches the commit path, FUSE, the sandbox or concurrency, or ends a sub-phase whose exit needs 10 passes. Re-add it after each new push. Never on docs-only or tooling PRs.
- `daemon-frozen.yml` fails a PR labeled `phase-5` that changes `crates/escrowd/` or `proto/`.
- Lint workflows with `actionlint`.

## Environment

- Every tool is pinned in `mise.toml`; run `mise install`. Keep `rust-toolchain.toml` and `.python-version` in sync with it.
- Use pnpm (not npm) and uv (not pip).
- Pin exact versions. When choosing one, take the latest stable or LTS.
- Run `lefthook install` once per clone. Hooks format staged files and lint on push; the suite runs only in CI.

## Lima VMs

Host-matrix VMs come from `spikes/lima/*.yaml` (`limactl start --name=escrow-ubuntu-24.04 spikes/lima/ubuntu-24.04.yaml`; also `ubuntu-26.04`, `debian-13`, `fedora-44`, `bench-ubuntu-24.04`).

- VMs see host binaries through Lima's read-only home mount, which serves stale files after host edits. Run `limactl shell escrow-<host> sudo sysctl -q vm.drop_caches=3` before every run.
- Run long jobs with `nohup … &` and wait on the PID with `kill -0`. `pgrep -f` matches its own command line.
