#!/bin/sh
# The no-cargo runner's build half: compile stage T3's Rust programs (t3/*.rs) for
# feos with rustc directly, against the std sysroot ports/build-std.sh made.
#
#   ports/test/rust/build.sh [x86_64|aarch64 ...]      # default both
#
# In localhost/feos-toolchain with the feos-ports-target volume at /build, after
# ports/build-std.sh. Each t3/NAME.rs (underscores becoming dashes) lands in
# /build/t3/<arch>/t3-NAME, built with
#
#   rustc -Zunstable-options --edition 2021 -O --target ports/targets/<arch>-unknown-feos.json \
#         --sysroot /build/rust-sysroot -L native=/build/rust-sysroot/lib/rustlib/<arch>-unknown-feos/lib \
#         -C debuginfo=0 -C strip=debuginfo
#
# (the -L because rustc 1.99.1 searches the target's own lib/ for native libraries only
# on Linux, Fuchsia, Fortanix and AIX targets: rustc_session's
# walk_native_lib_search_dirs; feos joins that list when the target is built in, T4)
# (panic=unwind, the targets' default), and checked as build-t1.sh checks C programs:
# a static ET_EXEC at the feos image base with no PT_INTERP or PT_DYNAMIC, no writable
# and executable segment, its PT_LOADs on pages of their own; and it must have a
# PT_GNU_EH_FRAME, which unwinding needs.
#
# With RUST_FEOS=DIR (track T4) the programs are built instead by that toolchain's
# rustc (ports/build-rust.sh's /build/rust-feos) for its built-in targets, with no JSON
# spec, --sysroot, -L or RUSTC_BOOTSTRAP, into /build/t4/t3/<arch>:
#
#   DIR/bin/rustc --edition 2021 -O --target <arch>-unknown-feos -C debuginfo=0 -C strip=debuginfo
set -eu
P=$(cd "$(dirname "$0")/../.." && pwd)
out=${PORTS_BUILD:-/build}
if [ -n "${RUST_FEOS:-}" ]; then
    unset RUSTC_BOOTSTRAP
    outdir=$out/t4/t3
else
    export RUSTUP_TOOLCHAIN=1.99.1 RUSTC_BOOTSTRAP=1
    outdir=$out/t3
fi
for arch in ${*:-x86_64 aarch64}; do
    target=$arch-unknown-feos
    if [ -n "${RUST_FEOS:-}" ]; then
        [ -d "$RUST_FEOS/lib/rustlib/$target/lib" ] ||
            { echo "build: no std for $target in $RUST_FEOS; run ports/build-rust.sh" >&2; exit 1; }
    else
        [ -d "$out/rust-sysroot/lib/rustlib/$target/lib" ] ||
            { echo "build: no std for $target; run ports/build-std.sh $arch" >&2; exit 1; }
    fi
    dest=$outdir/$arch
    rm -rf "$dest"
    mkdir -p "$dest"
    for src in "$P"/test/rust/t3/*.rs; do
        name=t3-$(basename "$src" .rs | tr _ -)
        if [ -n "${RUST_FEOS:-}" ]; then
            "$RUST_FEOS/bin/rustc" --edition 2021 -O -C debuginfo=0 -C strip=debuginfo \
                --target "$target" \
                --crate-name "$(echo "$name" | tr - _)" -o "$dest/$name" "$src"
        else
            rustc -Zunstable-options --edition 2021 -O -C debuginfo=0 -C strip=debuginfo \
                --target "$P/targets/$target.json" --sysroot "$out/rust-sysroot" \
                -L "native=$out/rust-sysroot/lib/rustlib/$target/lib" \
                --crate-name "$(echo "$name" | tr - _)" -o "$dest/$name" "$src"
        fi
        "$P/test/rust/check-elf.sh" "$dest/$name"
        echo "build: $arch $name ($(stat -c %s "$dest/$name") bytes)"
    done
done
