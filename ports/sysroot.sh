#!/bin/sh
# Lay out relibc's feos build as a sysroot: the tree a C compiler for feos is pointed at,
# and what goes into the feos image for native builds (docs/selfhost-plan.md phase 2).
#
#   ports/sysroot.sh x86_64|aarch64|all [--libc-only]
#
# In localhost/feos-toolchain with the feos-ports-target volume at /build, after
# ports/build-relibc-feos.sh (it builds nothing itself). From
# /build/relibc/<arch>-unknown-feos it makes /build/sysroot/<arch>-unknown-feos:
#
#   usr/include/                    relibc's headers (cbindgen's and its own)
#   usr/lib/crt0.o crt1.o           relibc's start (crt1.o a copy, for drivers that name it)
#   usr/lib/crti.o crtn.o           .init/.fini prologue and epilogue
#   usr/lib/libc.a                  relibc with openlibm
#   usr/lib/libm.a                  openlibm
#   usr/lib/libcompiler_builtins.a  compiler-rt's builtins for C code, Rust's build of them
#   usr/lib/lib{pthread,dl,rt,crypt,util,resolv,xnet}.a
#                                   empty, as relibc's own install and musl's: libc.a has
#                                   all of it, and build systems still pass -lpthread
#   usr/share/feos-sysroot/MANIFEST size and FNV-1a hash of every file above, which
#                                   /bin/sysroot-check verifies on feos
#
# With LLVM's runtimes built (ports/build-runtimes.sh, track T5, in
# /build/runtimes/<arch>-unknown-feos), they join it:
#
#   usr/include/c++/v1/             libc++'s headers
#   usr/lib/libc++.a libc++abi.a libc++experimental.a libunwind.a
#   usr/lib/clang/NN/lib/<triple>/  compiler-rt's builtins, crtbegin and crtend (clang's
#                                   resource directory when clang runs on feos as /usr/bin/clang)
#
# --libc-only makes /build/sysroot-libc/<arch>-unknown-feos instead, relibc alone: what
# the runtimes are built against.
#
# No libc.so, ld.so or /lib/ld64.so.1 yet: feos runs static programs until kernel stage
# 1f.1. The sysroot is a plain directory with no links (the feos image has none).
# ports/toolchain/bin/feos-cc takes it as FEOS_SYSROOT (its usr/ layout is found).
set -eu
P=$(cd "$(dirname "$0")" && pwd)
[ $# -eq 1 ] || [ $# -eq 2 ] && [ "${2:---libc-only}" = --libc-only ] ||
    { echo "usage: $0 x86_64|aarch64|all [--libc-only]" >&2; exit 2; }
if [ "$1" = all ]; then
    "$0" x86_64 ${2:-} && exec "$0" aarch64 ${2:-}
fi
arch=$1
case "$arch" in x86_64|aarch64) ;; *) echo "sysroot: unknown arch $arch" >&2; exit 2 ;; esac
target=$arch-unknown-feos
out=${PORTS_BUILD:-/build}
from=$out/relibc/$target
dest=$out/sysroot/$target
runtimes=$out/runtimes/$target
if [ "${2:-}" = --libc-only ]; then
    dest=$out/sysroot-libc/$target
    runtimes=
fi
[ -f "$from/lib/libc.a" ] || { echo "sysroot: no $from; run ports/build-relibc-feos.sh $arch" >&2; exit 1; }

rm -rf "$dest.partial"
mkdir -p "$dest.partial/usr/include" "$dest.partial/usr/lib" "$dest.partial/usr/share/feos-sysroot"
cp -RL "$from/include/." "$dest.partial/usr/include/"
for f in crt0.o crti.o crtn.o libc.a libm.a libcompiler_builtins.a; do
    cp "$from/lib/$f" "$dest.partial/usr/lib/$f"
done
cp "$from/lib/crt0.o" "$dest.partial/usr/lib/crt1.o"
for lib in pthread dl rt crypt util resolv xnet; do
    llvm-ar -rcs "$dest.partial/usr/lib/lib$lib.a"
done
if [ -n "$runtimes" ] && [ -f "$runtimes/usr/lib/libc++.a" ]; then
    cp -R "$runtimes/usr/include/c++" "$dest.partial/usr/include/c++"
    for f in libc++.a libc++abi.a libc++experimental.a libunwind.a; do
        [ ! -f "$runtimes/usr/lib/$f" ] || cp "$runtimes/usr/lib/$f" "$dest.partial/usr/lib/$f"
    done
    cp -R "$runtimes/lib/clang" "$dest.partial/usr/lib/clang"
fi
python3 -I - "$dest.partial" <<'PY'
import os, sys
root = sys.argv[1]
lines = []
for d, dirs, files in os.walk(root):
    dirs.sort()
    for f in sorted(files):
        path = os.path.join(d, f)
        if os.path.islink(path):
            sys.exit(f"sysroot: {path} is a link")
        rel = "/" + os.path.relpath(path, root)
        if rel.startswith("/usr/share/feos-sysroot/"):
            continue
        if any(part.startswith(".") for part in rel.split("/")[1:]):
            sys.exit(f"sysroot: {rel} is hidden (the feos image leaves out dotfiles)")
        h = 0xcbf29ce484222325
        data = open(path, "rb").read()
        for b in data:
            h = ((h ^ b) * 0x100000001b3) & 0xffffffffffffffff
        lines.append(f"{len(data)} {h:016x} {rel}\n")
open(os.path.join(root, "usr/share/feos-sysroot/MANIFEST"), "w").writelines(lines)
print(f"sysroot: {len(lines)} files")
PY
rm -rf "$dest"
mv "$dest.partial" "$dest"
echo "sysroot: $dest"
