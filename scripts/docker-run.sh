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

cargo build --release
exec ./scripts/qemu.sh target/aarch64-unknown-none/release/feos
