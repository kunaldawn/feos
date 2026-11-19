#!/bin/sh
# Build the libc-test subset feos runs (ports/test/libc-test-feos.txt) from musl's
# libc-test (ports/src/libc-test), static, against the feos sysroot, as a payload for the
# feos image that /bin/testrun runs.
#
#   ports/test/build-libc-test.sh [x86_64|aarch64 ...]      # default both
#
# In localhost/feos-toolchain with the feos-ports-target volume at /build, after
# ports/sysroot.sh. libtest.a (src/common, without libc-test's own runner) and every
# selected test build with libc-test's config.mak.def flags through
# ports/toolchain/bin/<arch>-unknown-feos-cc; a selected test that doesn't build fails
# the script unless libc-test-feos.txt lists it. The payload is
# /build/libc-suite/<arch>/libc-test: DIR/NAME binaries and libc-test.list, testrun's
# list (`status`: libc-test's tests print what failed and exit non-zero).
set -eu
P=$(cd "$(dirname "$0")/.." && pwd)
out=${PORTS_BUILD:-/build}
export PATH="$P/toolchain/bin:$PATH"
for arch in ${*:-x86_64 aarch64}; do
    export FEOS_SYSROOT="$out/sysroot/$arch-unknown-feos"
    [ -f "$FEOS_SYSROOT/usr/lib/libc.a" ] || { echo "build-libc-test: no sysroot; run ports/sysroot.sh $arch" >&2; exit 1; }
    python3 -I "$P/test/libc_test.py" "$P/src/libc-test" "$P/test/libc-test-feos.txt" \
        "$arch-unknown-feos-cc" "$out/libc-suite/$arch/libc-test" "$out/libc-suite/$arch/libc-test-build"
done
