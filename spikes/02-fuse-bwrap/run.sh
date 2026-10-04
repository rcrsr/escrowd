#!/usr/bin/env bash
# Spike 0.2 checks. Runs on the dev host or inside a Lima VM.
#   BWRAP=/usr/lib/escrowd/bwrap ./run.sh     (Ubuntu with install-ubuntu.sh applied)
#   ./run.sh                                  (system bwrap)
# Prints one PASS/FAIL line per check; exits 1 if any check fails.
set -u
here=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$here/../target/release/fuse-bwrap-spike}
BWRAP=${BWRAP:-bwrap}
# Ubuntu 26.04 confines fusermount3 to mountpoints under $HOME, /mnt, /run/user/<uid>, /media
# and /tmp, so work under the runtime dir, as escrowd will.
W=$(mktemp -d "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/escrow-02.XXXXXX")
base=$W/proj
view=$W/view
fails=0
pids=()

pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1${2:+: $2}"; fails=$((fails + 1)); }
check() { local name=$1; shift; if out=$("$@" 2>&1); then pass "$name"; else fail "$name" "$(echo "$out" | tail -1)"; fi; }

cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  for m in "$view" "$W/loop"; do fusermount3 -u -z "$m" 2>/dev/null; done
  rm -rf "$W"
}
trap cleanup EXIT

start_daemon() { # base view
  "$BIN" "$1" "$2" 2>>"$W/daemon.log" &
  pids+=($!)
  for _ in $(seq 50); do mountpoint -q "$2" && return 0; sleep 0.1; done
  return 1
}

sandbox() { # command run as sh -c inside the sandbox, with the view over $base
  "$BWRAP" --unshare-user --disable-userns --unshare-pid --die-with-parent \
    --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
    --bind "$view" "$base" --chdir "$base" \
    --setenv GIT_AUTHOR_NAME spike --setenv GIT_AUTHOR_EMAIL spike@example.invalid \
    --setenv GIT_COMMITTER_NAME spike --setenv GIT_COMMITTER_EMAIL spike@example.invalid \
    -- sh -c "$1"
}

echo "host: $(. /etc/os-release; echo "$PRETTY_NAME"), kernel $(uname -r), bwrap $("$BWRAP" --version | cut -d' ' -f2)"

mkdir -p "$base" "$view"
echo seed > "$base/seed.txt"

# 1. Mount without root through fusermount3.
if start_daemon "$base" "$view"; then pass "mount view unprivileged ($(findmnt -n -o FSTYPE "$view"))"; else fail "mount view unprivileged" "$(tail -1 "$W/daemon.log")"; exit 1; fi

# 2. Inside bwrap, the view covers $base and ordinary tools work on ordinary paths.
check "sandbox sees view at project path" sandbox "test \"\$(findmnt -n -o FSTYPE --target $base)\" = fuse.escrow && test \"\$(pwd)\" = $base"
check "read existing file" sandbox 'test "$(cat seed.txt)" = seed'
check "bash redirect write" sandbox 'echo bash > bash.txt && test "$(cat bash.txt)" = bash'
check "heredoc write" sandbox 'cat > heredoc.txt <<EOF
heredoc
EOF
test "$(cat heredoc.txt)" = heredoc'
check "python write" sandbox 'python3 -c "open(\"py.txt\",\"w\").write(\"py\")" && test "$(cat py.txt)" = py'
check "rename and delete" sandbox 'mkdir d && mv bash.txt d/moved.txt && test -f d/moved.txt && rm d/moved.txt && rmdir d'
check "git init, commit, clean status" sandbox 'git -c init.defaultBranch=main init -q && git add -A && git commit -qm spike && test -z "$(git status --porcelain)"'
check "nested user namespace blocked" sandbox '! unshare -Urm true 2>/dev/null'
check "daemon not visible (pid namespace)" sandbox "! ls /proc | grep -qx '${pids[0]}'"

# 3. Mirroring: writes landed in the base (0.3 replaces this with copy-on-write).
check "writes reached base" test -f "$base/py.txt" -a -f "$base/heredoc.txt" -a -d "$base/.git"

# 4. Pre-opened base: a view mounted over its own base path does not loop.
mkdir -p "$W/loop" && echo loop > "$W/loop/seed.txt"
if start_daemon "$W/loop" "$W/loop"; then
  check "view over its own base path serves reads" timeout 10 sh -c "test \"\$(cat $W/loop/seed.txt)\" = loop"
  check "view over its own base path takes writes" timeout 10 sh -c "echo w > $W/loop/w.txt && test \"\$(cat $W/loop/w.txt)\" = w"
else
  fail "mount view over its own base path" "$(tail -1 "$W/daemon.log")"
fi

# 5. Daemon death: the sandbox's bind turns stale and reports ENOTCONN.
sandbox 'for i in $(seq 40); do ls . >/dev/null 2>/tmp/err || { cat /tmp/err; exit 0; }; sleep 0.25; done; echo still-alive' >"$W/stale.out" 2>&1 &
sb=$!
sleep 1
kill -9 "${pids[0]}"
wait "$sb"
if grep -q "not connected" "$W/stale.out"; then pass "daemon death detected in sandbox (ENOTCONN)"; else fail "daemon death detected in sandbox" "$(tail -1 "$W/stale.out")"; fi

echo "result: $fails failed"
[ "$fails" -eq 0 ]
