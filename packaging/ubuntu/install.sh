#!/bin/sh
# Install escrowd's bwrap and its AppArmor profile (Ubuntu only; needs sudo).
#   install-ubuntu.sh          install and load
#   install-ubuntu.sh remove   unload and remove
set -eu
here=$(cd "$(dirname "$0")" && pwd)
profile=/etc/apparmor.d/escrowd-bwrap

if [ "${1:-}" = remove ]; then
  [ -f "$profile" ] && sudo apparmor_parser -R "$profile" || true
  sudo rm -f "$profile" /usr/lib/escrowd/bwrap
  echo "removed"
  exit 0
fi

sudo install -D -m 0755 "$(command -v bwrap)" /usr/lib/escrowd/bwrap
sudo install -m 0644 "$here/apparmor/escrowd-bwrap" "$profile"
sudo apparmor_parser -r "$profile"
echo "installed /usr/lib/escrowd/bwrap with profile escrowd_bwrap"
