#!/bin/sh
# Build stage T5's feos programs with the feos clang (ports/build-clang.sh's
# /build/llvm-feos, its default sysroot made by ports/build-runtimes.sh) and the T4
# rustc (/build/rust-feos), into /build/t5/<arch>/t5-*.
#
#   ports/test/t5/build.sh [x86_64|aarch64 ...]      (default both)
#
# In localhost/feos-toolchain, offline. C is built with `clang --target=<triple>` and
# nothing else; C++ with clang++ and -O2 (and -std=c++20 where a test needs it). The
# Rust panic test links the sysroot's libunwind (T5's) ahead of rust's own copy, and
# --trace-symbol shows which archive gave _Unwind_RaiseException. Each binary is
# checked by ports/test/rust/check-elf.sh.
set -eu
P=$(cd "$(dirname "$0")/../.." && pwd)
here=$P/test/t5
out=${PORTS_BUILD:-/build}
tc=${FEOS_LLVM:-$out/llvm-feos}
rust=${RUST_FEOS:-$out/rust-feos}
unset RUSTC_BOOTSTRAP RUSTUP_TOOLCHAIN
export PATH="$tc/bin:$rust/bin:$PATH"
[ $# -gt 0 ] || set -- x86_64 aarch64
for arch in "$@"; do
    t=$arch-unknown-feos
    sysroot=$tc/$t
    [ -f "$sysroot/usr/lib/libc++.a" ] || { echo "t5-build: no libc++ in $sysroot; run ports/build-runtimes.sh $arch" >&2; exit 1; }
    dest=$out/t5/$arch
    work=$dest/work
    rm -rf "$dest"
    mkdir -p "$work"
    set -x
    clang --target=$t "$here/c-hello.c" -o "$dest/t5-c-hello"
    clang --target=$t -O2 "$P/test/c/suite/sysroot-check.c" -o "$dest/t5-sysroot-check"
    clang++ --target=$t -O2 "$here/cxx-hello.cpp" -o "$dest/t5-cxx-hello"
    clang --target=$t -O2 -c "$here/cxx-except-c.c" -o "$work/cxx-except-c.o"
    clang++ --target=$t -O2 "$here/cxx-except.cpp" "$work/cxx-except-c.o" -o "$dest/t5-cxx-except"
    clang++ --target=$t -O2 -std=c++20 -pthread "$here/cxx-threads.cpp" -o "$dest/t5-cxx-threads"
    clang++ --target=$t -O2 -std=c++17 "$here/cxx-fs.cpp" -o "$dest/t5-cxx-fs"
    clang++ --target=$t -O2 -std=c++17 "$here/cxx-strtod.cpp" -o "$dest/t5-cxx-strtod"
    rustc --target $t -O --edition 2021 "$here/rust-panic.rs" -o "$dest/t5-rust-panic" \
        -L "native=$sysroot/usr/lib" -C link-arg=--trace-symbol=_Unwind_RaiseException \
        > "$work/rust-panic.link" 2>&1 || { cat "$work/rust-panic.link"; exit 1; }
    rustc --target $t -O --edition 2021 --crate-type staticlib "$here/mixed.rs" \
        -o "$work/libt5mixed.a" --print native-static-libs
    clang++ --target=$t -O2 "$here/mixed.cpp" -L"$work" -lt5mixed -o "$dest/t5-mixed" \
        -Wl,--trace-symbol=_Unwind_RaiseException > "$work/mixed.link" 2>&1 ||
        { cat "$work/mixed.link"; exit 1; }
    set +x
    cat "$work/rust-panic.link" "$work/mixed.link"
    grep -q "$t/usr/lib/libunwind.a(.*definition of _Unwind_RaiseException" "$work/rust-panic.link" ||
        { echo "t5-build: t5-rust-panic didn't take the sysroot's libunwind" >&2; exit 1; }
    grep -q "$t/usr/lib/libunwind.a(.*definition of _Unwind_RaiseException" "$work/mixed.link" ||
        { echo "t5-build: t5-mixed didn't take the sysroot's libunwind" >&2; exit 1; }
    "$P/test/rust/check-elf.sh" "$dest"/t5-*
    ls -l "$dest"
done
