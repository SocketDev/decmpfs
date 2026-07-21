#!/usr/bin/env bash
# Provision real btrfs loopback filesystems and export DECMPFS_BTRFS_DIR /
# DECMPFS_BTRFS_RO_DIR. SOURCE this (it exports), don't exec it.
#
# Single source of truth for the btrfs runtime setup, shared by the local
# ci/Dockerfile harness and (when wired) a CI btrfs job, so the two can't drift.
# Needs a btrfs-capable kernel + root (or sudo).
set -euo pipefail

# sudo when we aren't already root (CI runs as a user; the container runs as root).
sudo_() { if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@"; fi; }

grep -qw btrfs /proc/filesystems || sudo_ modprobe btrfs 2>/dev/null || true
grep -qw btrfs /proc/filesystems || {
  echo "FAIL: kernel has no btrfs support"
  return 99 2>/dev/null || exit 99
}

# Read-write btrfs. No compress= mount option, so the only compression comes from
# the per-file codec the backend requests via the btrfs.compression property.
truncate -s 400M /tmp/btrfs.img
mkfs.btrfs -q /tmp/btrfs.img
sudo_ mkdir -p /mnt/bt
sudo_ mount -o loop /tmp/btrfs.img /mnt/bt
sudo_ chmod 1777 /mnt/bt

# Read-only btrfs: seed a file, then remount read-only (the fail-soft path).
truncate -s 200M /tmp/btrfs-ro.img
mkfs.btrfs -q /tmp/btrfs-ro.img
sudo_ mkdir -p /mnt/bt-ro
sudo_ mount -o loop /tmp/btrfs-ro.img /mnt/bt-ro
head -c 1048576 /dev/zero | sudo_ tee /mnt/bt-ro/ro.node > /dev/null
sync
sudo_ mount -o remount,ro /mnt/bt-ro

export DECMPFS_BTRFS_DIR=/mnt/bt
export DECMPFS_BTRFS_RO_DIR=/mnt/bt-ro
echo "btrfs ready: $DECMPFS_BTRFS_DIR (rw) + $DECMPFS_BTRFS_RO_DIR (ro)"
