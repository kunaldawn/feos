#!/bin/sh
# Compile stage T1's C programs (test/c/t1/*.c) for feos against relibc's sysroot, with
# the host clang and ld.lld through ports/toolchain/bin/<arch>-unknown-feos-cc.
#
#   ports/test/build-t1.sh [x86_64|aarch64 ...]      # default both
#
# In localhost/feos-toolchain with the feos-ports-target volume at /build, after
# ports/build-relibc-feos.sh. The programs land in /build/t1/<arch>/t1-NAME, each
# checked to be a static ET_EXEC with no PT_INTERP or PT_DYNAMIC, linked at the feos
# image base, its PT_LOADs on pages of their own.
set -eu
P=$(cd "$(dirname "$0")/.." && pwd)
out=${PORTS_BUILD:-/build}
export PATH="$P/toolchain/bin:$PATH"
for arch in ${*:-x86_64 aarch64}; do
    dest=$out/t1/$arch
    rm -rf "$dest"
    mkdir -p "$dest"
    for src in "$P"/test/c/t1/*.c; do
        name=t1-$(basename "$src" .c)
        "$arch-unknown-feos-cc" -O2 -Wl,--gc-sections,-s -Wall -Wextra -Wno-unused-parameter -o "$dest/$name" "$src"
        headers=$(llvm-readelf -h -l "$dest/$name")
        echo "$headers" | grep -q 'Type: *EXEC' || { echo "build-t1: $name isn't ET_EXEC" >&2; exit 1; }
        if echo "$headers" | grep -Eq '^ *(INTERP|DYNAMIC) '; then
            echo "build-t1: $name isn't static" >&2; exit 1
        fi
        echo "$headers" | grep -q '0x0000001000400000' || { echo "build-t1: $name not at the image base" >&2; exit 1; }
        python3 - "$dest/$name" <<'PY'
import struct, sys
data = open(sys.argv[1], 'rb').read()
phoff, = struct.unpack_from('<Q', data, 32)
phentsize, phnum = struct.unpack_from('<HH', data, 54)
loads = []
for i in range(phnum):
    kind, flags, off, vaddr, paddr, filesz, memsz, align = struct.unpack_from('<IIQQQQQQ', data, phoff + i * phentsize)
    if kind == 1 and memsz:
        loads.append((vaddr & ~0xfff, (vaddr + memsz + 0xfff) & ~0xfff, flags))
        if flags & 2 and flags & 1:
            sys.exit(f"{sys.argv[1]}: a writable and executable segment")
entry, = struct.unpack_from('<Q', data, 24)
if not any(a <= entry < b and f & 1 for a, b, f in loads):
    sys.exit(f"{sys.argv[1]}: the entry {entry:#x} isn't in an executable segment")
loads.sort()
for (a, b, _), (c, d, _) in zip(loads, loads[1:]):
    if b > c:
        sys.exit(f"{sys.argv[1]}: segments share a page ({b:#x} > {c:#x})")
PY
        echo "build-t1: $arch $name ($(stat -c %s "$dest/$name") bytes)"
    done
done
