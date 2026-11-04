#!/bin/sh
# Container entry point: build the selected kernel and boot it in QEMU.
set -eu

cd "$(dirname "$0")/.."

# `make docker-run` sets FEOS_ARCH=arm or FEOS_ARCH=x64. Default to AArch64 so
# a plain `docker run feos` keeps the original behaviour.
FEOS_ARCH="${FEOS_ARCH:-arm}"

case "$FEOS_ARCH" in
    arm|aarch64)
        TARGET="aarch64-unknown-none"
        KERNEL="target/$TARGET/release/feos"
        RUNNER="./scripts/qemu.sh"
        ;;
    x64|x86_64)
        TARGET="x86_64-unknown-none"
        KERNEL="target/$TARGET/release/feos"
        RUNNER="./scripts/qemu-x64.sh"
        ;;
    *)
        echo "unknown FEOS_ARCH=$FEOS_ARCH (use arm or x64)" >&2
        exit 1
        ;;
esac

# Default to a VNC backend when run without `make` (e.g. plain `docker run`),
# bound to all interfaces so `-p 5900:5900` works.
export FEOS_DISPLAY="${FEOS_DISPLAY:-vnc=0.0.0.0:0}"

# Audio backend. `make docker-run` passes a path under /out (bind-mounted to
# target/audio); a bare `docker run` has nowhere to copy the WAV, so keep it
# inside the container unless the caller provides FEOS_AUDIODEV.
export FEOS_AUDIODEV="${FEOS_AUDIODEV:-wav,id=snd0,path=/tmp/feos-qemu.wav,out.frequency=48000,out.channels=2,out.format=s16}"

# `make docker-run` asks for two small test volumes: one FAT32 and one exFAT.
# They are embedded in the kernel and exposed as /fat32 and /exfat. The x86_64
# board shares the AArch64 board's embedded tree and disk images.
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

cargo build --release --target "$TARGET"
exec "$RUNNER" "$KERNEL"
