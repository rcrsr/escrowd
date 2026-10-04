#!/usr/bin/env bash
# shellcheck disable=SC2016 # the step scripts are expanded by their own shell
# Phase 2 benchmark: real repositories natively and under `escrow run` (release build).
#   [RUNS=7] [MODES="native sandbox escrow"] [JOBS="express:A …"] bench/run.sh > log
#   python3 bench/summarize.py < log
# Modes: native; sandbox (`escrow run --unscoped passthrough`: daemon and bwrap, the
# project bound directly, no FUSE); escrow (`--unscoped implicit --on-exit commit`: every
# write lands in one scope, committed at exit).
# Workload A, from an empty project: clone, install, test.
# Workload B, on a tree already in the base: status, read every file, test.
# Workload C, read-heavy, on a large tree already in the base (CPython, about 5,000
# files): rg, git status, git log -p -n 50, each cold (first touch after mount) and then
# warm (again, same scope).
# JOBS lists repo:workload pairs; the default runs A and B on express and attrs, C on CPython.
# Setup (network allowed) fills $CACHE once: git mirrors, the pnpm store and metadata
# cache, the uv cache. Timed runs are offline. Mirrors are sandbox.read; the stores and
# caches are sandbox.write (outside escrow, as package stores are).
# Tools (node, pnpm, uv, Python) come from mise by resolved path, so the run works over
# Lima's read-only home mount, where sshfs fails readlink.
set -u
here=$(cd "$(dirname "$0")" && pwd)
ESCROW=${ESCROW:-$here/../target/release/escrow}
RUNS=${RUNS:-7}
MODES=${MODES:-native sandbox escrow}
JOBS=${JOBS:-express:A express:B attrs:A attrs:B cpython:C}
CACHE=${CACHE:-/var/tmp/escrow-bench-cache}
RT=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
NODE=$(dirname "$(realpath "${NODE:-$(mise which node)}")")
PNPM=$(dirname "$(realpath "${PNPM:-$(mise which pnpm)}")")
RG=$(dirname "$(realpath "${RG:-$(mise which rg)}")")
UV=$(realpath "${UV:-$(mise which uv)}")
PYLINK=${PY:-$("$UV" python find 3.14)}
PY=$(realpath "$PYLINK")
# Virtualenvs link to uv's alias directory (cpython-3.14-…), a symlink: bind both.
PYHOME=$(dirname "$(dirname "$PY")")
PYALIAS=${PYALIAS:-$(dirname "$(dirname "$PYLINK")")}
PATH=$NODE:$PNPM:$RG:$(dirname "$UV"):$PATH
export PATH UV_PYTHON=$PY UV_CACHE_DIR=$CACHE/uv-cache
export UV_PYTHON_DOWNLOADS=never UV_LINK_MODE=copy
EXPRESS_TAG=v5.2.1 ATTRS_TAG=26.1.0 CPYTHON_TAG=v3.14.8

[ -x "$ESCROW" ] || {
  echo "build first: cargo build --release" >&2
  exit 1
}

setup() {
  mkdir -p "$CACHE"
  [ -d "$CACHE/express.git" ] || git clone -q --mirror https://github.com/expressjs/express "$CACHE/express.git"
  [ -d "$CACHE/attrs.git" ] || git clone -q --mirror https://github.com/python-attrs/attrs "$CACHE/attrs.git"
  # Shallow: workload C reads 50 commits of history, not 100,000.
  [ -d "$CACHE/cpython.git" ] || git clone -q --bare --depth 100 --branch "$CPYTHON_TAG" \
    https://github.com/python/cpython "$CACHE/cpython.git"
  if [ ! -d "$CACHE/pnpm-cache" ]; then
    rm -rf -- "$CACHE/src" "$CACHE/store"
    git clone -q --branch "$EXPRESS_TAG" "$CACHE/express.git" "$CACHE/src" 2>/dev/null
    cp "$here/express-pnpm-lock.yaml" "$CACHE/src/pnpm-lock.yaml"
    (cd "$CACHE/src" && XDG_CACHE_HOME=$CACHE/pnpm-cache pnpm fetch --store-dir "$CACHE/store" >/dev/null)
    rm -rf -- "$CACHE/src"
  fi
  if [ ! -d "$CACHE/uv-cache" ]; then
    git clone -q --branch "$ATTRS_TAG" "$CACHE/attrs.git" "$CACHE/src" 2>/dev/null
    (cd "$CACHE/src" && uv sync -q --frozen --no-default-groups --group tests)
    rm -rf -- "$CACHE/src"
  fi
}

# Steps run in the project directory and print step=seconds pairs and passing=N.
# attrs skips tests/test_pyright.py: it needs pyright on PATH, which the sandbox lacks.
# Logs and XDG dirs go to a private temp dir (the sandbox's /tmp is its own).
COMMON='set -e
X=$(mktemp -d)
export XDG_STATE_HOME=$X XDG_DATA_HOME=$X
t() { date +%s.%N; }
d() { awk "BEGIN{print $2-$1}"; }'

express_install="cp $here/express-pnpm-lock.yaml pnpm-lock.yaml && XDG_CACHE_HOME=$CACHE/pnpm-cache pnpm install --frozen-lockfile --offline --store-dir $CACHE/store --package-import-method copy >/dev/null"
express_test='XDG_CACHE_HOME='$CACHE'/pnpm-cache pnpm test >$X/test.log 2>&1 || true; passing=$(grep -o "[0-9]* passing" $X/test.log | cut -d" " -f1)'
attrs_install='uv sync -q --frozen --offline --no-default-groups --group tests'
attrs_test='uv run -q --frozen --offline --no-sync pytest -q -p no:cacheprovider --ignore tests/test_pyright.py >$X/test.log 2>&1 || true; passing=$(grep -o "[0-9]* passed" $X/test.log | cut -d" " -f1)'

