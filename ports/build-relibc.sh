#!/bin/sh
# Build relibc from ports/src/relibc, offline, and install its sysroot.
#
#   ports/build-relibc.sh TARGET       # e.g. x86_64-unknown-linux-gnu
#
# In localhost/feos-toolchain, with the tree at /src and the feos-ports-target
# volume at /build; needs /build/bin/cbindgen (ports/build-cbindgen.sh). Cargo's
# output goes to /build/relibc-target, the sysroot (include/, lib/) to
# /build/relibc/TARGET. Nothing is written into ports/src/relibc.
set -eu
P=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 1 ] || { echo "usage: $0 TARGET" >&2; exit 2; }
target=$1
out=${PORTS_BUILD:-/build}
[ -x "$out/bin/cbindgen" ] || { echo "build-relibc: build cbindgen first (ports/build-cbindgen.sh)" >&2; exit 1; }
# relibc pins a nightly; the image's 1.99.1 builds it with RUSTC_BOOTSTRAP for
# -Zbuild-std (std's crates come from ports/vendor, as for the x86_64 kernel).
export RUSTUP_TOOLCHAIN=1.99.1 RUSTC_BOOTSTRAP=1 CARGO_NET_OFFLINE=true
export PATH="$out/bin:$PATH"
export CARGO_TARGET_DIR="$out/relibc-target"
# relibc's Makefile copies openlibm into the build once (its rule depends on the
# directory's own mtime), so an edited openlibm file (ports/PATCHES.md) would never reach
# libm: start that copy again when a file in it is newer.
if [ -d "$CARGO_TARGET_DIR/$target/openlibm" ] && [ -n "$(find "$P/src/relibc/openlibm" -type f -newer "$CARGO_TARGET_DIR/$target/openlibm" -print -quit)" ]; then
    rm -rf "$CARGO_TARGET_DIR/$target/openlibm"
fi
cd "$P/src/relibc"
make TARGET="$target" all
rm -rf "$out/relibc/$target.partial"
make TARGET="$target" install DESTDIR="$out/relibc/$target.partial"
rm -rf "$out/relibc/$target"
mv "$out/relibc/$target.partial" "$out/relibc/$target"
echo "build-relibc: sysroot at $out/relibc/$target"
ls "$out/relibc/$target/lib"
