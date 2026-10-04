#!/bin/sh
# Check 9: run the conformance suite once in a host-matrix VM (spikes/lima/<host>.yaml).
#   tests/conformance/lima.sh ubuntu-24.04     (also ubuntu-26.04, debian-13, fedora-44)
# The VM has no toolchain: it runs the host's target/debug/escrow and the host's
# mise-installed uv and Python through Lima's read-only home mount, with the virtualenv
# and caches under /var/tmp. Both go by their resolved paths: Debian's sshfs home mount
# fails readlink with EPERM. The log goes to tests/conformance/results/<host>.log.
set -eu
host=$1
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
limactl shell "$vm" env UV="$uv" UV_PYTHON="$py" ROOT="$root" sh "$root/tests/conformance/lima-guest.sh" 2>&1 |
  tee "$root/tests/conformance/results/$host.log"
