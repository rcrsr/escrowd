#!/usr/bin/env bash
# Spike 0.3 checks: copy-on-write view over a read-only base.
#   ./run.sh                                  (system bwrap)
#   BWRAP=/usr/lib/escrowd/bwrap ./run.sh     (Ubuntu with spikes/02-fuse-bwrap/install-ubuntu.sh applied)
# One scenario runs twice: on a native copy of the project and inside the view.
# Prints one PASS/FAIL line per check; exits 1 if any check fails.
set -u
here=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$here/../target/release/fuse-cow-spike}
BWRAP=${BWRAP:-bwrap}
W=$(mktemp -d "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/escrow-03.XXXXXX")
base=$W/proj upper=$W/upper view=$W/view native=$W/native
fails=0
daemon=

pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1${2:+: $2}"; fails=$((fails + 1)); }
check() { local name=$1; shift; if out=$("$@" 2>&1); then pass "$name"; else fail "$name" "$(echo "$out" | tail -1)"; fi; }
same() { if [ "$2" = "$3" ]; then pass "$1"; else fail "$1" "$(diff <(echo "$2") <(echo "$3") | head -3 | tr '\n' ' ')"; fi; }

cleanup() {
  [ -n "$daemon" ] && kill "$daemon" 2>/dev/null
  fusermount3 -u -z "$view" 2>/dev/null
  [ -n "$W" ] && rm -rf -- "$W"
}
trap cleanup EXIT

export GIT_AUTHOR_NAME=spike GIT_AUTHOR_EMAIL=spike@example.invalid
export GIT_COMMITTER_NAME=spike GIT_COMMITTER_EMAIL=spike@example.invalid
export GIT_AUTHOR_DATE=2026-10-03T12:00:00Z GIT_COMMITTER_DATE=2026-10-03T12:00:00Z

# Names, types, modes, symlink targets and content hashes; no sizes (dir sizes vary by fs), times or inodes.
listing() { (cd "$1" && find . -path ./.git -prune -o -printf '%P|%y|%m|%l\n' | sort && find . -path ./.git -prune -o -type f -print0 | sort -z | xargs -0 sha256sum); }
# Everything in the base including .git, with mtimes: any write to the base changes it.
fingerprint() { (cd "$1" && find . -printf '%P|%y|%m|%s|%T@|%l\n' | sort && find . -type f -print0 | sort -z | xargs -0 sha256sum); }

scenario='set -e
echo appended >> a.txt
mv dir/b.txt dir/b2.txt
rm dir/sub/c.txt
rm -r dir/sub
mkdir dir/sub
test -z "$(ls -A dir/sub)"
mkdir -p new/deep && echo x > new/deep/x.txt
chmod 600 a.txt
: > trunc.txt
ln -s a.txt link2
printf "edited\n" > .ed.tmp && mv .ed.tmp edit.txt
mv vim.txt vim.txt~ && printf "vim new\n" > vim.txt && rm vim.txt~
git add -A && git commit -qm scenario
git status --porcelain'

echo "host: $(. /etc/os-release; echo "$PRETTY_NAME"), kernel $(uname -r), bwrap $("$BWRAP" --version | cut -d' ' -f2)"

# Base project, committed to git before anything is mounted.
mkdir -p "$base/dir/sub" "$upper" "$view"
(cd "$base" && echo a > a.txt && echo b > dir/b.txt && echo c > dir/sub/c.txt && echo t > trunc.txt \
  && echo e > edit.txt && echo v > vim.txt && echo s > stable.txt && printf '#!/bin/sh\n' > exec.sh \
  && chmod 755 exec.sh && ln -s a.txt link && git -c init.defaultBranch=main init -q && git add -A && git commit -qm base)
cp -a "$base" "$native"
before=$(fingerprint "$base")
lower_ino=$(stat -c %i "$base/stable.txt")

"$BIN" "$base" "$upper" "$view" 2>"$W/daemon.log" &
daemon=$!
for _ in $(seq 50); do mountpoint -q "$view" && break; sleep 0.1; done
if mountpoint -q "$view"; then pass "mount copy-on-write view"; else fail "mount copy-on-write view" "$(tail -1 "$W/daemon.log")"; exit 1; fi

sandbox() {
  "$BWRAP" --unshare-user --disable-userns --unshare-pid --die-with-parent \
    --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --bind "$view" "$base" --chdir "$base" -- sh -c "$1"
}

# Inode numbers: the lower st_ino shows through, survives copy-up and survives rename.
check "inode equals lower inode" sandbox "test \$(stat -c %i stable.txt) = $lower_ino"
check "inode stable across copy-up" sandbox "echo more >> stable.txt && test \$(stat -c %i stable.txt) = $lower_ino"
check "inode stable across rename" sandbox "mv stable.txt stable2.txt && test \$(stat -c %i stable2.txt) = $lower_ino && mv stable2.txt stable.txt"

# The same scenario natively and in the view must give the same tree and git status.
(cd "$native" && echo more >> stable.txt)  # mirror the inode checks' edit
native_status=$(cd "$native" && sh -c "$scenario" 2>&1)
view_status=$(sandbox "$scenario" 2>&1)
check "scenario runs in view" test -z "$(echo "$view_status" | grep -v '^$')"
same "git status matches native" "$native_status" "$view_status"
same "tree matches native" "$(listing "$native")" "$(sandbox "$(declare -f listing); listing .")"
same "git log matches native" "$(git -C "$native" log --format='%T %s')" "$(sandbox 'git log --format="%T %s"')"

# Whiteouts and opaque directories.
check "deleted lower file hidden" sandbox 'test ! -e dir/sub/c.txt && test ! -e dir/b.txt'
check "recreated dir hides lower contents" sandbox 'test -d dir/sub && test -z "$(ls -A dir/sub)"'

# Lower-backed directories rename with EXDEV, like overlayfs; mv falls back to copy and delete.
check "mv of lower directory (EXDEV fallback)" sandbox 'mv dir dir-moved && test -f dir-moved/b2.txt && test ! -e dir'

# The base never changed; the upper holds the changes.
same "base byte-identical (incl. .git, mtimes)" "$before" "$(fingerprint "$base")"
check "upper holds staged changes" test -f "$upper/a.txt" -a -f "$upper/new/deep/x.txt" -a -d "$upper/.git"
check "upper has no copy of untouched files" test ! -e "$upper/exec.sh"

echo "result: $fails failed"
[ "$fails" -eq 0 ]
