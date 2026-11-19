#!/bin/sh
# Stage T0's host-side tests, from nothing: cbindgen from source, relibc for
# x86_64- and aarch64-unknown-linux-gnu, and relibc-linux.sh on each (the
# suite on x86_64; aarch64 runs under qemu-user, which can't run the suite's
# runner). In localhost/feos-toolchain, --network none:
#
#   PORTS_FORCE=1 ports/test/container.sh t0-host ports/test/stage-t0-host.sh
#
# PORTS_BUILD (default /build/t0-host) is emptied first, so nothing is reused.
set -eu
cd "$(dirname "$0")/../.."
export PORTS_BUILD=${PORTS_BUILD:-/build/t0-host}
rm -rf "$PORTS_BUILD"
mkdir -p "$PORTS_BUILD"
# No crate cache either: everything comes from ports/vendor and ports/vendor-git.
export CARGO_HOME="$PORTS_BUILD/cargo-home"
ports/build-cbindgen.sh
status=0
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    ports/build-relibc.sh $target
    suite=1
    [ $target = aarch64-unknown-linux-gnu ] && suite=0
    RELIBC_SUITE=$suite ports/test/relibc-linux.sh $target || status=1
done
exit $status
