#!/usr/bin/env bash
# Spike 0.4 checks: per-scope routing, the contextvars shim, read gating, flush before decision.
#   ./run.sh                                  (system bwrap)
#   BWRAP=/usr/lib/escrowd/bwrap ./run.sh     (Ubuntu with spikes/02-fuse-bwrap/install-ubuntu.sh applied)
set -u
here=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$here/../target/release/fuse-routing-spike}
BWRAP=${BWRAP:-bwrap}
W=$(mktemp -d "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/escrow-04.XXXXXX")
base=$W/proj uppers=$W/uppers mnt=$W/mnt ledger=$W/ledger.log
daemon=

cleanup() {
  [ -n "$daemon" ] && kill "$daemon" 2>/dev/null
  fusermount3 -u -z "$mnt" 2>/dev/null
  [ -n "$W" ] && rm -rf -- "$W"
}
trap cleanup EXIT

fingerprint() { (cd "$1" && find . -printf '%P|%y|%m|%s|%T@|%l\n' | sort && find . -type f -print0 | sort -z | xargs -0 sha256sum); }

echo "host: $(. /etc/os-release; echo "$PRETTY_NAME"), kernel $(uname -r), bwrap $("$BWRAP" --version | cut -d' ' -f2), python $(python3 -c 'import sys; print(sys.version.split()[0])')"

mkdir -p "$base" "$uppers" "$mnt"
echo -n base > "$base/README.md"
echo SECRET=1 > "$base/.env"
echo b > "$base/base.txt"
before=$(fingerprint "$base")

"$BIN" "$base" "$uppers" "$mnt" "$ledger" 2>"$W/daemon.log" &
daemon=$!
for _ in $(seq 50); do mountpoint -q "$mnt" && break; sleep 0.1; done
mountpoint -q "$mnt" || { echo "FAIL  mount: $(tail -1 "$W/daemon.log")"; exit 1; }
echo "PASS  mount routing view"

python3 "$here/python/test_04.py" "$base" "$mnt" "$uppers" "$ledger" "$BWRAP"
rc=$?

if [ "$before" = "$(fingerprint "$base")" ]; then echo "PASS  base byte-identical"; else echo "FAIL  base byte-identical"; rc=1; fi
exit $rc
