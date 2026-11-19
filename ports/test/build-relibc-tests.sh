#!/bin/sh
# Build relibc's own test suite (ports/src/relibc/tests) for feos, static, against the
# feos sysroot, as a payload for the feos image that /bin/testrun runs.
#
#   ports/test/build-relibc-tests.sh [x86_64|aarch64 ...]      # default both
#
# In localhost/feos-toolchain with the feos-ports-target volume at /build, after
# ports/sysroot.sh. Every test relibc's tests/Makefile.tests.mk names is compiled with
# relibc's own flags through ports/toolchain/bin/<arch>-unknown-feos-cc (so a test that
# doesn't build fails this script), and ports/test/relibc-feos.txt says which of them
# feos runs: a test listed there is left out of the payload, as pending on the kernel
# stage named or skipped for the reason given. The payload, in
# /build/libc-suite/<arch>/relibc-tests, is laid out as relibc's own runner expects
# its build directory: bins_static/, expected/, example_dir/ and the input files, with
# relibc.list, testrun's list (ports/test/c/suite/testrun.c): `expect` for the tests
# whose stdout and stderr relibc checks against expected/, `status` for the rest.
set -eu
P=$(cd "$(dirname "$0")/.." && pwd)
out=${PORTS_BUILD:-/build}
export PATH="$P/toolchain/bin:$PATH"
for arch in ${*:-x86_64 aarch64}; do
    export FEOS_SYSROOT="$out/sysroot/$arch-unknown-feos"
    [ -f "$FEOS_SYSROOT/usr/lib/libc.a" ] || { echo "build-relibc-tests: no sysroot; run ports/sysroot.sh $arch" >&2; exit 1; }
    python3 -I "$P/test/relibc_tests.py" "$P/src/relibc/tests" "$P/test/relibc-feos.txt" \
        "$arch-unknown-feos-cc" "$out/libc-suite/$arch/relibc-tests" "$out/libc-suite/$arch/relibc-build"
done
