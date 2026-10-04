#!/usr/bin/env bash
# Spike 0.5: snapshot at open. Run unprivileged in a VM after setup-volumes.sh.
#   TOOLS_PATH=<node and pnpm dirs> [RUNS=5] [CACHE=/var/tmp/escrow-06-cache] ./run.sh
# Builds the express v5.2.1 tree (clone + pnpm install) on ext4, XFS and btrfs, then:
#   timings: full copy, reflink copy, btrfs subvolume snapshot, stat of every file (version check)
#   isolation: a 0.3 copy-on-write view whose lower is the snapshot must not see later base changes
set -u
here=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$here/../target/release/fuse-cow-spike}
RUNS=${RUNS:-5}
CACHE=${CACHE:-/var/tmp/escrow-06-cache}
RT=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export PATH=${TOOLS_PATH:?set TOOLS_PATH}:$PATH
LOCK=$here/../06-bench/pnpm-lock.yaml
declare -A ROOT=([ext4]=/var/tmp/escrow-05 [xfs]=/mnt/escrow-xfs/escrow-05 [btrfs]=/mnt/escrow-btrfs/escrow-05)
declare -A MECHS=([ext4]="full" [xfs]="full reflink" [btrfs]="full reflink subvol")
fails=0

pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1${2:+: $2}"; fails=$((fails + 1)); }
now() { date +%s.%N; }

snap() { # mech src dst
  case $1 in
    full) cp -a "$2" "$3" ;;
    reflink) cp -a --reflink=always "$2" "$3" ;;
    subvol) btrfs -q subvolume snapshot "$2" "$3" 2>&1 | grep -v 'default subvolume id' ;;
  esac
}
unsnap() { # mech dst
  if [ "$1" = subvol ]; then btrfs -q subvolume delete "$2" 2>&1 | grep -v 'default subvolume id'; else rm -rf -- "$2"; fi
}

build() { # fs
  local r=${ROOT[$1]}
  rm -rf -- "$r"; mkdir -p "$r"
  if [ "$1" = btrfs ]; then btrfs -q subvolume create "$r/proj"; else mkdir "$r/proj"; fi
  (cd "$r/proj" && git clone -q --no-local --branch v5.2.1 "$CACHE/express.git" . 2>/dev/null && cp "$LOCK" pnpm-lock.yaml \
    && XDG_CACHE_HOME=$CACHE/pnpm-cache pnpm install --frozen-lockfile --offline --store-dir "$CACHE/store" --package-import-method copy >/dev/null)
}

isolation() { # fs mech
  local r=${ROOT[$1]} s up view daemon before ok=1
  s=$r/snap-iso up=$r/up-iso
  (cd "$r/proj" && git checkout -q -- . && rm -f added.txt)
  before=$(sha256sum < "$r/proj/Readme.md")
  snap "$2" "$r/proj" "$s"; mkdir -p "$up"
  view=$(mktemp -d "$RT/escrow-05.XXXXXX")
  "$BIN" "$s" "$up" "$view" 2>/dev/null & daemon=$!
  for _ in $(seq 50); do mountpoint -q "$view" && break; sleep 0.1; done
  # Another scope commits to the base after this scope opened.
  echo changed >> "$r/proj/Readme.md"; rm "$r/proj/History.md"; echo new > "$r/proj/added.txt"
  [ "$(sha256sum < "$view/Readme.md")" = "$before" ] || ok=0
  [ -f "$view/History.md" ] || ok=0
  [ ! -e "$view/added.txt" ] || ok=0
  if [ $ok = 1 ]; then pass "$1 $2: scope view does not see a later base commit"; else fail "$1 $2: scope view does not see a later base commit"; fi
  kill "$daemon"; wait "$daemon" 2>/dev/null; fusermount3 -u -z "$view" 2>/dev/null; rmdir "$view"
  unsnap "$2" "$s"; rm -rf -- "$up"
  (cd "$r/proj" && git checkout -q -- . && rm -f added.txt)
}

echo "host: $(. /etc/os-release; echo "$PRETTY_NAME"), kernel $(uname -r), runs $RUNS"
for fs in ext4 xfs btrfs; do
  build "$fs"
  r=${ROOT[$fs]}
  files=$(find "$r/proj" | wc -l); size=$(du -sm "$r/proj" | cut -f1)
  echo "tree fs=$fs entries=$files size_mb=$size"
  for mech in ${MECHS[$fs]}; do
    sync; used0=$(df -m --output=used "$r" | tail -1)
    for i in $(seq "$RUNS"); do
      t0=$(now); snap "$mech" "$r/proj" "$r/snap"; t1=$(now)
      [ "$i" = 1 ] && { sync; echo "space fs=$fs mech=$mech used_mb=$(( $(df -m --output=used "$r" | tail -1) - used0 ))"; }
      unsnap "$mech" "$r/snap"; t2=$(now)
      echo "time fs=$fs mech=$mech op=create secs=$(awk "BEGIN{print $t1-$t0}")"
      echo "time fs=$fs mech=$mech op=delete secs=$(awk "BEGIN{print $t2-$t1}")"
    done
  done
  for i in $(seq "$RUNS"); do
    t0=$(now); find "$r/proj" -printf '%i %s %T@ %C@\n' >/dev/null; t1=$(now)
    echo "time fs=$fs mech=statall op=create secs=$(awk "BEGIN{print $t1-$t0}")"
  done
  for mech in ${MECHS[$fs]}; do isolation "$fs" "$mech"; done
done

# Without a snapshot the scope sees the commit; a per-file version check can only detect it.
r=${ROOT[ext4]}
v0=$(stat -c '%i %s %Y %Z' "$r/proj/Readme.md")
up=$r/up-live; mkdir -p "$up"; view=$(mktemp -d "$RT/escrow-05.XXXXXX")
"$BIN" "$r/proj" "$up" "$view" 2>/dev/null & daemon=$!
for _ in $(seq 50); do mountpoint -q "$view" && break; sleep 0.1; done
sleep 1.1  # let the view's 1 s attribute cache expire after the first lookup
echo changed >> "$r/proj/Readme.md"; sleep 1.1
if grep -q changed "$view/Readme.md"; then pass "live lower: scope view sees the later commit (no isolation)"; else fail "live lower: scope view sees the later commit"; fi
if [ "$(stat -c '%i %s %Y %Z' "$r/proj/Readme.md")" != "$v0" ]; then pass "per-file version check detects the change at commit"; else fail "per-file version check detects the change"; fi
kill "$daemon"; wait "$daemon" 2>/dev/null; fusermount3 -u -z "$view" 2>/dev/null; rmdir "$view"; rm -rf -- "$up"

for fs in ext4 xfs btrfs; do
  r=${ROOT[$fs]}
  [ "$fs" = btrfs ] && btrfs -q subvolume delete "$r/proj" 2>&1 | grep -v 'default subvolume id'
  rm -rf -- "$r"
done
echo "result: $fails failed"
[ "$fails" -eq 0 ]
