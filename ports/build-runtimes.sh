#!/bin/sh
# Build LLVM's runtimes for feos with the feos clang (track T5): compiler-rt's builtins
# and crtbegin/crtend, libunwind, libc++abi and libc++, static, against the relibc
# sysroot, from ports/src/rust/src/llvm-project.
#
#   ports/build-runtimes.sh x86_64|aarch64|all
#
# In localhost/feos-toolchain, offline, after ports/build-clang.sh and ports/sysroot.sh.
# CMake runs with ports/cmake/<arch>-unknown-feos.cmake (CMAKE_SYSTEM_NAME Feos) and
# --sysroot the relibc sysroot (/build/sysroot/<triple> as built from relibc alone; the
# runtimes' own headers are never taken from it). Output, /build/runtimes/<triple>:
#   lib/clang/NN/lib/<triple>/libclang_rt.builtins.a, clang_rt.crtbegin.o, clang_rt.crtend.o
#   usr/include/c++/v1/            libc++'s headers, its __config_site among them
#   usr/lib/libc++.a libc++abi.a libc++experimental.a libunwind.a
# Then ports/sysroot.sh puts them into the sysroot (usr/ and usr/lib/clang/NN/) and
# this script copies compiler-rt into the toolchain's resource directory and the
# sysroot into /build/llvm-feos/<triple>, clang's default sysroot.
#
# What relibc can't back yet is off and recorded in docs/toolchain.md (T5): no shared
# libraries (1f.1), no time zone database.
set -eu
P=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 1 ] || { echo "usage: $0 x86_64|aarch64|all" >&2; exit 2; }
if [ "$1" = all ]; then
    "$0" x86_64 && exec "$0" aarch64
fi
arch=$1
case "$arch" in x86_64|aarch64) ;; *) echo "build-runtimes: unknown arch $arch" >&2; exit 2 ;; esac
target=$arch-unknown-feos
out=${PORTS_BUILD:-/build}
tc=$out/llvm-feos
src=$P/src/rust/src/llvm-project
[ -x "$tc/bin/clang" ] || { echo "build-runtimes: no $tc/bin/clang; run ports/build-clang.sh" >&2; exit 1; }
"$P/sysroot.sh" "$arch" --libc-only
export FEOS_LLVM=$tc FEOS_SYSROOT=$out/sysroot-libc/$target
ver=$("$tc/bin/clang" -dumpversion | cut -d. -f1)
dest=$out/runtimes/$target
work=$out/runtimes-build/$target

stamp=$( (cat "$0" "$P/cmake/$target.cmake" "$P/cmake/Platform/Feos.cmake";
    sha256sum "$tc/bin/clang-$ver" "$tc/bin/lld";
    cat "$FEOS_SYSROOT/usr/share/feos-sysroot/MANIFEST";
    git -C "$src" diff HEAD -- compiler-rt libunwind libcxxabi libcxx runtimes cmake llvm/cmake 2>/dev/null ||
        find "$src/compiler-rt" "$src/libunwind" "$src/libcxxabi" "$src/libcxx" -newer "$0" -type f) |
    sha256sum | cut -d' ' -f1)
if [ -f "$dest/usr/lib/libc++.a" ] && [ "$(cat "$dest/.stamp" 2>/dev/null)" = "$stamp" ]; then
    echo "build-runtimes: $target is up to date"
