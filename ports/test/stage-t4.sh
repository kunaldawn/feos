#!/bin/sh
# Stage T4's tests: rustc with the feos targets built in (ports/build-rust.sh's
# /build/rust-feos), its programs on both ports. In the localhost/feos-toolchain
# container with the tree at /src, the feos-ports-target volume at /build,
# --network none and /dev/kvm:
#
#   ports/test/container.sh t4 ports/test/stage-t4.sh [REF]
#
# REF defaults to refs/feos-checkpoints/1c.11, the base T3's programs need (EXEC,
# WAIT4, KILL, signals, SIGALTSTACK, POLL). Builds the toolchain (ports/build-rust.sh:
# hours the first time, then only what changed; PORTS_T4_NO_BUILD=1 skips it and takes
# /build/rust-feos as it is), runs the host side (ports/test/rust/t4-host.sh: the
# built-in targets against T1's JSON specs, T3's programs and the cargo project built
# with them), makes a base of REF, injects T3's programs rebuilt by this toolchain as
# /bin/t3-* and the cargo project as /bin/t4-cargo, builds both kernels and runs
# specs/t3-rust.toml and specs/t4-rust.toml on each port.
set -eu
cd "$(dirname "$0")/../.."
t=ports/test
ref=${1:-refs/feos-checkpoints/1c.11}
[ "${PORTS_T4_NO_BUILD:-0}" = 1 ] || ports/build-rust.sh
$t/rust/t4-host.sh
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
    for program in /build/t4/t3/$arch/t3-* /build/t4/$arch/t4-cargo; do
        $t/inject.sh $arch "$program" "/bin/$(basename "$program")"
    done
done
$t/build-kernels.sh
export PORTS_SHOW_OUTPUT=${PORTS_SHOW_OUTPUT:-1}
status=0
for port in arm x64; do
    python3 $t/boot.py $port $base/kernels/$port $t/specs/t3-rust.toml $t/specs/t4-rust.toml || status=1
done
exit $status
