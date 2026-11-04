#!/bin/sh
# Creates and seeds the small FAT32 and exFAT test volumes that the boards embed
# as /fat32 and /exfat. Does nothing when they already exist.
set -eu

cd "$(dirname "$0")/.."

FAT32=boards/qemu-virt/disk-fat32.img
EXFAT=boards/qemu-virt/disk-exfat.img
if [ -f "$FAT32" ] && [ -f "$EXFAT" ]; then
    exit 0
fi

HOST="$(rustc -vV | sed -n 's/^host: //p')"

echo "[test] creating FAT32 and exFAT images"
rm -f "$FAT32" "$EXFAT"
truncate -s 36M "$FAT32"
mkfs.fat -F 32 -n FEOSFAT "$FAT32" >/dev/null
truncate -s 16M "$EXFAT"
mkfs.exfat -n FEOSEXFAT "$EXFAT" >/dev/null
cargo run -q -p feos-fs --example seed --target "$HOST" -- fat32 "$FAT32"
cargo run -q -p feos-fs --example seed --target "$HOST" -- exfat "$EXFAT"