steps() { # steps <repo> <A|B>
  local repo=$1 wl=$2 tag install test
  case $repo in
  express) tag=$EXPRESS_TAG install=$express_install test=$express_test ;;
  attrs) tag=$ATTRS_TAG install=$attrs_install test=$attrs_test ;;
  cpython) tag=$CPYTHON_TAG install=true test=true ;;
  esac
  echo "$COMMON"
  if [ "$wl" = clone ]; then # the base tree for workload C
    echo "git clone -q --no-local --branch $tag $CACHE/$repo.git . 2>/dev/null"
  elif [ "$wl" = C ]; then
    cat <<EOF2
r() { rg -c return . >/dev/null || true; }
s() { git status --porcelain >/dev/null; }
l() { git log -p -n 50 >/dev/null; }
t0=\$(t); r
t1=\$(t); s
t2=\$(t); l
t3=\$(t); r
t4=\$(t); s
t5=\$(t); l
t6=\$(t)
echo "rg_cold=\$(d \$t0 \$t1) status_cold=\$(d \$t1 \$t2) log_cold=\$(d \$t2 \$t3) rg_warm=\$(d \$t3 \$t4) status_warm=\$(d \$t4 \$t5) log_warm=\$(d \$t5 \$t6)"
EOF2
  elif [ "$wl" = A ]; then
    cat <<EOF2
t0=\$(t); git clone -q --no-local --branch $tag $CACHE/$repo.git . 2>/dev/null
t1=\$(t); $install
t2=\$(t); $test
t3=\$(t)
echo "clone=\$(d \$t0 \$t1) install=\$(d \$t1 \$t2) test=\$(d \$t2 \$t3) passing=\$passing"
EOF2
  else
    cat <<EOF2
t0=\$(t); git status --porcelain >/dev/null
t1=\$(t); tar cf - . | wc -c >/dev/null
t2=\$(t); $test
t3=\$(t)
echo "status=\$(d \$t0 \$t1) readall=\$(d \$t1 \$t2) test=\$(d \$t2 \$t3) passing=\$passing"
EOF2
  fi
}

run() { # run <mode> <repo> <A|B>
  local mode=$1 repo=$2 wl=$3 D proj script out t0 t1 rt
  D=$(mktemp -d /var/tmp/escrow-bench.XXXXXX)
  proj=$D/proj
  mkdir -p "$proj"
  script=$(steps "$repo" "$wl")
  case $wl in # the tree is in the base before the timed steps: installed for B, cloned for C
  B) (cd "$proj" && bash -c "$(steps "$repo" A)" >/dev/null 2>&1) ;;
  C) (cd "$proj" && bash -c "$(steps "$repo" clone)" >/dev/null 2>&1) ;;
  esac
  cat >"$D/policy.yaml" <<EOF2
version: 1
sandbox:
  read: ['$here', '$CACHE/express.git', '$CACHE/attrs.git', '$CACHE/cpython.git', '$NODE', '$PNPM', '$RG', '$(dirname "$UV")', '$PYHOME', '$PYALIAS']
  write: ['$CACHE/store', '$CACHE/pnpm-cache', '$CACHE/uv-cache']
EOF2
  rt=$(mktemp -d "$RT/escrow-bench.XXXXXX")
  t0=$(date +%s.%N)
  case $mode in
  native) out=$(cd "$proj" && bash -c "$script" 2>&1) ;;
  sandbox | escrow)
    local unscoped=passthrough
    [ "$mode" = escrow ] && unscoped=implicit
    out=$(cd "$proj" && "$ESCROW" run --project "$proj" --unscoped "$unscoped" --on-exit commit \
      --policy "$D/policy.yaml" --state "$D/state" --mount "$rt/view" --socket "$rt/s.sock" \
      -- bash -c "$script" 2>&1)
    ;;
  esac
  t1=$(date +%s.%N)
  echo "run=$i mode=$mode repo=$repo workload=$wl $(echo "$out" | grep -E '^(clone|status|rg_cold)=' | tail -1) wall=$(awk "BEGIN{print $t1-$t0}")"
  if [ "$mode" = escrow ] && ! echo "$out" | grep -q "change(s) committed"; then
    echo "run=$i mode=$mode repo=$repo workload=$wl error=no-commit $(echo "$out" | tail -3 | tr '\n' ' ')"
  fi
  fusermount3 -u -z "$rt/view" 2>/dev/null
  rm -rf -- "$D" "$rt"
}

# shellcheck source=/dev/null
echo "host: $(. /etc/os-release && echo "$PRETTY_NAME"), kernel $(uname -r), $(nproc) CPUs, node $(node --version), pnpm $(pnpm --version), $(rg --version | head -1 | cut -d' ' -f1-2), $("$PY" --version), escrow $("$ESCROW" --version | cut -d' ' -f2) ($(git -C "$here" describe --always --dirty 2>/dev/null || git -C "$here" rev-parse --short HEAD)), runs $RUNS"
setup
read -r -a modes <<<"$MODES"
for i in $(seq "$RUNS"); do
  for job in $JOBS; do
    # Rotate the mode order every run so drift (thermal, caches) does not favor one mode.
    n=${#modes[@]}
    for k in $(seq 0 $((n - 1))); do run "${modes[$(((k + i) % n))]}" "${job%:*}" "${job#*:}"; done
  done
done
