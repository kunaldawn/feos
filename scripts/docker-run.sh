#!/bin/sh
# Container entry point: build the kernel and boot it in QEMU.
set -eu

cd "$(dirname "$0")/.."

# Default to a VNC backend when run without `make` (e.g. plain `docker run`),
# bound to all interfaces so `-p 5900:5900` works.
export FEOS_DISPLAY="${FEOS_DISPLAY:-vnc=0.0.0.0:0}"

# Audio backend. `make docker-run` passes a path under /out (bind-mounted to
# target/audio); a bare `docker run` has nowhere to copy the WAV, so keep it
# inside the container unless the caller provides FEOS_AUDIODEV.
export FEOS_AUDIODEV="${FEOS_AUDIODEV:-wav,id=snd0,path=/tmp/feos-qemu.wav,out.frequency=48000,out.channels=2,out.format=s16}"

# `make docker-run` asks for two small test volumes: one FAT32 and one exFAT.
# They are embedded in the kernel and exposed as /fat32 and /exfat.
if [ "${FEOS_TEST_DISKS:-0}" = "1" ]; then
    echo "[test] creating FAT32 and exFAT images"
    rm -f boards/qemu-virt/disk-fat32.img boards/qemu-virt/disk-exfat.img
    truncate -s 36M boards/qemu-virt/disk-fat32.img
    mkfs.fat -F 32 -n FEOSFAT boards/qemu-virt/disk-fat32.img >/dev/null
    truncate -s 16M boards/qemu-virt/disk-exfat.img
    mkfs.exfat -n FEOSEXFAT boards/qemu-virt/disk-exfat.img >/dev/null
    cargo run -q -p feos-fs --example seed --target x86_64-unknown-linux-gnu -- \
        fat32 boards/qemu-virt/disk-fat32.img
    cargo run -q -p feos-fs --example seed --target x86_64-unknown-linux-gnu -- \
        exfat boards/qemu-virt/disk-exfat.img
fi

cargo build --release
exec ./scripts/qemu.sh target/aarch64-unknown-none/release/feos
