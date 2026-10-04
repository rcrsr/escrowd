#!/bin/sh
# Create and mount 2 GB loopback XFS and btrfs volumes for spike 0.5 (needs sudo; run in a VM).
#   setup-volumes.sh            create (once) and mount, owned by the calling user
#   setup-volumes.sh umount     unmount (images stay in $DIR)
set -eu
DIR=${DIR:-/var/escrowd-volumes}
user=$(id -un)
if [ "${1:-}" = umount ]; then
  for fs in xfs btrfs; do sudo umount "/mnt/escrow-$fs" 2>/dev/null || true; done
  echo "unmounted"; exit 0
fi
command -v mkfs.xfs >/dev/null && command -v mkfs.btrfs >/dev/null || sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq xfsprogs btrfs-progs >/dev/null
sudo mkdir -p "$DIR"
for fs in xfs btrfs; do
  img=$DIR/$fs.img mnt=/mnt/escrow-$fs
  if [ ! -f "$img" ]; then
    sudo truncate -s 2G "$img"
    if [ $fs = xfs ]; then sudo mkfs.xfs -q -m reflink=1 "$img"; else sudo mkfs.btrfs -q "$img"; fi
  fi
  sudo mkdir -p "$mnt"
  if ! mountpoint -q "$mnt"; then
    opts=loop; [ $fs = btrfs ] && opts=loop,user_subvol_rm_allowed
    sudo mount -o "$opts" "$img" "$mnt"
  fi
  sudo chown "$user:" "$mnt"
done
for fs in xfs btrfs; do findmnt -n -o TARGET,FSTYPE,SIZE "/mnt/escrow-$fs"; done
