#!/bin/sh
# Build relibc for feos, static, offline, and install its sysroot.
#
#   ports/build-relibc-feos.sh x86_64|aarch64     # or both: ports/build-relibc-feos.sh all
#
# In localhost/feos-toolchain, the tree at /src and the feos-ports-target volume at
# /build; needs /build/bin/cbindgen (ports/build-cbindgen.sh). The target is
# ports/targets/<arch>-unknown-feos.json, built with the image's rust 1.99.1
# (RUSTC_BOOTSTRAP for -Zbuild-std=core,alloc,compiler_builtins and
# -Zjson-target-spec; std's crates from ports/vendor). C parts (relibc's src/c,
# openlibm) go through ports/toolchain/bin/<arch>-unknown-feos-cc, the host clang.
#
# Cargo's output goes to /build/relibc-target/<arch>-unknown-feos, the sysroot to
# /build/relibc/<arch>-unknown-feos: include/ and lib/ with crt0.o (also as crt1.o),
# crti.o, crtn.o, libc.a (relibc and openlibm), libm.a (openlibm alone),
# libcompiler_builtins.a, and empty libdl.a, libpthread.a, librt.a. No libc.so or
# ld.so: feos runs static programs until kernel stage 1f.1.
set -eu
P=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 1 ] || { echo "usage: $0 x86_64|aarch64|all" >&2; exit 2; }
if [ "$1" = all ]; then
    "$0" x86_64 && exec "$0" aarch64
fi
arch=$1
case "$arch" in x86_64|aarch64) ;; *) echo "build-relibc-feos: unknown arch $arch" >&2; exit 2 ;; esac
target=$arch-unknown-feos
out=${PORTS_BUILD:-/build}
[ -x "$out/bin/cbindgen" ] || { echo "build-relibc-feos: build cbindgen first (ports/build-cbindgen.sh)" >&2; exit 1; }

export RUSTUP_TOOLCHAIN=1.99.1 RUSTC_BOOTSTRAP=1 CARGO_NET_OFFLINE=true
export PATH="$out/bin:$P/toolchain/bin:$PATH"
export CARGO_TARGET_DIR="$out/relibc-target"
build=$CARGO_TARGET_DIR/$target
sysroot=$out/relibc/$target
# relibc's build: its build script's cc crate and openlibm's make use these.
export CC=$target-cc AR=llvm-ar NM=llvm-nm OBJCOPY=llvm-objcopy LD=ld.lld RANLIB=llvm-ranlib
export CRATE_CC_NO_DEFAULTS=1 USECLANG=1 USEGCC=0
# relibc's own C sources build before its headers exist.
export FEOS_SYSROOT="$build"
# openlibm keeps its floating-point exceptions (log(0) raising FE_DIVBYZERO, and so on) only
# if the compiler doesn't fold them away, which gcc (relibc's Linux build) doesn't by
# default and clang does unless told: strict on x86_64, through CPPFLAGS (relibc's
# Makefile sets CFLAGS itself, and hands CPPFLAGS to openlibm's make). Debian's clang 14
# can't do strict floating point on AArch64 (it warns and ignores it), so there the flags
# stay clang's; docs/toolchain.md has what that costs in libc-test.
case "$arch" in
    x86_64) export CPPFLAGS="-ffp-exception-behavior=strict" ;;
    *) export CPPFLAGS="" ;;
esac
spec=$P/targets/$target.json
flags="-Zbuild-std=core,alloc,compiler_builtins -Zjson-target-spec --target=$spec"

# relibc's Makefile knows its sources but not the target spec, the C driver or CPPFLAGS:
# when one changed since the last build, everything built for this target is stale.
mkdir -p "$build"
stamp=$( (cat "$spec" "$P/toolchain/bin/feos-cc"; echo "CPPFLAGS=$CPPFLAGS") | sha256sum | cut -d' ' -f1)
if [ "$(cat "$build/.feos-spec" 2>/dev/null)" != "$stamp" ]; then
    # The Makefile's rules emit with `cargo rustc --emit`, which emits nothing when cargo
    # finds the crate fresh, so cargo's own output goes too.
    rm -rf "$build/release" "$build/openlibm" "$build/include"
fi

cd "$P/src/relibc"
make BUILD="$build" TARGET="$target" CARGOFLAGS="$flags" \
    headers "$build/release/libc.a" "$build/release/crt0.o" "$build/release/crti.o" \
    "$build/release/crtn.o"

for object in crt0.o crti.o crtn.o; do
    if ! llvm-readelf -h "$build/release/$object" >/dev/null 2>&1; then
        rm -f "$build/.feos-spec"
        echo "build-relibc-feos: $object is empty (cargo found crt0 fresh); run again" >&2
        exit 1
    fi
done
echo "$stamp" > "$build/.feos-spec"

rm -rf "$sysroot.partial"
mkdir -p "$sysroot.partial/include" "$sysroot.partial/lib"
cp -r "$build/include/." "$sysroot.partial/include/"
cp "$build/release/libc.a" "$build/release/crt0.o" "$build/release/crti.o" \
    "$build/release/crtn.o" "$sysroot.partial/lib/"
ln -s crt0.o "$sysroot.partial/lib/crt1.o"
cp "$build/openlibm/libopenlibm.a" "$sysroot.partial/lib/libm.a"
# compiler-builtins for C code (relibc renames its own copy's symbols inside libc.a):
# the rlib -Zbuild-std made is an archive with a symbol table, which lld links as is.
builtins=$(ls -t "$build"/release/deps/libcompiler_builtins-*.rlib | head -n 1)
cp "$builtins" "$sysroot.partial/lib/libcompiler_builtins.a"
for lib in dl pthread rt; do
    llvm-ar -rcs "$sysroot.partial/lib/lib$lib.a"
done
rm -rf "$sysroot"
mv "$sysroot.partial" "$sysroot"
echo "build-relibc-feos: sysroot at $sysroot"
ls "$sysroot/lib"
