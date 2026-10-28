# Phase 5: TypeScript SDK Plan

Oct 5, 2026 · Andre Bremer · Draft

**Status, Oct 25, 2026: 5.1 built** (nx workspace, CI and hooks call nx; see the 5.1 as-built note); 5.2 to 5.5 not started. Built after phase 4, policy reviewers ([plan](phase-4-reviewers.md)). Drafted as phase 3; renumbered Oct 5, 2026, when held decisions moved ahead of the freeze, and again Oct 6, 2026 (phase 4 to 5), when policy reviewers moved ahead of it.

Phase 5 proves that escrowd is one daemon behind many SDKs. Phase 3 froze the protocol and phase 4 adds policy reviewers to it, additions only; a TypeScript SDK is built against the daemon as phase 4 leaves it, and the SDK half of the conformance suite must pass against it, held decisions and reviewers included. Phase 6's pi adapter is written in TypeScript and captures pi's built-in tools through this SDK, so the SDK must cover what pi does: `fs` and `fs/promises`, `child_process`, concurrent async work on one thread.

From the proposal: **the conformance suite passes against the same daemon with no daemon changes, with the SDK checks run against the TypeScript SDK.**

The suite is one pytest suite in two parts, decided Oct 19, 2026 (reversing the Oct 5 decision to port it whole): `tests/conformance/daemon/` is Python only and drives escrowd through `escrow.lowlevel`; `tests/conformance/sdk/` runs once per SDK.

## Exit criteria

1. **No daemon changes.** From the end of phase 4 (tag `protocol-8`) to the end of phase 5, `crates/escrowd/` and `proto/` are unchanged: a CI check fails any phase 5 PR that touches them. A daemon bug found by the TypeScript work is fixed in its own PR, which moves the tag and restarts this count.
2. **The suite, per SDK.** The pytest suite passes: `daemon/` once against the daemon, `sdk/` against the Python SDK and the TypeScript SDK (the test app parametrized over `app.py` and `app.ts`), held-decision checks included. `nx run-many -t conformance` runs it, 10 consecutive times on each CI runner (`ci:repeat`) and once on each Lima host.
3. **pi's IO is covered.** Each call pi's built-in bash, read, write and edit tools make (an inventory taken in 5.2) is either captured in the scope or listed in `LIMITATIONS.md` with what it falls to.
4. **No regression.** The daemon-level checks and the Python SDK pass as at the end of phase 4.

## Sub-phases

```mermaid
flowchart LR
    S1["5.1 Repo task runner<br/>(nx)"] --> S4["5.4 SDK conformance<br/>for TypeScript"]
    S2["5.2 TypeScript SDK:<br/>client, scopes, fs, held decisions"] --> S3["5.3 child_process,<br/>chdir, escapes"]
    S3 --> S4
    S4 --> S5["5.5 Exit runs"]
```

| # | Sub-phase | Goal (done when…) | Checks |
| --- | --- | --- | --- |
| 5.1 | Repo task runner | An nx workspace: build, lint, test and conformance targets for Rust, Python and TypeScript; CI and the git hooks run nx; the Python suite runs through it unchanged | 2 (Python), 4 |
| 5.2 | TypeScript SDK: client, scopes, fs, held decisions | `sdk/typescript/`: gRPC client, `init` re-exec under `escrow run`, `scope()` in `AsyncLocalStorage`, decide callbacks, held outcomes and `AwaitDecision`, outcome, typed errors, path rewrite of `fs`, `fs/promises` and streams; pi's IO inventory | 3 (fs) |
| 5.3 | `child_process`, `chdir`, escapes | `spawn`, `exec`, `execFile` and their sync forms run through `escrow exec` with the scope's token; `process.chdir` into the view; unscoped count and warning; stale handles | 3 |
| 5.4 | SDK conformance for TypeScript | The `sdk/` half of the pytest suite runs against the TypeScript SDK (the test app in TypeScript, `app.ts` beside `app.py`), passing on the dev host | 2 |
| 5.5 | Exit runs | Checks 1–4 on the final commit: 10 consecutive CI runs per runner, the Lima matrix, the daemon unchanged since the tag | All |

## Design for each sub-phase

### 5.1 Repo task runner

With three languages (Rust, Python, TypeScript), each with its own build, lint and test commands, nx (23.2.1, latest on Oct 5, 2026) gives the whole repo one task graph, decided Oct 5, 2026. Each part is an nx project with `build`, `lint`, `test` and, where it applies, `conformance` targets that run the existing tools (`cargo`, `uv`, `pnpm`): the daemon and CLI (`crates/`), the Python SDK, the TypeScript SDK, and the two conformance suites, which depend on the daemon's build. `nx run-many -t lint test` and `nx affected -t conformance` replace the tool commands spelled out in CI and the lefthook hooks; nx caches task outputs by their inputs, so an unchanged part is not rebuilt or retested. The cost: Rust and Python work needs Node and pnpm installed (both already pinned in `mise.toml`). The Python suite runs through nx unchanged before the TypeScript work starts.

**As built, Oct 25, 2026.** Decisions D-DX-1 to D-DX-11:

