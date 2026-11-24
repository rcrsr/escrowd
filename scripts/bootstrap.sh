#!/usr/bin/env bash
# Install the pinned toolchain and every dependency, then wire the git hooks. Idempotent, and
# safe without a terminal (CI, agents): nothing here prompts (APP-MONOREPO-STANDARDS §6.1).
set -euo pipefail
cd "$(dirname "$0")/.."

mise install --locked --yes
eval "$(mise env -s bash)"
pnpm install --frozen-lockfile # its prepare script runs `lefthook install`
uv sync --locked

if [[ "$(uname -s)" == Linux ]] && [[ -r /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]] \
  && [[ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)" == 1 ]] \
  && [[ ! -x /usr/lib/escrowd/bwrap ]]; then
  echo "bootstrap: AppArmor restricts user namespaces here; run packaging/ubuntu/install.sh once (needs sudo)."
fi
echo "bootstrap: done"
