#!/usr/bin/env bash
# Spike 0.6: overhead of the 0.3 copy-on-write FUSE view against native, on express v5.2.1.
#   TOOLS_PATH=<dirs with node and pnpm> [RUNS=5] [MODES="native fuse"] [BWRAP=...] ./run.sh
# Modes: native; fuse (spike defaults: 1 request thread, 1 s cache, page cache dropped on open);
# fuse-cache (keep page cache across opens, 60 s entry/attr cache); fuse-tuned (fuse-cache + 8 threads); bindfs (a libfuse mirror without copy-on-write, as a floor for plain FUSE cost);
# bwrap (the sandbox without FUSE, to separate sandbox cost from FUSE cost).
# Setup (network allowed) fills $CACHE once: a git mirror, a pnpm store and pnpm's metadata cache.
# Timed runs are offline. Store and cache live outside the project, as in escrowd.
# Workload A, from an empty project: git clone, pnpm install (copy import), pnpm test.
# Workload B, on a tree already in the base layer: git status, read every file, pnpm test.
set -u
here=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$here/../target/release/fuse-cow-spike}
BWRAP=${BWRAP:-bwrap}
RUNS=${RUNS:-5}
MODES=${MODES:-native fuse}
CACHE=${CACHE:-/var/tmp/escrow-06-cache}
RT=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export PATH=${TOOLS_PATH:?set TOOLS_PATH to the node and pnpm directories}:$PATH
LOCK=$here/pnpm-lock.yaml
TAG=v5.2.1

setup() {
  mkdir -p "$CACHE"
  [ -d "$CACHE/express.git" ] || git clone -q --mirror https://github.com/expressjs/express "$CACHE/express.git"
  if [ ! -d "$CACHE/pnpm-cache" ]; then
    rm -rf -- "$CACHE/src" "$CACHE/store"
    git clone -q --branch "$TAG" "$CACHE/express.git" "$CACHE/src" 2>/dev/null
    cp "$LOCK" "$CACHE/src/pnpm-lock.yaml"
    (cd "$CACHE/src" && XDG_CACHE_HOME=$CACHE/pnpm-cache pnpm fetch --store-dir "$CACHE/store" >/dev/null)
  fi
}

# Steps run inside the project dir; print step=seconds pairs.
read -r -d '' STEPS_A <<EOF
set -e
export XDG_CACHE_HOME=$CACHE/pnpm-cache XDG_STATE_HOME=\$PWD/../xdg XDG_DATA_HOME=\$PWD/../xdg
t() { date +%s.%N; }
t0=\$(t); git clone -q --no-local --branch $TAG $CACHE/express.git . 2>/dev/null
t1=\$(t); cp $LOCK pnpm-lock.yaml && pnpm install --frozen-lockfile --offline --store-dir $CACHE/store --package-import-method copy >/dev/null
t2=\$(t); pnpm test >../test.log 2>&1 || true
t3=\$(t)
echo "clone=\$(awk "BEGIN{print \$t1-\$t0}") install=\$(awk "BEGIN{print \$t2-\$t1}") test=\$(awk "BEGIN{print \$t3-\$t2}") passing=\$(grep -o '[0-9]* passing' ../test.log | cut -d' ' -f1)"
EOF
read -r -d '' STEPS_B <<EOF
set -e
export XDG_CACHE_HOME=$CACHE/pnpm-cache XDG_STATE_HOME=\$PWD/../xdg XDG_DATA_HOME=\$PWD/../xdg
t() { date +%s.%N; }
t0=\$(t); git status --porcelain >/dev/null
t1=\$(t); tar cf - . | wc -c >/dev/null
t2=\$(t); pnpm test >../test.log 2>&1 || true
t3=\$(t)
echo "status=\$(awk "BEGIN{print \$t1-\$t0}") readall=\$(awk "BEGIN{print \$t2-\$t1}") test=\$(awk "BEGIN{print \$t3-\$t2}") passing=\$(grep -o '[0-9]* passing' ../test.log | cut -d' ' -f1)"
EOF

# run <mode native|fuse> <workload A|B>
run() {
  local mode=$1 wl=$2 D proj steps out daemon= view=
  D=$(mktemp -d /var/tmp/escrow-06.XXXXXX)
  proj=$D/proj
  mkdir -p "$proj" "$D/upper" "$D/xdg"
  steps=$STEPS_A
  if [ "$wl" = B ]; then
    steps=$STEPS_B
    (cd "$proj" && git clone -q --no-local --branch "$TAG" "$CACHE/express.git" . 2>/dev/null && cp "$LOCK" pnpm-lock.yaml \
      && XDG_CACHE_HOME=$CACHE/pnpm-cache pnpm install --frozen-lockfile --offline --store-dir "$CACHE/store" --package-import-method copy >/dev/null)
  fi
  if [ "$mode" = native ]; then
    out=$(cd "$proj" && bash -c "$steps" 2>&1)
  else
    view=$(mktemp -d "$RT/escrow-06.XXXXXX")
    case $mode in
      bwrap) rmdir "$view"; view=$proj ;;
      fuse) "$BIN" "$proj" "$D/upper" "$view" 2>"$D/daemon.log" & daemon=$! ;;
      fuse-cache) ESCROW_KEEP_CACHE=1 ESCROW_TTL_SECS=60 "$BIN" "$proj" "$D/upper" "$view" 2>"$D/daemon.log" & daemon=$! ;;
      fuse-tuned) ESCROW_KEEP_CACHE=1 ESCROW_THREADS=8 ESCROW_TTL_SECS=60 "$BIN" "$proj" "$D/upper" "$view" 2>"$D/daemon.log" & daemon=$! ;;
      bindfs) bindfs --no-allow-other "$proj" "$view" ;;
    esac
    for _ in $(seq 50); do mountpoint -q "$view" && break; sleep 0.1; done
    if [ "$mode" != bwrap ] && ! mountpoint -q "$view"; then
      echo "run=$i mode=$mode workload=$wl error=mount-failed"
      [ -n "$daemon" ] && kill "$daemon" 2>/dev/null
      rmdir "$view" 2>/dev/null; [ -n "$D" ] && rm -rf -- "$D"
      return
    fi
    out=$("$BWRAP" --unshare-user --disable-userns --unshare-pid --die-with-parent \
      --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --bind "$D" "$D" --bind "$CACHE/pnpm-cache" "$CACHE/pnpm-cache" --bind "$CACHE/store" "$CACHE/store" --bind "$view" "$proj" --chdir "$proj" \
      -- bash -c "$steps" 2>&1)
    [ -n "$daemon" ] && { kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null; }
    [ "$mode" != bwrap ] && { fusermount3 -u -z "$view" 2>/dev/null; rmdir "$view" 2>/dev/null; }
  fi
  echo "run=$i mode=$mode workload=$wl $(echo "$out" | tail -1)"
  [ -n "$D" ] && rm -rf -- "$D"
}

echo "host: $(. /etc/os-release; echo "$PRETTY_NAME"), kernel $(uname -r), $(nproc) CPUs, node $(node --version), pnpm $(pnpm --version), runs $RUNS"
setup
read -r -a modes <<<"$MODES"
for i in $(seq "$RUNS"); do
  for wl in A B; do
    # Rotate the mode order every run so drift (thermal, caches) does not favor one mode.
    n=${#modes[@]}
    for k in $(seq 0 $((n - 1))); do run "${modes[$(((k + i) % n))]}" $wl; done
  done
done
