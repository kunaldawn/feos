#!/bin/sh
# Build Rust's std for feos (track T3) and lay it out as a sysroot rustc can use
# directly.
#
#   ports/build-std.sh x86_64|aarch64|all
#
# In localhost/feos-toolchain, offline, the tree at /src and the feos-ports-target volume
# at /build. It builds what it needs first (each a no-op when built and unchanged):
# cbindgen, relibc for feos, the sysroot (ports/sysroot.sh) and libunwind
# (ports/build-libunwind.sh). Then:
#
# 1. /build/rust-src-sysroot: the image's rust 1.99.1 (its rustc and the shared
#    libraries next to it, copied, so rustc finds this as its sysroot), the host's
#    rustlib linked in, and lib/rustlib/src/rust pointing at ports/src/rust, so that
#    -Zbuild-std builds the patched library/ (std's feos arms, and libc from
#    ports/src/libc through library/Cargo.toml's [patch]; lib/rustlib/src/libc points
#    at ports/src/libc for it).
# 2. cargo build --release -Zbuild-std=std,panic_unwind of ports/test/rust/std-build
#    with that rustc, for ports/targets/<arch>-unknown-feos.json (RUSTC_BOOTSTRAP=1,
#    -Zjson-target-spec), crates from ports/vendor; cargo's output in /build/std-target.
# 3. /build/rust-sysroot/lib/rustlib/<arch>-unknown-feos/lib: std's rlibs, and the
#    feos sysroot's crt0.o, crti.o, crtn.o, libc.a, libm.a, libcompiler_builtins.a and
#    the empty archives, with libunwind.a. A program then builds with
#        rustc --target ports/targets/<arch>-unknown-feos.json --sysroot /build/rust-sysroot
#    (ports/test/rust/build.sh does, for the T3 tests).
set -eu
P=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 1 ] || { echo "usage: $0 x86_64|aarch64|all" >&2; exit 2; }
if [ "$1" = all ]; then
    "$0" x86_64 && exec "$0" aarch64
fi
arch=$1
case "$arch" in x86_64|aarch64) ;; *) echo "build-std: unknown arch $arch" >&2; exit 2 ;; esac
target=$arch-unknown-feos
out=${PORTS_BUILD:-/build}
export RUSTUP_TOOLCHAIN=1.99.1 CARGO_NET_OFFLINE=true

[ -x "$out/bin/cbindgen" ] || "$P/build-cbindgen.sh"
"$P/build-relibc-feos.sh" "$arch"
"$P/sysroot.sh" "$arch"
"$P/build-libunwind.sh" "$arch"

# 1. The source sysroot.
host_root=$(rustc --print sysroot)
host=$(rustc -vV | sed -n 's/^host: //p')
src_root=$out/rust-src-sysroot
if [ "$(cat "$src_root/.host" 2>/dev/null)" != "$(rustc -vV | sha256sum)" ]; then
    rm -rf "$src_root"
    mkdir -p "$src_root/bin" "$src_root/lib/rustlib/src"
    cp "$host_root/bin/rustc" "$src_root/bin/"
    cp -P "$host_root"/lib/*.so* "$src_root/lib/"
    ln -s "$host_root/lib/rustlib/$host" "$src_root/lib/rustlib/$host"
    rustc -vV | sha256sum > "$src_root/.host"
fi
ln -sfn "$P/src/rust" "$src_root/lib/rustlib/src/rust"
# library/Cargo.toml's [patch] names libc as ../../libc, which cargo resolves on this
# (uncanonicalised) path.
ln -sfn "$P/src/libc" "$src_root/lib/rustlib/src/libc"
[ "$("$src_root/bin/rustc" --print sysroot)" = "$src_root" ] ||
    { echo "build-std: $src_root/bin/rustc doesn't take $src_root as its sysroot" >&2; exit 1; }

# 2. std.
# The toolchain's cargo itself, not rustup's proxy: the proxy puts the host toolchain's
# lib/ in LD_LIBRARY_PATH, where the copied rustc would find librustc_driver and so take
# the host's sysroot (and its unpatched library/) after all.
unset LD_LIBRARY_PATH
export RUSTC="$src_root/bin/rustc" RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR="$out/std-target"
# Cargo takes -Zbuild-std's crates as non-local and never looks at their sources'
# mtimes, so an edit to library/, libc or the target spec would go unbuilt: when anything
# there is newer than the last build, build this target's std from scratch.
built=$CARGO_TARGET_DIR/$target/.feos-built
if [ -f "$built" ] && [ -n "$(find "$P/src/rust/library" "$P/src/libc/src" "$P/src/libc/Cargo.toml" \
        "$P/src/libc/build.rs" "$P/targets/$target.json" "$0" -newer "$built" -print -quit)" ]; then
    echo "build-std: sources changed since the last build; building $target's std again"
    # (the host build scripts too: cargo would keep std's old build.rs binary)
    rm -rf "${CARGO_TARGET_DIR:?}/$target" "${CARGO_TARGET_DIR:?}/release"
fi
mkdir -p "$CARGO_TARGET_DIR/$target"
start=$CARGO_TARGET_DIR/$target/.feos-start
touch "$start"
cd "$P/test/rust/std-build"
"$host_root/bin/cargo" build --release -Zbuild-std=std,panic_unwind -Zjson-target-spec \
    --target "$P/targets/$target.json"

mv "$start" "$built"

# 3. The sysroot rustc uses.
deps=$CARGO_TARGET_DIR/$target/release/deps
lib=$out/rust-sysroot/lib/rustlib/$target/lib
rm -rf "$lib.partial"
mkdir -p "$lib.partial"
for f in "$deps"/lib*.rlib; do
    case "$(basename "$f")" in libfeos_std_build-*) continue ;; esac
    cp "$f" "$lib.partial/"
done
cp "$out/sysroot/$target/usr/lib"/*.o "$out/sysroot/$target/usr/lib"/*.a "$lib.partial/"
cp "$out/libunwind/$target/libunwind.a" "$lib.partial/"
rm -rf "$lib"
mv "$lib.partial" "$lib"
ls "$lib" | grep -q '^libstd-' || { echo "build-std: no libstd in $lib" >&2; exit 1; }
echo "build-std: $target sysroot at $out/rust-sysroot ($(ls "$lib" | wc -l) files)"
