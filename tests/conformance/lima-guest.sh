#!/bin/sh
# The guest half of lima.sh (and tests/soak/lima.sh): runs inside the VM with UV,
# UV_PYTHON and ROOT set. Arguments: a Python script and its arguments to run instead of
# the conformance suite.
set -eu
# shellcheck source=/dev/null
. /etc/os-release
echo "host: $PRETTY_NAME, kernel $(uname -r), $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [ "$ID" = ubuntu ]; then
  "$ROOT/packaging/ubuntu/install.sh" # escrowd's bwrap and AppArmor profile
fi
export UV_PROJECT_ENVIRONMENT=/var/tmp/escrow-venv UV_CACHE_DIR=/var/tmp/uv-cache
export UV_PYTHON_DOWNLOADS=never PYTHONDONTWRITEBYTECODE=1
cd "$ROOT"
if [ $# -gt 0 ]; then
  exec "$UV" run --project sdk/python --frozen python "$@"
fi
"$UV" run --project sdk/python --frozen pytest -v -p no:cacheprovider tests/conformance
