#!/bin/sh
# Stage T5's tests: clang with the feos target, LLVM's runtimes for feos, C and C++
# (and Rust linked with them) on both ports. In the localhost/feos-toolchain container
# with the tree at /src, the feos-ports-target volume at /build, --network none and
# /dev/kvm:
#
#   ports/test/container.sh t5 ports/test/stage-t5.sh [REF]
#
# REF defaults to refs/feos-checkpoints/1c.12. Builds (each a no-op when built and
# unchanged; PORTS_T5_NO_BUILD=1 skips them): clang (ports/build-clang.sh), relibc and
# the runtimes for both architectures (ports/build-relibc-feos.sh,
# ports/build-runtimes.sh). Then the host side: clang's lit test for the driver
# (clang/test/Driver/feos.c, through ports/test/clang-lit/run.py) and the programs
# (ports/test/t5/build.sh). Then a base of REF with the programs injected as /bin/t5-*
# and the sysroot, runtimes included, at /usr (checked there by t5-sysroot-check),
# both kernels, and specs/t5-toolchain.toml on each port (PORTS_T5_NO_BOOT=1 stops
# before the boots).
set -eu
cd "$(dirname "$0")/../.."
t=ports/test
ref=${1:-refs/feos-checkpoints/1c.12}
if [ "${PORTS_T5_NO_BUILD:-0}" != 1 ]; then
    ports/build-clang.sh
    ports/build-relibc-feos.sh all
    ports/build-runtimes.sh all
fi
python3 $t/clang-lit/run.py -v ports/src/rust/src/llvm-project/clang/test/Driver/feos.c
$t/t5/build.sh x86_64 aarch64
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
    $t/inject.sh $arch "/build/sysroot/$arch-unknown-feos/usr" /usr
    for program in /build/t5/$arch/t5-*; do
        $t/inject.sh $arch "$program" "/bin/$(basename "$program")"
    done
done
$t/build-kernels.sh
# PORTS_T5_NO_BOOT=1: build everything and stop (a build may overlap the kernel
# workflow's boot gates; a boot may not).
[ "${PORTS_T5_NO_BOOT:-0}" != 1 ] || exit 0
export PORTS_SHOW_OUTPUT=${PORTS_SHOW_OUTPUT:-1}
status=0
for port in arm x64; do
    python3 $t/boot.py $port $base/kernels/$port $t/specs/t5-toolchain.toml || status=1
done
exit $status
