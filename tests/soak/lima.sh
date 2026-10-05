#!/bin/sh
# Check 1 on the host matrix: the crash soak in a Lima VM (spikes/lima/<host>.yaml).
#   tests/soak/lima.sh ubuntu-24.04 [RUNS]   (also ubuntu-26.04, debian-13, fedora-44; RUNS 100)
# Runs the host's target/debug/escrow, uv and Python through Lima's read-only home mount,
# as tests/conformance/lima.sh does; the soak's projects live on the VM's local /tmp.
# The log goes to tests/soak/results/crash-<host>.log.
set -eu
host=$1
runs=${2:-100}
root=$(cd "$(dirname "$0")/../.." && pwd)
vm=escrow-$host
uv=$(realpath "$(mise which uv)")
py=$(realpath "$("$uv" python find --project "$root/sdk/python")")
[ -x "$root/target/debug/escrow" ] || {
  echo "build first: cargo build" >&2
  exit 1
}
limactl start "$vm" >/dev/null 2>&1 || true
# Lima home mounts serve stale files after host edits.
limactl shell "$vm" sudo sysctl -q vm.drop_caches=3
limactl shell "$vm" env UV="$uv" UV_PYTHON="$py" ROOT="$root" \
  sh "$root/tests/conformance/lima-guest.sh" tests/soak/crash.py "$runs" 2>&1 |
  tee "$root/tests/soak/results/crash-$host.log"
