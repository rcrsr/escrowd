#!/bin/sh
# Run bench/run.sh in the benchmark VM (spikes/lima/bench-ubuntu-24.04.yaml).
#   [RUNS=7] bench/lima.sh   (log: bench/results/bench-ubuntu-24.04.log)
# The VM has no toolchain or mise: the host resolves node, pnpm, uv and Python to real
# paths (Lima's sshfs home mount fails readlink) and the VM runs them read-only.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
vm=escrow-bench
[ -x "$root/target/release/escrow" ] || {
  echo "build first: cargo build --release" >&2
  exit 1
}
cd "$root"
uv=$(realpath "$(mise which uv)")
py=$(realpath "$("$uv" python find 3.14)")
node=$(realpath "$(mise which node)")
pnpm=$(realpath "$(mise which pnpm)")
rg=$(realpath "$(mise which rg)")
limactl start "$vm" >/dev/null 2>&1 || true
limactl shell "$vm" sudo sysctl -q vm.drop_caches=3
limactl shell "$vm" sudo "$root/packaging/ubuntu/install.sh" >/dev/null
limactl shell "$vm" env RUNS="${RUNS:-7}" UV="$uv" PY="$py" PYALIAS="$(dirname "$(dirname "$py")")" \
  NODE="$node" PNPM="$pnpm" RG="$rg" bash "$root/bench/run.sh" >"$root/bench/results/bench-ubuntu-24.04.log" 2>&1
python3 "$root/bench/summarize.py" <"$root/bench/results/bench-ubuntu-24.04.log"
