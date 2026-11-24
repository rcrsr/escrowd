#!/usr/bin/env bash
# Changed-line coverage, failing under 80% (APP-TS-5). The daemon's real coverage comes from the
# conformance suite driving the binary, so the suite runs against an llvm-cov instrumented build,
# beside the Rust unit tests and pytest-cov on the Python packages. Reports land in target/coverage.
set -euo pipefail
cd "$(dirname "$0")/.."

base="${NX_BASE:-origin/main}"
out=target/coverage
mkdir -p "$out"

eval "$(cargo llvm-cov show-env --sh)"
cargo llvm-cov clean --workspace
cargo build --locked -p escrow-cli
cargo nextest run --locked --workspace --no-tests=pass
ESCROW_BIN="$PWD/${CARGO_TARGET_DIR:-target}/debug/escrow" uv run --no-sync pytest -n 0 --rootdir tests/conformance tests/conformance \
  --cov=libs/python-sdk/src --cov=libs/testkit --cov-report="lcov:$out/python.lcov" --cov-fail-under=0
# Children whose environment the suite clears write default_*.profraw into the cwd; fold them in.
shopt -s nullglob
stray=(default_*.profraw)
if ((${#stray[@]})); then mv "${stray[@]}" "$(dirname "$LLVM_PROFILE_FILE")/"; fi
cargo llvm-cov report --lcov --output-path "$out/rust.lcov"
# One merged LCOV report: diff-cover drops files when given several reports (11.0).
{
  cat "$out/rust.lcov"
  echo
  cat "$out/python.lcov"
} >"$out/all.lcov"
uv run --no-sync diff-cover "$out/all.lcov" --compare-branch="$base" --fail-under=80 \
  --exclude 'libs/python-sdk/src/escrow/v1/*' --html-report "$out/diff-cover.html"