- **D-DX-1** Five nx projects, each with its own `project.json`: `rust` (`crates/`), `python-sdk` (`sdk/python/`), `protocol` (root `project.json`), `conformance-daemon` and `conformance-sdk` (`tests/conformance/{daemon,sdk}/`). Workspace settings in `nx.json`, with `neverConnectToCloud`.
- **D-DX-2** Commands live in `project.json` through `nx:run-commands`, run from the workspace root. The root `package.json` has no scripts; always `pnpm exec nx`.
- **D-DX-3** nx `23.2.1` pinned exactly in root `devDependencies`; `packageManager` is `pnpm@12.7.0`, mirroring `mise.toml`; `"type": "module"`; nx is not in `mise.toml`. `pnpm-workspace.yaml` allows nx's build script.
- **D-DX-4** Cached: `rust:lint`, `rust:test`, `python-sdk:lint`, `protocol:lint`, `protocol:test`, each with explicit `inputs` (including `mise.toml` and, for Rust, `rust-toolchain.toml` and `RUSTUP_TOOLCHAIN`; for Python, `.python-version`) and `outputs: []`. `cache: false` on `rust:build`, `python-sdk:build`, `python-sdk:test` and the conformance targets. `python-sdk:test` is the generated-stub diff (`gen.sh && git diff --exit-code`); `python-sdk:build` builds the wheel and checks it ships `py.typed`.
- **D-DX-5** `targetDefaults.conformance`: `cache: false`, `parallelism: false`, `dependsOn: ["rust:build"]`. The suites run `pytest --rootdir tests/conformance` on `tests/conformance/daemon` and `tests/conformance/sdk`; both projects have `implicitDependencies` `rust` and `python-sdk`.
- **D-DX-6** Deviation: CI keeps `dorny/paths-filter` in `ci.yml` and each job calls `pnpm exec nx run-many -t <targets> -p <project>`; no `nx affected`. `conformance.yml` calls `pnpm exec nx run-many -t conformance --outputStyle=stream`. `ci-gate` `needs:` is unchanged.
- **D-DX-7** CI setup goes through the composite action `.github/actions/setup-nx` (node from the `mise.toml` pin, `pnpm/action-setup`, `pnpm install --frozen-lockfile`).
- **D-DX-8** Deviation: `lefthook.yml` pre-commit is unchanged. Pre-push calls `nx run rust:lint`, `python-sdk:lint` and `protocol:lint` behind narrow globs, not `nx affected`; the `lockfiles` job also runs `pnpm install --frozen-lockfile`.
- **D-DX-9** Deviation: `protocol:test` runs `buf breaking --against '.git#tag=protocol-v1'` unconditionally; the protocol-v1 guard is dropped.
- **D-DX-10** Deviation: no placeholder build targets; `protocol` and the conformance projects have no `build`.
- **D-DX-11** Nothing under `crates/escrowd/` or `proto/` changed; the daemon stays frozen for phase 5.

Not verified in this change (ENV): the CI Gate and the `ci:repeat` runs run on PR #42. Open gap: `tests/conformance/lima-guest.sh` still runs pytest directly through `uv` (the Lima guest is not yet routed through nx).

### 5.2 TypeScript SDK: client, scopes, fs, held decisions

`sdk/typescript/`, a pnpm package, TypeScript 7 compiled to ESM, Node 22 and 24, lint and format with oxlint and oxfmt, unit tests in Vitest (decided Oct 5, 2026):

