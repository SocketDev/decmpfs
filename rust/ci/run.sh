#!/bin/sh
# Runtime entry for the slim harness image: provision btrfs (shared with CI) and
# run the prebuilt decmpfs test binaries against it. No cargo in this stage.
# POSIX sh so the runtime needs only busybox ash, not bash.
set -eu

# shellcheck source=./btrfs-loopback.sh
. /ci/btrfs-loopback.sh

for bin in /bins/*; do
  echo "== $(basename "$bin") =="
  "$bin" --nocapture --test-threads=1
done
