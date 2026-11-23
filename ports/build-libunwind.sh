#!/bin/sh
# Build LLVM's libunwind for feos, static, from rust's own src/llvm-project (the copy
# rust 1.99.1 pins), for std's panic_unwind (track T3).
#
#   ports/build-libunwind.sh x86_64|aarch64|all
#
# In localhost/feos-toolchain with the feos-ports-target volume at /build, after
# ports/sysroot.sh (the headers it compiles against). The flags are rust bootstrap's
# own for an in-tree libunwind (src/bootstrap/src/core/build_steps/llvm.rs, Libunwind):
# -funwind-tables -fvisibility=hidden -fstrict-aliasing, _LIBUNWIND_IS_NATIVE_ONLY,
# C++ without exceptions, RTTI or a C++ library. It finds a program's frames through
# PT_GNU_EH_FRAME from dl_iterate_phdr, which relibc answers for a static program from
# the aux vector (ports/PATCHES.md).
#
# Output: /build/libunwind/<arch>-unknown-feos/libunwind.a (and the objects beside it).
# Rebuilt only when a source, this script or the C driver changed.
set -eu
P=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 1 ] || { echo "usage: $0 x86_64|aarch64|all" >&2; exit 2; }
if [ "$1" = all ]; then
    "$0" x86_64 && exec "$0" aarch64
fi
arch=$1
case "$arch" in x86_64|aarch64) ;; *) echo "build-libunwind: unknown arch $arch" >&2; exit 2 ;; esac
target=$arch-unknown-feos
out=${PORTS_BUILD:-/build}
export FEOS_SYSROOT=$out/sysroot/$target
[ -f "$FEOS_SYSROOT/usr/lib/libc.a" ] || { echo "build-libunwind: no sysroot; run ports/sysroot.sh $arch" >&2; exit 1; }
src=$P/src/rust/src/llvm-project/libunwind
dest=$out/libunwind/$target
cc=$P/toolchain/bin/$target-cc

stamp=$( (cat "$0" "$P/toolchain/bin/feos-cc"; find "$src/src" "$src/include" -type f | sort | xargs cat;
    cat "$FEOS_SYSROOT/usr/share/feos-sysroot/MANIFEST") | sha256sum | cut -d' ' -f1)
if [ -f "$dest/libunwind.a" ] && [ "$(cat "$dest/.stamp" 2>/dev/null)" = "$stamp" ]; then
    echo "build-libunwind: $target is up to date"
    exit 0
fi
rm -rf "$dest"
mkdir -p "$dest"
common="-O3 -fstrict-aliasing -funwind-tables -fvisibility=hidden \
    -D_LIBUNWIND_DISABLE_VISIBILITY_ANNOTATIONS -D_LIBUNWIND_IS_NATIVE_ONLY=1 -DNDEBUG \
    -I$src/include -w"
for f in Unwind-sjlj.c UnwindLevel1-gcc-ext.c UnwindLevel1.c UnwindRegistersRestore.S UnwindRegistersSave.S; do
    # shellcheck disable=SC2086
    "$cc" $common -std=c99 -c "$src/src/$f" -o "$dest/${f%.*}.o"
done
for f in Unwind-EHABI.cpp Unwind-seh.cpp libunwind.cpp; do
    # shellcheck disable=SC2086
    "$cc" $common -std=c++17 -nostdinc++ -fno-exceptions -fno-rtti \
        -fvisibility-global-new-delete-hidden -c "$src/src/$f" -o "$dest/${f%.*}.o"
done
llvm-ar -rcs "$dest/libunwind.a" "$dest"/*.o
for s in _Unwind_RaiseException _Unwind_Resume _Unwind_Backtrace _Unwind_GetIP; do
    llvm-nm --defined-only "$dest/libunwind.a" | grep -q " T $s\$" ||
        { echo "build-libunwind: $s missing from libunwind.a" >&2; exit 1; }
done
echo "$stamp" > "$dest/.stamp"
echo "build-libunwind: $dest/libunwind.a"
