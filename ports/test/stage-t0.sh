#!/bin/sh
# Stage T0's feos-side test: the harness end to end, on both ports. Runs in the
# localhost/feos-toolchain container with the tree at /src, the
# feos-ports-target volume at /build, --network none and /dev/kvm:
#
#   ports/test/container.sh t0 ports/test/stage-t0.sh [REF]
#
# Makes a base from REF (default HEAD), builds its user programs, injects a copy
# of its hello as /bin/ports-hello and a marker file per port, builds both
# kernels and runs specs/t0-harness.toml on each.
set -eu
cd "$(dirname "$0")/../.."
t=ports/test
$t/base.sh "${1:-HEAD}"
base=${PORTS_BASE:?run in ports/test/container.sh}
export CARGO_TARGET_DIR=/build/ports-base-target RUSTUP_TOOLCHAIN=1.99.1
chmod +x $base/scripts/*.sh $base/scripts/*.py
. $t/fresh.sh
ports_base_sync user-arm user-x64 arm x64
make -C $base user-arm user-x64 CARGO_TARGET_DIR="$CARGO_TARGET_DIR"
ports_base_built user-arm
ports_base_built user-x64
scratch=$(mktemp -d)
for arch in aarch64 x86_64; do
    $t/inject.sh $arch $base/user/out/$arch/hello /bin/ports-hello
    echo "ports-overlay-$arch" > "$scratch/marker-$arch"
    $t/inject.sh $arch "$scratch/marker-$arch" /usr/share/ports-test/marker.txt
done
rm -rf "$scratch"
$t/build-kernels.sh
status=0
for port in arm x64; do
    python3 $t/boot.py $port $base/kernels/$port $t/specs/t0-harness.toml || status=1
done
# The harness must also report a failure as one: harness-selfcheck.toml has a
# pass, a failure, a case skipped after it and a pending one.
out=$(python3 $t/boot.py x64 $base/kernels/x64 $t/specs/harness-selfcheck.toml) && rc=0 || rc=$?
echo "$out" | grep -v '^---'
if [ $rc -eq 1 ] && echo "$out" | grep -q '^x64: 1 passed, 1 failed, 1 skipped, 1 pending$'; then
    echo "harness-selfcheck: PASS (failure, skip and pending reported as such)"
else
    echo "harness-selfcheck: FAIL (exit $rc)"; status=1
fi
exit $status
