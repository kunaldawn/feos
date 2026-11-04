#!/bin/sh
# Boot the x86_64 kernel in QEMU's q35 machine with ramfb display, PS/2 input,
# Intel HDA audio and the serial console on stdio.
#
#   ./scripts/qemu-x64.sh [path/to/kernel.elf]
#
# QEMU's multiboot loader expects a flat image, not an ELF with 64-bit program
# headers, so this script converts the linked ELF with objcopy before booting.
#
# FEOS_DISPLAY selects the display backend (`vnc=127.0.0.1:0` by default,
# `gtk` for a native window, `none` for serial only). FEOS_AUDIODEV selects the
# QEMU audio backend; the default writes a WAV under target/audio. Use the
# `tone` shell command to play audio and `poweroff` (or Ctrl-A X) to leave QEMU.
set -eu

KERNEL_ELF="${1:-target/x86_64-unknown-none/release/feos}"
DISPLAY_BACKEND="${FEOS_DISPLAY:-vnc=127.0.0.1:0}"
AUDIODEV="${FEOS_AUDIODEV:-wav,id=snd0,path=target/audio/qemu.wav,out.frequency=48000,out.channels=2,out.format=s16}"
SMP="${FEOS_SMP:-1}"
MEM="${FEOS_MEM:-512M}"

if [ ! -f "$KERNEL_ELF" ]; then
    echo "kernel not found: $KERNEL_ELF" >&2
    echo "build it first with: cargo build --release --target x86_64-unknown-none" >&2
    exit 1
fi

# Find an objcopy. The Rust toolchain bundles `rust-objcopy` outside PATH on
# some installations; GNU binutils is the usual host fallback.
if [ -z "${OBJCOPY:-}" ]; then
    if command -v objcopy >/dev/null 2>&1; then
        OBJCOPY=objcopy
    elif command -v llvm-objcopy >/dev/null 2>&1; then
        OBJCOPY=llvm-objcopy
    else
        SYSROOT="$(rustc --print sysroot 2>/dev/null || true)"
        HOST="$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')"
        OBJCOPY="$SYSROOT/lib/rustlib/$HOST/bin/rust-objcopy"
    fi
fi

if [ ! -x "$OBJCOPY" ] && ! command -v "$OBJCOPY" >/dev/null 2>&1; then
    echo "objcopy not found (install binutils or set OBJCOPY=...)" >&2
    exit 1
fi

KERNEL_BIN="${KERNEL_ELF%.elf}.bin"
if [ "$KERNEL_BIN" = "$KERNEL_ELF" ]; then
    KERNEL_BIN="$KERNEL_ELF.bin"
fi

"$OBJCOPY" -O binary "$KERNEL_ELF" "$KERNEL_BIN"

mkdir -p target/audio

# `-vga none` makes ramfb the only display. q35 supplies the PS/2 controller
# and the fw_cfg I/O ports the display and audio drivers use.
exec qemu-system-x86_64 \
    -machine q35 \
    -cpu max \
    -smp "$SMP" \
    -m "$MEM" \
    -net none \
    -vga none \
    -device ramfb \
    -audiodev "$AUDIODEV" \
    -device intel-hda \
    -device hda-output,audiodev=snd0 \
    -display "$DISPLAY_BACKEND" \
    -serial mon:stdio \
    -kernel "$KERNEL_BIN"
