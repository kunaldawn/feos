#!/bin/sh
# The libc suite's Linux reference: relibc's tests and the same libc-test subset, built
# against relibc's own Linux platform and run on the host with the same runner (testrun,
# built with the host's gcc) and lists as on feos, x86_64 natively and AArch64 under
# qemu-aarch64. A test that fails here too is relibc's (an upstream limitation), not
# feos's; docs/toolchain.md's tables have both (ports/test/suite-report.py).
#
#   ports/test/container.sh linux ports/test/libc-suite-linux.sh [x86_64|aarch64 ...]
#
# In localhost/feos-toolchain with the feos-ports-target volume at /build, offline.
# Rebuilds relibc's Linux platform (incremental), makes the payloads in
# /build/libc-suite/linux-<arch>/ with every test that builds (the feos lists' pending
# and skipped ones too), and writes each list's testrun output to
# /build/libc-suite/linux-<arch>/{relibc-tests,libc-test}.result. Under qemu-user,
# relibc's threads don't start (its clone lacks CLONE_SYSVSEM, which qemu wants for a
# thread), so the AArch64 reference says nothing about thread tests.
set -eu
P=$(cd "$(dirname "$0")/.." && pwd)
out=${PORTS_BUILD:-/build}
"$P/build-cbindgen.sh"
mkdir -p "$out/libc-suite"
gcc -O2 -Wall -o "$out/libc-suite/testrun-host" "$P/test/c/suite/testrun.c" -lpthread
for arch in ${*:-x86_64 aarch64}; do
    dest=$out/libc-suite/linux-$arch
    "$P/build-relibc.sh" "$arch-unknown-linux-gnu"
    export LINUX_ARCH=$arch
    cc=$P/test/relibc-linux-cc
    python3 -I "$P/test/relibc_tests.py" "$P/src/relibc/tests" /dev/null "$cc" "$dest/relibc-tests" \
        "$dest/relibc-build" --all
    python3 -I "$P/test/libc_test.py" "$P/src/libc-test" "$P/test/libc-test-feos.txt" "$cc" \
        "$dest/libc-test" "$dest/libc-test-build" --all
    wrap=""
    [ "$arch" = x86_64 ] || wrap="-w /usr/bin/qemu-$arch"
    for list in relibc-tests/relibc.list libc-test/libc-test.list; do
        # shellcheck disable=SC2086
        "$out/libc-suite/testrun-host" -t 30 $wrap "$dest/$list" > "$dest/$(dirname $list).result" 2>&1 || true
        echo "linux-$arch: $(tail -n 2 "$dest/$(dirname $list).result" | tr '\n' ' ')"
    done
done
