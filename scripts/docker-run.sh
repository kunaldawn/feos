#!/bin/sh
# Container entry point: build the selected kernel and boot it in QEMU.
set -eu

cd "$(dirname "$0")/.."

# `make docker-run` sets FEOS_ARCH=arm or FEOS_ARCH=x64. Default to AArch64 so
# a plain `docker run feos` keeps the original behaviour.
FEOS_ARCH="${FEOS_ARCH:-arm}"

case "$FEOS_ARCH" in
    arm|aarch64)
        KERNEL="target/aarch64-unknown-none/release/feos"
        USER_BUILD="make user-arm"
        BUILD="cargo build --release --target aarch64-unknown-none"
        RUNNER="./scripts/qemu.sh"
        ;;
    x64|x86_64)
        KERNEL="target/x86_64-feos/release/feos"
        USER_BUILD="make user-x64"
        BUILD="./scripts/cargo-x64.sh build --release"
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
# board shares the AArch64 board's embedded tree and disk images. The image
# build already made them, so this normally does nothing.
if [ "${FEOS_TEST_DISKS:-0}" = "1" ]; then
    ./scripts/make-test-disks.sh
else
    rm -f boards/qemu-virt/disk-fat32.img boards/qemu-virt/disk-exfat.img
fi

$USER_BUILD
$BUILD
exec "$RUNNER" "$KERNEL"
