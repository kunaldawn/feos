#!/bin/sh
# Stage T3's tests: the libc crate's feos module against relibc's headers
# (libc-check, host side), std for both feos targets, and Rust programs on both ports.
# In the localhost/feos-toolchain container with the tree at /src, the
# feos-ports-target volume at /build, --network none and /dev/kvm:
#
#   ports/test/container.sh t3 ports/test/stage-t3.sh [REF]
#
# REF defaults to refs/feos-checkpoints/1c.11, the newest verified kernel stage: EXEC,
# WAIT4, KILL, signal handlers and SIGALTSTACK for the process and stack overflow cases,
# POLL for Command::output (on 1c.10 everything else passes). Builds what it needs (each
# step a no-op when built and unchanged): relibc for feos and the sysroot, libunwind,
# std (ports/build-std.sh), the programs (ports/test/rust/build.sh). Then makes a base
# of REF, injects the programs as /bin/t3-*, builds both kernels and runs
# specs/t3-rust.toml on each port.
set -eu
cd "$(dirname "$0")/../.."
t=ports/test
ref=${1:-refs/feos-checkpoints/1c.11}
ports/build-std.sh all
$t/libc-check/libc-check.sh
$t/rust/build.sh
$t/base.sh "$ref"
base=${PORTS_BASE:?run in ports/test/container.sh}
export CARGO_TARGET_DIR=/build/ports-base-target RUSTUP_TOOLCHAIN=1.99.1
chmod +x $base/scripts/*.sh $base/scripts/*.py
. $t/fresh.sh
ports_base_sync user-arm user-x64 arm x64
make -C $base user-arm user-x64 CARGO_TARGET_DIR="$CARGO_TARGET_DIR"
ports_base_built user-arm
ports_base_built user-x64
for arch in aarch64 x86_64; do
    for program in /build/t3/$arch/t3-*; do
        $t/inject.sh $arch "$program" "/bin/$(basename "$program")"
    done
done
$t/build-kernels.sh
export PORTS_SHOW_OUTPUT=${PORTS_SHOW_OUTPUT:-1}
status=0
for port in arm x64; do
    python3 $t/boot.py $port $base/kernels/$port $t/specs/t3-rust.toml || status=1
done
exit $status
