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

# FEOS_QEMU_ARGS is deliberately word-split: it holds extra QEMU arguments.
# shellcheck disable=SC2086
exec "$QEMU" \
    -machine virt \
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
    ${FEOS_QEMU_ARGS:---device virtio-tablet-device -device virtio-keyboard-device}
