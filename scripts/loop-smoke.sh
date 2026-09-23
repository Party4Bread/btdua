#!/usr/bin/env bash
# Builds a small btrfs image with known contents for manual testing.
# Usage: sudo scripts/loop-smoke.sh [image-path]   (prints the mount point)
set -euo pipefail
img=${1:-/tmp/btdua-smoke.img}
mnt=$(mktemp -d /tmp/btdua-smoke-mnt.XXXX)
truncate -s 1G "$img"
mkfs.btrfs -q -f "$img"
mount -o loop,compress=zstd "$img" "$mnt"
btrfs subvolume create -p "$mnt/vol" >/dev/null
head -c 300M /dev/urandom > "$mnt/vol/random.bin"
head -c 200M < <(yes "compressible line of text") > "$mnt/vol/text.txt"
cp --reflink=always "$mnt/vol/random.bin" "$mnt/vol/random-reflink.bin"
btrfs subvolume snapshot "$mnt/vol" "$mnt/snap" >/dev/null
btrfs subvolume create "$mnt/vol/nested" >/dev/null
head -c 20M /dev/urandom > "$mnt/vol/nested/n.bin"
mkdir -p "$mnt/dir/sub" && head -c 50M /dev/urandom > "$mnt/dir/sub/f.bin"
sync
echo "$mnt"
