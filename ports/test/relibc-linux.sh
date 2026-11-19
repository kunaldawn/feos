#!/bin/sh
# Stage T0's host-side test: relibc built for its Linux platform works. Links
# test/c/hello.c against the sysroot ports/build-relibc.sh installed, statically
# and dynamically (through relibc's own ld.so), and runs both; then, unless
# RELIBC_SUITE=0, relibc's own test suite (ports/src/relibc/tests), with its
# output under /build, nothing written into the source tree.
#
#   ports/build-relibc.sh x86_64-unknown-linux-gnu
#   ports/test/relibc-linux.sh x86_64-unknown-linux-gnu
#
# In localhost/feos-toolchain with the feos-ports-target volume at /build.
set -eu
P=$(cd "$(dirname "$0")/.." && pwd)
target=${1:-x86_64-unknown-linux-gnu}
out=${PORTS_BUILD:-/build}
sysroot=$out/relibc/$target
work=$out/relibc-test/$target
rm -rf "$work"
mkdir -p "$work"
case "$target" in
    x86_64-unknown-linux-gnu) cc=x86_64-linux-gnu-gcc ld_so=ld64.so.1 run="timeout 60" ;;
    # qemu-user refuses relibc's thread clone (no CLONE_SYSVSEM), so no threads there.
    aarch64-unknown-linux-gnu) cc=aarch64-linux-gnu-gcc ld_so=ld.so.1 run="env RELIBC_HELLO_NO_THREADS=1 timeout 60 qemu-aarch64" ;;
    *) echo "relibc-linux: no Linux target $target" >&2; exit 2 ;;
esac
common="-nostdinc -nostdlib -isystem $sysroot/include $sysroot/lib/crt0.o $sysroot/lib/crti.o"

# shellcheck disable=SC2086
$cc -O2 $P/test/c/hello.c -o "$work/hello-static" $common -static $sysroot/lib/libc.a $sysroot/lib/crtn.o -lgcc
# shellcheck disable=SC2086
$cc -O2 $P/test/c/hello.c -o "$work/hello-dynamic" $common \
    -Wl,-dynamic-linker=$sysroot/lib/$ld_so -Wl,-rpath=$sysroot/lib -L $sysroot/lib -lc $sysroot/lib/crtn.o -lgcc

status=0
for kind in static dynamic; do
    if (cd "$work" && $run ./hello-$kind "$work/$kind.tmp") > "$work/$kind.out" 2>&1 \
            && grep -q '^relibc-hello: ok$' "$work/$kind.out"; then
        echo "relibc-linux $target PASS hello-$kind"
    else
        echo "relibc-linux $target FAIL hello-$kind:"; cat "$work/$kind.out"; status=1
    fi
done
file "$work/hello-static" "$work/hello-dynamic"

if [ "${RELIBC_SUITE:-1}" != 0 ]; then
    export RUSTUP_TOOLCHAIN=1.99.1 RUSTC_BOOTSTRAP=1 CARGO_NET_OFFLINE=true
    suite=$out/relibc-suite/$target
    rm -rf "$suite"
    if make -C "$P/src/relibc/tests" TARGET="$target" SYSROOT="$sysroot" DEPS="$sysroot" BUILD="$suite" \
            TEST_RUNNER="$P/test/relibc-runner.sh" run > "$work/suite.log" 2>&1; then
        echo "relibc-linux $target PASS relibc test suite: $(grep -c '^# /.* #$' "$work/suite.log") programs run"
        grep '^relibc-runner:' "$work/suite.log" || true
    else
        echo "relibc-linux $target FAIL relibc test suite (log: $work/suite.log)"; tail -40 "$work/suite.log"; status=1
    fi
fi
exit $status
