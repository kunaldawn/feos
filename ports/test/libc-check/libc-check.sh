#!/bin/sh
# The libc crate's feos module (ports/src/libc/src/unix/feos) against relibc's feos
# headers and libraries, for each architecture: every exported type's size, alignment
# and signedness, every public struct field's offset and size, every integer constant's
# value, every C function's signature and presence in libc.a/libm.a. See libc_check.py.
#
#   ports/test/libc-check/libc-check.sh [x86_64|aarch64 ...]     # default both
#
# In localhost/feos-toolchain, offline, after ports/build-std.sh (the probe builds
# against its sysroot's core) and ports/sysroot.sh. Output in
# /build/libc-check/<arch>/.
set -eu
P=$(cd "$(dirname "$0")/../.." && pwd)
H=$P/test/libc-check
out=${PORTS_BUILD:-/build}
export RUSTUP_TOOLCHAIN=1.99.1 RUSTC_BOOTSTRAP=1
status=0
for arch in ${*:-x86_64 aarch64}; do
    target=$arch-unknown-feos
    dir=$out/libc-check/$arch
    rm -rf "$dir"
    mkdir -p "$dir"
    [ -d "$out/rust-sysroot/lib/rustlib/$target/lib" ] ||
        { echo "libc-check: no std for $target; run ports/build-std.sh $arch" >&2; exit 1; }
    # 1. What the crate exports for feos, cfgs resolved.
    rustdoc --edition 2021 --target "$P/targets/$target.json" --sysroot "$out/rust-sysroot" \
        -Zunstable-options --output-format json --document-private-items --cap-lints allow \
        --crate-name libc "$P/src/libc/src/lib.rs" -o "$dir" 2>"$dir/rustdoc.log" ||
        { cat "$dir/rustdoc.log" >&2; exit 1; }
    # 2. rustc's numbers for it, in an object file: the crate as it is in the tree (not
    #    std's copy, which may be older), then the probe against it.
    rustc -Zunstable-options --edition 2021 --crate-type rlib --crate-name libc -O --cap-lints allow \
        --target "$P/targets/$target.json" --sysroot "$out/rust-sysroot" \
        -o "$dir/liblibc.rlib" "$P/src/libc/src/lib.rs"
    python3 -I "$H/libc_check.py" gen-probe "$arch" "$dir/libc.json" "$dir/probe.rs"
    rustc -Zunstable-options --edition 2021 --crate-type lib --emit obj -O --target "$P/targets/$target.json" \
        --sysroot "$out/rust-sysroot" --extern "libc=$dir/liblibc.rlib" \
        -o "$dir/probe.o" "$dir/probe.rs"
    # 3. relibc's, and the comparison.
    python3 -I "$H/libc_check.py" "$arch" "$dir/libc.json" "$dir/probe.o" \
        "$out/relibc/$target" "$dir" || status=1
done
exit $status
