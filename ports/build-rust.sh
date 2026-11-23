#!/bin/sh
# Build the feos Rust toolchain (track T4): rustc 1.99.1 with x86_64-unknown-feos and
# aarch64-unknown-feos built in, from ports/src/rust with LLVM, clang and lld from its
# own src/llvm-project, std for x86_64-unknown-linux-gnu and both feos targets, and
# cargo; then lay it out as one toolchain directory.
#
#   ports/build-rust.sh [X.PY ARGS...]
#
# In localhost/feos-toolchain, offline, the tree at /src and the feos-ports-target
# volume at /build (ports/test/container.sh NAME ports/build-rust.sh). Hours the first
# time (LLVM with clang, then rustc); after that bootstrap rebuilds only what changed.
#
# 1. The C side the feos targets need, each a no-op when built and unchanged: cbindgen,
#    relibc for feos and the sysroot (/build/sysroot/<arch>-unknown-feos) for both
#    architectures. The targets' C compiler in ports/rust-bootstrap.toml is
#    ports/toolchain/bin/<arch>-unknown-feos-cc, which takes its sysroot from
#    FEOS_SYSROOT_<ARCH>; bootstrap builds LLVM's libunwind with it and copies relibc's
#    crt0.o, crti.o, crtn.o, libc.a, libm.a and libcompiler_builtins.a from it into
#    each target's lib/rustlib/<target>/lib (`-print-file-name`).
# 2. x.py build --stage 1 library cargo with ports/rust-bootstrap.toml; bootstrap's
#    output in /build/rust-build.
# 3. /build/rust-feos: the stage 1 sysroot (bin/rustc, bin/rustdoc, lib/, lib/rustlib/
#    with std for the three targets and rust-lld), with bin/cargo beside rustc and
#    lib/rustlib/src/{rust,libc} the library sources (rust-src). A program then builds with
#        /build/rust-feos/bin/rustc --target x86_64-unknown-feos hello.rs
#    and a crate with
#        /build/rust-feos/bin/cargo build --target aarch64-unknown-feos
#    (PATH=/build/rust-feos/bin:$PATH, or RUSTC=/build/rust-feos/bin/rustc).
set -eu
P=$(cd "$(dirname "$0")" && pwd)
out=${PORTS_BUILD:-/build}
host=x86_64-unknown-linux-gnu
export RUSTUP_TOOLCHAIN=1.99.1 CARGO_NET_OFFLINE=true

# 1. relibc and the sysroots.
[ -x "$out/bin/cbindgen" ] || "$P/build-cbindgen.sh"
for arch in x86_64 aarch64; do
    "$P/build-relibc-feos.sh" "$arch"
    "$P/sysroot.sh" "$arch"
done
export FEOS_SYSROOT_X86_64="$out/sysroot/x86_64-unknown-feos"
export FEOS_SYSROOT_AARCH64="$out/sysroot/aarch64-unknown-feos"

# 2. bootstrap.
build=$out/rust-build
# bootstrap.py builds bootstrap itself with cargo in ports/src/rust, where
# ports/.cargo/config.toml's `build.target = "host-tuple"` puts the binary in
# bootstrap/<host>/debug/bootstrap, not the bootstrap/debug/bootstrap bootstrap.py
# runs: link the one to the other. (Only the file: bootstrap/debug is cargo's host
# directory, with a lock of its own.)
mkdir -p "$build/bootstrap/debug"
ln -sfn "../$host/debug/bootstrap" "$build/bootstrap/debug/bootstrap"
cd "$P/src/rust"
python3 x.py build --config "$P/rust-bootstrap.toml" --stage 1 library cargo "$@"

# 3. The toolchain.
stage1=$build/$host/stage1
cargo_bin=$(ls -t "$build/$host"/stage*-tools-bin/cargo 2>/dev/null | head -n 1)
[ -x "$stage1/bin/rustc" ] || { echo "build-rust: no $stage1/bin/rustc" >&2; exit 1; }
[ -n "$cargo_bin" ] || { echo "build-rust: no cargo in $build/$host/stage*-tools-bin" >&2; exit 1; }
tc=$out/rust-feos
rm -rf "$tc.partial"
mkdir -p "$tc.partial"
cp -a "$stage1/bin" "$stage1/lib" "$tc.partial/"
cp "$cargo_bin" "$tc.partial/bin/cargo"
# rust-src, what -Zbuild-std and rust-analyzer read: the patched library/ and the
# libunwind sources, as the rust-src component has them, and the libc crate that
# library/Cargo.toml's [patch] names (../../libc). Copied rather than made by
# `x.py dist rust-src`, which also runs `cargo vendor` of the library's dependencies
# outside ports/ (so without ports/vendor) and can't follow that [patch]
# (docs/toolchain.md, deviations); the library's crates.io dependencies come from
# ports/vendor as for every build under ports/.
src=$tc.partial/lib/rustlib/src
rm -rf "$src"
mkdir -p "$src/rust/src/llvm-project"
rsync -a --exclude .git --exclude target "$P/src/rust/library" "$src/rust/"
rsync -a --exclude .git "$P/src/rust/src/llvm-project/libunwind" "$src/rust/src/llvm-project/"
rsync -a --exclude .git --exclude target "$P/src/libc/" "$src/libc/"
rm -rf "$tc"
mv "$tc.partial" "$tc"
for t in x86_64-unknown-feos aarch64-unknown-feos; do
    for f in crt0.o crti.o crtn.o libc.a libm.a libcompiler_builtins.a libunwind.a; do
        [ -f "$tc/lib/rustlib/$t/lib/$f" ] || { echo "build-rust: no $f for $t" >&2; exit 1; }
    done
    ls "$tc/lib/rustlib/$t/lib" | grep -q '^libstd-' || { echo "build-rust: no std for $t" >&2; exit 1; }
done
[ -x "$tc/lib/rustlib/$host/bin/rust-lld" ] || { echo "build-rust: no rust-lld" >&2; exit 1; }
"$tc/bin/rustc" -vV
"$tc/bin/cargo" -V
echo "build-rust: the toolchain is $tc"
