#!/bin/sh
# Container entry point: build the kernel and boot it in QEMU.
set -eu

cd "$(dirname "$0")/.."

# Default to a VNC backend when run without `make` (e.g. plain `docker run`),
# bound to all interfaces so `-p 5900:5900` works.
export FEOS_DISPLAY="${FEOS_DISPLAY:-vnc=0.0.0.0:0}"

cargo build --release
exec ./scripts/qemu.sh target/aarch64-unknown-none/release/feos
