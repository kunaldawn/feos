#!/bin/sh
# Build clang with the feos target (track T5) and lay out the feos C/C++ toolchain.
#
#   ports/build-clang.sh
#
# In localhost/feos-toolchain, offline, the tree at /src and the feos-ports-target
# volume at /build (ports/test/container.sh NAME ports/build-clang.sh). It reuses T4's
# LLVM build (rust's bootstrap configured it in /build/rust-build/<host>/llvm/build from
# ports/src/rust/src/llvm-project, with clang; lld in .../lld): ninja rebuilds what the
# feos driver changed (clang's Driver and Basic libraries, the clang binary), nothing
# else. bootstrap's own install of LLVM is left alone; its next run sees the changed
# llvm-project and reinstalls (docs/toolchain.md, T5).
#
# Output, /build/llvm-feos:
#   bin/clang-NN, clang, clang++, clang-cpp, <arch>-unknown-feos-clang{,++}
#   bin/ld.lld (lld), llvm-ar, llvm-ranlib, llvm-nm, llvm-objcopy, llvm-strip,
#       llvm-objdump, llvm-readelf, llvm-size, llvm-addr2line
#   lib/clang/NN/include            clang's own headers
#   lib/clang/NN/lib/<triple>/      compiler-rt (ports/build-runtimes.sh)
#   <triple>/                       the sysroot (ports/sysroot.sh's, with the runtimes),
#                                   clang's default for --target=<triple>
# A program then builds with no flags beyond the target:
#   /build/llvm-feos/bin/clang --target=x86_64-unknown-feos hello.c -o hello
set -eu
P=$(cd "$(dirname "$0")" && pwd)
out=${PORTS_BUILD:-/build}
host=x86_64-unknown-linux-gnu
llvm=$out/rust-build/$host/llvm/build
lld=$out/rust-build/$host/lld
[ -f "$llvm/build.ninja" ] || { echo "build-clang: no LLVM build in $llvm; run ports/build-rust.sh (T4)" >&2; exit 1; }
[ -x "$lld/bin/lld" ] || { echo "build-clang: no lld in $lld; run ports/build-rust.sh (T4)" >&2; exit 1; }

# LLVM_PARALLEL_LINK_JOBS is 1 in that build already (ports/rust-bootstrap.toml).
ninja -C "$llvm" -j6 clang clang-resource-headers llvm-ar llvm-nm llvm-objcopy \
    llvm-objdump llvm-readobj llvm-size llvm-symbolizer FileCheck not count split-file

ver=$(sed -n 's/^CMAKE_PROJECT_VERSION_MAJOR:STATIC=//p' "$llvm/CMakeCache.txt")
[ -n "$ver" ] || ver=$(sed -n 's/^LLVM_VERSION_MAJOR:STATIC=//p' "$llvm/CMakeCache.txt")
[ -n "$ver" ] || { echo "build-clang: no LLVM version in $llvm/CMakeCache.txt" >&2; exit 1; }
tc=$out/llvm-feos
mkdir -p "$tc/bin" "$tc/lib/clang/$ver"
install -m 755 "$llvm/bin/clang-$ver" "$tc/bin/clang-$ver"
for n in clang clang++ clang-cpp x86_64-unknown-feos-clang x86_64-unknown-feos-clang++ \
        aarch64-unknown-feos-clang aarch64-unknown-feos-clang++; do
    ln -sfn "clang-$ver" "$tc/bin/$n"
done
install -m 755 "$lld/bin/lld" "$tc/bin/lld"
ln -sfn lld "$tc/bin/ld.lld"
for t in llvm-ar llvm-nm llvm-objcopy llvm-objdump llvm-readobj llvm-size llvm-symbolizer; do
    install -m 755 "$llvm/bin/$t" "$tc/bin/$t"
done
ln -sfn llvm-ar "$tc/bin/llvm-ranlib"
ln -sfn llvm-objcopy "$tc/bin/llvm-strip"
ln -sfn llvm-readobj "$tc/bin/llvm-readelf"
ln -sfn llvm-symbolizer "$tc/bin/llvm-addr2line"
rm -rf "$tc/lib/clang/$ver/include"
cp -R "$llvm/lib/clang/$ver/include" "$tc/lib/clang/$ver/include"
"$tc/bin/clang" --version | head -n 1
echo "build-clang: $tc"
