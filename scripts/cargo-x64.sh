#!/bin/sh
# Runs cargo for the x86_64 kernel target: scripts/cargo-x64.sh build --release
#
# The kernel target is arch/x86_64/x86_64-feos.json. The stock
# `x86_64-unknown-none` is soft-float: rustc refuses to turn that off, and its
# precompiled `core` and `compiler_builtins` pass floats in integer registers, so
# hard-float code linked against them would be silently wrong. A custom target
# with hard floats needs `core` built from rust-src, and building it from source
# needs these unstable flags, which the pinned stable toolchain only accepts with
# RUSTC_BOOTSTRAP=1. Nothing else in the tree uses unstable features.
set -eu

command="$1"
shift
cd "$(dirname "$0")/.."
RUSTC_BOOTSTRAP=1 exec cargo "$command" \
    -Zbuild-std=core,alloc,compiler_builtins \
    -Zbuild-std-features=compiler-builtins-mem \
    -Zjson-target-spec \
    --target arch/x86_64/x86_64-feos.json \
    "$@"
