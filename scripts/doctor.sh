#!/usr/bin/env bash
# Toolchain health: locked tools installed, the Rust pin in force, host features escrowd needs.
# Prints one line per check and exits 1 if any check fails.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

failed=0
ok() { printf '  ok    %s\n' "$1"; }
bad() {
  printf '  FAIL  %s\n' "$1"
  failed=1
}

if mise install --locked --dry-run >/dev/null 2>&1; then ok "mise tools match mise.lock"; else bad "mise tools: run make bootstrap"; fi
eval "$(mise env -s bash)"

want=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
have=$(rustc --version 2>/dev/null | awk '{print $2}')
if [[ "$have" == "$want" ]]; then ok "rustc $have"; else bad "rustc ${have:-missing}, want $want (a stale RUSTUP_TOOLCHAIN in this shell?)"; fi

if uv sync --locked --dry-run >/dev/null 2>&1; then ok "uv.lock in sync"; else bad "uv.lock out of sync: run uv lock"; fi
if [[ -d node_modules/nx ]]; then ok "pnpm install done"; else bad "node_modules missing: run make bootstrap"; fi
if [[ -f .git/hooks/pre-commit ]] && grep -q lefthook .git/hooks/pre-commit; then ok "lefthook hooks installed"; else bad "git hooks missing: run lefthook install"; fi

if command -v fusermount3 >/dev/null; then ok "fusermount3"; else bad "fusermount3 missing: install fuse3"; fi
if command -v bwrap >/dev/null || [[ -x /usr/lib/escrowd/bwrap ]]; then ok "bwrap"; else bad "bwrap missing: install bubblewrap"; fi
kernel=$(uname -r | cut -d. -f1,2)
if printf '6.8\n%s\n' "$kernel" | sort -V -C; then ok "kernel $kernel"; else bad "kernel $kernel, escrowd needs 6.8 or later"; fi

outdated=$(mise outdated --bump 2>/dev/null | tail -n +2)
if [[ -z "$outdated" ]]; then ok "no newer tool releases"; else printf '  info  newer releases:\n%s\n' "$outdated"; fi
exit "$failed"
