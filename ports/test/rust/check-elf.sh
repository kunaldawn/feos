#!/bin/sh
# Check that each FILE is a feos program as ports/test/build-t1.sh checks C ones: a
# static ET_EXEC at the feos image base (0x1000400000) with no PT_INTERP or
# PT_DYNAMIC, no writable and executable segment, the entry in an executable segment,
# its PT_LOADs on pages of their own, and a PT_GNU_EH_FRAME, which unwinding needs.
#
#   ports/test/rust/check-elf.sh FILE...
set -eu
for f in "$@"; do
    name=$(basename "$f")
    headers=$(llvm-readelf -h -l "$f")
    echo "$headers" | grep -q 'Type: *EXEC' || { echo "check-elf: $name isn't ET_EXEC" >&2; exit 1; }
    if echo "$headers" | grep -Eq '^ *(INTERP|DYNAMIC) '; then
        echo "check-elf: $name isn't static" >&2; exit 1
    fi
    echo "$headers" | grep -q 'GNU_EH_FRAME' || { echo "check-elf: $name has no PT_GNU_EH_FRAME" >&2; exit 1; }
    echo "$headers" | grep -q '0x0000001000400000' || { echo "check-elf: $name not at the image base" >&2; exit 1; }
    python3 -I - "$f" <<'PY'
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
done
