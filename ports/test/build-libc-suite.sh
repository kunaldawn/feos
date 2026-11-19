#!/bin/sh
# Build everything the libc suite puts into the feos image, for both architectures:
# cbindgen and relibc for feos (each a no-op when built and unchanged), the sysroot
# (ports/sysroot.sh), the suite's own programs (ports/test/c/suite: testrun, the runner,
# and sysroot-check), relibc's tests and the libc-test subset.
#
#   ports/test/build-libc-suite.sh [x86_64|aarch64 ...]      # default both
#
# In localhost/feos-toolchain with the tree at /src and the feos-ports-target volume at
# /build, offline. Output: /build/sysroot/<arch>-unknown-feos and
# /build/libc-suite/<arch>/{bin,relibc-tests,libc-test}.
set -eu
P=$(cd "$(dirname "$0")/.." && pwd)
out=${PORTS_BUILD:-/build}
arches=${*:-x86_64 aarch64}
"$P/build-cbindgen.sh"
for arch in $arches; do
    "$P/build-relibc-feos.sh" "$arch"
    "$P/sysroot.sh" "$arch"
done
export PATH="$P/toolchain/bin:$PATH"
for arch in $arches; do
    bin=$out/libc-suite/$arch/bin
    rm -rf "$bin"
    mkdir -p "$bin"
    # PORTS_SUITE_EXTRA_C: more C programs for /bin while debugging (libc-suite.sh).
    for src in "$P/test/c/suite/testrun.c" "$P/test/c/suite/sysroot-check.c" ${PORTS_SUITE_EXTRA_C:-}; do
        FEOS_SYSROOT="$out/sysroot/$arch-unknown-feos" "$arch-unknown-feos-cc" -O2 -Wall -Wextra \
            -Wl,--gc-sections,-s -o "$bin/$(basename "$src" .c)" "$src"
    done
done
"$P/test/build-relibc-tests.sh" $arches
"$P/test/build-libc-test.sh" $arches
for arch in $arches; do
    echo "build-libc-suite: $arch: $(du -sh "$out/libc-suite/$arch/relibc-tests" | cut -f1) relibc tests," \
        "$(du -sh "$out/libc-suite/$arch/libc-test" | cut -f1) libc-test," \
        "$(du -sh "$out/sysroot/$arch-unknown-feos" | cut -f1) sysroot"
done