- **Client.** `@grpc/grpc-js` over `unix:` with `grpc.default_authority` set (tonic rejects the socket path as authority, as with grpcio). Stubs generated from `proto/` and committed; CI diffs them, as it does Python's.
- **`init()`.** Without `ESCROW_SOCKET`, re-executes the process under `escrow run` (same arguments as Python: project, unscoped mode, policy, on-exit) and exits with the child's code.
- **`scope()`.** `await escrow.scope(name, fn, {decide, labels})` and a `Scope` with `outcome`, `resume`; the current scope lives in an `AsyncLocalStorage`, so concurrent async work on one thread keeps separate scopes (exit test 3). The token stays on the `Scope` object.
- **Held decisions.** The close carries the proposed verdict and whether the client can wait, as phase 3 defines it; a `held` outcome resolves through `AwaitDecision`, and the scope's promise settles with the verdict. The outcome names its pending reviewers (`Outcome.reviewers`, phase 4) as Python's does.
- **fs.** Path rewrite (project, `$HOME` and `/tmp` roots from `OpenScopeResponse.roots`) in every path-taking function of `fs`, `fs/promises` and the stream constructors; `realpath`, `__filename`-style results and error paths mapped back. ESM imports of `node:fs` see the patched functions through `module.syncBuiltinESMExports()`. Flush before close: every handle opened in the scope is fsynced (`FileHandle.sync`, `fs.fsync`), as Python's `Scope.flush` does.
- **Typed errors.** `EscrowUnscopedError` (code `EROFS`, a `deny`-mode write outside a scope), `EscrowStaleHandleError` (`EBADF` on a handle of a closed scope), `EscrowUnscopedWarning` through `process.emitWarning`; each keeps `code`, `errno`, `syscall` and `path`, so `err.code === 'EROFS'` checks keep working.
- **nx project and hooks.** `sdk/typescript/project.json` defines `typescript-sdk` with `lint` (oxlint, `oxfmt --check`, `tsc --noEmit`), `test` (Vitest and the stub diff) and `build`. Hooks ship in the same PR, mirroring Rust and Python: pre-commit runs `pnpm exec oxfmt {staged_files}` on staged `*.{ts,mts}` with `stage_fixed`; pre-push runs `pnpm exec nx run typescript-sdk:lint`, gated on `{sdk/typescript/**,pnpm-lock.yaml}`. No Vitest in hooks: tests run in CI only (decided Oct 27, 2026).
- **pi's IO inventory.** List the calls pi's bash, read, write and edit tools make (from pi's source at a pinned version), and mark each covered, covered in 5.3, or a limit. This is check 3's list.

### 5.3 `child_process`, `chdir`, escapes

- **Subprocesses.** `spawn`, `exec`, `execFile`, `spawnSync`, `execSync`, `execFileSync` inside a scope run `escrow exec --scope ID --socket S -- argv` with `ESCROW_SCOPE_TOKEN` in its environment, `cwd` rewritten into the view, `shell: true` as `/bin/sh -c`. `fork` (Node IPC over an extra fd) falls to the unscoped mode: `escrow exec` passes fds 0–2 only (a limit since 1.5).
- **`process.chdir`** into the project moves into the scope's view; `process.cwd()` maps back; the decision restores it (Python's 2.6 design).
- **Unscoped count.** `outcome.unscoped` and a warning at close, from `ChangeSet.unscoped_ops`.
- **Not covered, by design:** native addons doing their own IO, `worker_threads` (a worker does not inherit the `AsyncLocalStorage` context; work it does falls to the unscoped mode unless the worker opens its own scope), `process.binding` and other internals.

### 5.4 SDK conformance for TypeScript

One pytest suite, decided Oct 19, 2026. `tests/conformance/daemon/` is Python only: it drives escrowd through `escrow.lowlevel` and `escrow_testkit` and imports no public SDK API, so a second SDK never duplicates it. `tests/conformance/sdk/` runs per SDK: the harnesses in `escrow_testkit.apps` take an argv prefix, and the `app` fixture is parametrized over `APPS` (`python` now; 5.4 adds `typescript` with `examples/test-app/app.ts` beside `app.py`). Both directories run against the same daemon binary and share one `conftest.py`; nx runs them (`nx run-many -t conformance`). No name-diff CI step is needed, since there is one suite and no second copy to keep in step.

The SDK's own unit tests (path rewrite tables, error mapping) run in Vitest in a `TypeScript` CI job beside `Python`. Vitest is used for nothing else.

### 5.5 Exit runs

Checks 1–4 on the final commit: the suite 10 consecutive times on each CI runner with both SDKs under `sdk/` (`ci:repeat`), once on each Lima host (the Lima guest gets the host's Node through the read-only home mount, as it gets uv and Python), and the daemon diff since the `protocol-8` tag empty. Logs go to `tests/conformance/results/`, and the status line gets the PR link.

## Carried limits

All known limits are in [LIMITATIONS.md](../LIMITATIONS.md). Out of phase 5, by design:

- Bun and Deno: Node only. Their `fs` and process APIs differ; each would be its own SDK work.
- Browsers and edge runtimes: no filesystem to capture.
- The LLM auditor and a human review interface beyond `escrow review`: phase 8.
- The pi adapter itself: phase 6.

## Open questions

- [x] Node versions: 22 and 24 (both LTS; 22 leaves maintenance in Apr 2027), one extra conformance job on `ubuntu-24.04`, as Python covers 3.11 and 3.14. Decided Oct 5, 2026.
- [x] Module format: ESM only; Node 22+ loads ESM from CommonJS with `require()`. Decided Oct 5, 2026.
- [x] TypeScript toolchain: TypeScript 7 (7.0.2, the native compiler, latest on Oct 5, 2026), oxlint and oxfmt (OXC) for lint and format, Vitest for tests; versions pinned exactly in `package.json` and `mise.toml` when 5.2 starts. Decided Oct 5, 2026.
- [x] gRPC stack: `@grpc/grpc-js` with generated stubs committed and diffed in CI (it handles `unix:` and `grpc.default_authority`, as grpcio does). Decided Oct 5, 2026.
- [x] Publishing: unpublished; a workspace package that pi's adapter (phase 6) uses from the repo. Decided Oct 5, 2026.
- [x] The conformance suite: one pytest suite split into `daemon/` (Python only, through `escrow.lowlevel`) and `sdk/` (per SDK), driven through a repo task runner. Re-decided Oct 19, 2026; this reverses the Oct 5, 2026 decision to port the suite to TypeScript as a whole.
- [x] Task runner: nx for the whole repo (one task graph, caching, affected-only runs; Node and pnpm needed for every target, both already pinned). Decided Oct 5, 2026 (`make` was the alternative). Built Oct 25, 2026 (5.1), without `nx affected`: CI keeps `paths-filter` and calls `nx run-many -p`.