else
    rm -rf "$dest" "$work"
    mkdir -p "$dest" "$work"
    common="-G Ninja -DCMAKE_TOOLCHAIN_FILE=$P/cmake/$target.cmake -DCMAKE_BUILD_TYPE=Release
        -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY -DLLVM_INCLUDE_TESTS=OFF
        -DLLVM_INCLUDE_DOCS=OFF -DCMAKE_C_FLAGS=-ffunction-sections -DCMAKE_CXX_FLAGS=-ffunction-sections"

    # 1. compiler-rt: the builtins (with the generic __atomic_* functions, which C++'s
    #    std::atomic of a large type calls) and crtbegin/crtend (__dso_handle and
    #    __cxa_finalize from .fini_array; no frame registry: libunwind finds frames
    #    through PT_GNU_EH_FRAME, and the registry's weak calls can't be left undefined
    #    in an executable at 64 GiB).
    # shellcheck disable=SC2086
    cmake -S "$src/runtimes" -B "$work/compiler-rt" $common \
        -DLLVM_ENABLE_RUNTIMES=compiler-rt \
        -DLLVM_ENABLE_PER_TARGET_RUNTIME_DIR=ON \
        -DCOMPILER_RT_INSTALL_PATH="$dest/lib/clang/$ver" \
        -DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON \
        -DCOMPILER_RT_BUILD_BUILTINS=ON -DCOMPILER_RT_BUILD_CRT=ON \
        -DCOMPILER_RT_CRT_USE_EH_FRAME_REGISTRY=OFF \
        -DCOMPILER_RT_EXCLUDE_ATOMIC_BUILTIN=OFF \
        -DCOMPILER_RT_BUILD_SANITIZERS=OFF -DCOMPILER_RT_BUILD_XRAY=OFF \
        -DCOMPILER_RT_BUILD_LIBFUZZER=OFF -DCOMPILER_RT_BUILD_PROFILE=OFF \
        -DCOMPILER_RT_BUILD_MEMPROF=OFF -DCOMPILER_RT_BUILD_ORC=OFF \
        -DCOMPILER_RT_BUILD_CTX_PROFILE=OFF -DCOMPILER_RT_BUILD_GWP_ASAN=OFF \
        -DCOMPILER_RT_BUILD_SCUDO_STANDALONE_WITH_LLVM_LIBC=OFF
    ninja -C "$work/compiler-rt" -j6 install

    # 2. libunwind, libc++abi, libc++: static, threads on relibc's pthreads, libc++abi
    #    and libunwind as libraries of their own (Rust's std links the same libunwind).
    # shellcheck disable=SC2086
    cmake -S "$src/runtimes" -B "$work/cxx" $common \
        -DCMAKE_INSTALL_PREFIX="$dest/usr" \
        -DLLVM_ENABLE_RUNTIMES="libunwind;libcxxabi;libcxx" \
        -DLLVM_ENABLE_PER_TARGET_RUNTIME_DIR=OFF \
        -DLIBUNWIND_ENABLE_SHARED=OFF -DLIBUNWIND_USE_COMPILER_RT=ON \
        -DLIBCXXABI_ENABLE_SHARED=OFF -DLIBCXXABI_USE_COMPILER_RT=ON \
        -DLIBCXXABI_USE_LLVM_UNWINDER=ON \
        -DLIBCXXABI_STATICALLY_LINK_UNWINDER_IN_STATIC_LIBRARY=OFF \
        -DLIBCXXABI_HAS_CXA_THREAD_ATEXIT_IMPL=ON \
        -DLIBCXX_ENABLE_SHARED=OFF -DLIBCXX_CXX_ABI=libcxxabi \
        -DLIBCXX_ENABLE_STATIC_ABI_LIBRARY=OFF -DLIBCXX_USE_COMPILER_RT=ON \
        -DLIBCXX_HAS_PTHREAD_API=ON -DLIBCXX_HAS_MUSL_LIBC=OFF \
        -DLIBCXX_ENABLE_FILESYSTEM=ON -DLIBCXX_ENABLE_LOCALIZATION=ON \
        -DLIBCXX_ENABLE_WIDE_CHARACTERS=ON -DLIBCXX_ENABLE_RANDOM_DEVICE=ON \
        -DLIBCXX_ENABLE_TIME_ZONE_DATABASE=OFF \
        -DLIBCXX_INSTALL_MODULES=OFF -DLIBCXX_INCLUDE_BENCHMARKS=OFF
    ninja -C "$work/cxx" -j6 install
    echo "$stamp" > "$dest/.stamp"
fi

# 3. Into the sysroot (which /bin/sysroot-check verifies on feos) and the toolchain.
"$P/sysroot.sh" "$arch"
mkdir -p "$tc/lib/clang/$ver/lib"
rm -rf "$tc/lib/clang/$ver/lib/$target"
cp -R "$dest/lib/clang/$ver/lib/$target" "$tc/lib/clang/$ver/lib/$target"
rm -rf "$tc/$target.partial"
cp -R "$out/sysroot/$target" "$tc/$target.partial"
rm -rf "$tc/$target"
mv "$tc/$target.partial" "$tc/$target"
echo "build-runtimes: $dest, $tc/$target"
