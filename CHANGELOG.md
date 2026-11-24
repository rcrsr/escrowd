# Changelog

All notable changes to escrowd. The version lives in `VERSION`; `pnpm run fix:versions` stamps it
into the manifests. escrowd has no releases yet, so every change sits under Unreleased.

## Unreleased

### Changed

- **Layout:** Repository layout follows the app-monorepo standard: `libs/escrowd`, `libs/proto` (new crate
  `escrowd-proto` with the generated protocol code), `libs/python-sdk`, `libs/testkit` and
  `apps/cli`. One uv workspace at the root. ([#43](https://github.com/rcrsr/escrowd/pull/43))
- **Toolchain and dependencies:** Rust 1.100, Python 3.15 (uv-managed), Node 26, nx 24, pnpm 12.23. Dependencies:
  fuser 0.19, rusqlite 0.43, serde-saphyr 2, parking_lot 0.13, grpcio 1.85.3. Python SDK requires
  `grpcio>=1.85.3`. ([#43](https://github.com/rcrsr/escrowd/pull/43))
- **Lint rules:** clippy pedantic and the extended ruff rule set are enforced; findings were fixed, not allowed. ([#43](https://github.com/rcrsr/escrowd/pull/43))
- **Locked CI:** Every tool is locked in `mise.lock`. CI runs on Nx affected projects with SHA-pinned actions,
  cargo-deny, gitleaks, osv-scanner, typos, zizmor and an 80% changed-line coverage gate. The daemon-frozen
  check now also covers `libs/proto/`. ([#43](https://github.com/rcrsr/escrowd/pull/43))

### Fixed

- **Test temp files:** Rust tests and the conformance suite no longer leak temporary directories. ([#43](https://github.com/rcrsr/escrowd/pull/43))
