#!/bin/sh
# Boot the kernel in QEMU's aarch64 `virt` machine with ramfb display, a
# virtio tablet pointer, a virtio keyboard and an Intel HDA audio device. Set
# FEOS_QEMU_ARGS to replace the input devices and add QEMU arguments (`-accel kvm`).
#
#   ./scripts/qemu.sh [path/to/kernel.elf]
#
# The serial console is on stdio and the graphical window has its own virtio
# keyboard. The display backend comes from FEOS_DISPLAY:
# `vnc=127.0.0.1:0` (the default), `gtk` for a native window, or `none` for
# serial only. With VNC, connect a viewer to localhost:5900 to see the
# graphics. Use the `tone` shell command to play audio and `poweroff` (or
# Ctrl-A X) to leave QEMU.
#
# FEOS_AUDIODEV selects the QEMU audio backend. The default writes a WAV file
# under target/audio, which works on headless machines. Use `pa,id=snd0` for
# PulseAudio/PipeWire or `alsa,id=snd0` for ALSA (and a sink, see `aplay -l`).
#
# FEOS_DISK names the disk image, target/feos-disk-arm.img by default. It's
# created sparse at FEOS_DISK_SIZE (default 512M) if missing, and the kernel
# formats a blank disk on first boot, so files survive reboots. Delete it to start
# over, or set FEOS_DISK=none to boot with no disk and a root in RAM.
set -eu

KERNEL="${1:-target/aarch64-unknown-none/release/feos}"
DISPLAY_BACKEND="${FEOS_DISPLAY:-vnc=127.0.0.1:0}"
AUDIODEV="${FEOS_AUDIODEV:-wav,id=snd0,path=target/audio/qemu.wav,out.frequency=48000,out.channels=2,out.format=s16}"
SMP="${FEOS_SMP:-4}"
MEM="${FEOS_MEM:-512M}"
QEMU="${FEOS_QEMU:-qemu-system-aarch64}"

if [ ! -f "$KERNEL" ]; then
    echo "kernel not found: $KERNEL" >&2
    echo "build it first with: cargo build --release" >&2
    exit 1
fi

mkdir -p target/audio

DISK="${FEOS_DISK:-target/feos-disk-arm.img}"
DISK_ARGS=""
if [ "$DISK" != "none" ]; then
    if [ ! -e "$DISK" ]; then
        truncate -s "${FEOS_DISK_SIZE:-512M}" "$DISK"
    fi
    DISK_ARGS="-drive if=none,id=disk0,format=raw,file=$DISK -device virtio-blk-device,drive=disk0"
fi

# FEOS_QEMU_ARGS is deliberately word-split: it holds extra QEMU arguments.
# shellcheck disable=SC2086
exec "$QEMU" \
    -machine "${FEOS_MACHINE:-virt}" \
    -cpu cortex-a53 \
    -smp "$SMP" \
    -m "$MEM" \
    -net none \
    -device ramfb \
    -audiodev "$AUDIODEV" \
    -device intel-hda \
    -device hda-output,audiodev=snd0 \
    -display "$DISPLAY_BACKEND" \
    -serial mon:stdio \
    -kernel "$KERNEL" \
    $DISK_ARGS \
    ${FEOS_QEMU_ARGS:---device virtio-tablet-device -device virtio-keyboard-device}
