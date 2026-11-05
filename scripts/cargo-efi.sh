#!/bin/sh
# Builds the UEFI loader: scripts/cargo-efi.sh <x86_64|aarch64> [cargo build flags]
#
# The loader is its own target, a PE file firmware can start. The toolchain has no
# precompiled `core` for it, so as with the x86_64 kernel, it's built from rust-src.
# scripts/cargo-x64.sh has more on why this takes RUSTC_BOOTSTRAP=1.
set -eu

arch="$1"
shift
cd "$(dirname "$0")/.."
RUSTC_BOOTSTRAP=1 exec cargo build \
    -Zbuild-std=core,compiler_builtins \
    -Zbuild-std-features=compiler-builtins-mem \
    --target "$arch-unknown-uefi" \
    -p feos-efi \
    "$@"
