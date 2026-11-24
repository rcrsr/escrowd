#!/usr/bin/env bash
# A foreground debug daemon over a scratch project, for trying the CLI and SDKs by hand:
#   ESCROW_SOCKET=$XDG_RUNTIME_DIR/escrowd-dev/escrow.sock escrow ...
# The scratch tree lives under $XDG_RUNTIME_DIR (views must, see CLAUDE.md); Ctrl-C stops it.
set -euo pipefail
cd "$(dirname "$0")/.."

dev="${XDG_RUNTIME_DIR:?escrowd needs XDG_RUNTIME_DIR}/escrowd-dev"
mkdir -p "$dev/project"
cargo build --locked -p escrow-cli
echo "dev: project $dev/project, socket $dev/escrow.sock"
exec target/debug/escrow daemon --project "$dev/project" --socket "$dev/escrow.sock" --state "$dev/state" --mount "$dev/view"
