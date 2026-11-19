#!/bin/sh
# Stage T1's feos-side test: relibc's feos platform, static, running C programs on
# both ports. In the localhost/feos-toolchain container with the tree at /src, the
# feos-ports-target volume at /build, --network none and /dev/kvm:
#
#   ports/test/container.sh t1 ports/test/stage-t1.sh [REF]
#
# REF defaults to refs/feos-checkpoints/1c.6, the stage whose calls relibc needs (any
# later verified ref works too). Builds cbindgen (build-cbindgen.sh, which relibc's
# headers need), relibc for both feos targets (build-relibc-feos.sh) and the C
# programs (build-t1.sh), each a no-op when built and unchanged, so a fresh
# feos-ports-target volume works too. Then makes a base of REF, injects the programs
# as /bin/t1-*, builds both kernels and runs specs/t1-relibc.toml on each port.
set -eu
cd "$(dirname "$0")/../.."
t=ports/test
ref=${1:-refs/feos-checkpoints/1c.6}
ports/build-cbindgen.sh
ports/build-relibc-feos.sh all
$t/build-t1.sh
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
    for program in /build/t1/$arch/t1-*; do
        $t/inject.sh $arch "$program" "/bin/$(basename "$program")"
    done
done
$t/build-kernels.sh
# The passing cases' output goes into the log too, for the record.
export PORTS_SHOW_OUTPUT=${PORTS_SHOW_OUTPUT:-1}
status=0
for port in arm x64; do
    python3 $t/boot.py $port $base/kernels/$port $t/specs/t1-relibc.toml || status=1
done
exit $status
