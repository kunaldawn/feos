#!/usr/bin/env python3
"""Generates the FEOS-DOS CPU's per-instruction test vectors from the host CPU.

No published 386 vector set was available offline, so the reference is the x86
processor this runs on: every case below is assembled with nasm into a small
32-bit Linux program, runs natively, and its registers and flags are recorded. A
flag the Intel manuals leave undefined for a case is masked out, so a difference
between a 386 and a modern processor there can't fail a test.

    python3 scripts/gen-dos-cpu-vectors.py libs/dosbox/src/cpu/vectors

It needs nasm, ld and a kernel that runs 32-bit programs. Each output line is a JSON
object, and each file is one JSON array:

    {"n":"add eax,ebx","c":"01D8","m":2261,"i":[eax,ecx,edx,ebx,flags],"f":[...]}

`c` is the code, `m` the flags that are checked, `i` the state before and `f` after.
Only the flags in `m` matter in `f`.
"""
import json
import os
import platform
import struct
import subprocess
import sys
import tempfile

CF, PF, AF, ZF, SF, OF = 1, 4, 0x10, 0x40, 0x80, 0x800
ALL = CF | PF | AF | ZF | SF | OF
SZP = SF | ZF | PF
BASE_FLAGS = 0x202

V8 = [0, 1, 0x0F, 0x7F, 0x80, 0xA5, 0xFF]
V16 = [0, 1, 0x00FF, 0x1234, 0x7FFF, 0x8000, 0xFFFF]
V32 = [0, 1, 0x12345678, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFF]
# What's above the operand in each register, to see that it's preserved.
FILL = 0xDEADBE00

REG = {
    8: ("al", "bl", "dl"),
    16: ("ax", "bx", "dx"),
    32: ("eax", "ebx", "edx"),
}


def with_fill(value, bits, fill=FILL):
    if bits == 32:
        return value
    mask = (1 << bits) - 1
    return (fill & ~mask & 0xFFFFFFFF) | value


def values(bits):
    return {8: V8, 16: V16, 32: V32}[bits]


def alu_cases():
    for bits in (8, 16, 32):
        a, b, _ = REG[bits]
        for name in ("add", "or", "adc", "sbb", "and", "sub", "xor", "cmp", "test"):
            for x in values(bits):
                for y in values(bits):
                    for carry in ((0, 1) if name in ("adc", "sbb") else (0,)):
                        mask = ALL
                        if name in ("or", "and", "xor", "test"):
                            mask = CF | OF | SZP
                        yield f"{name} {a},{b}", mask, [
                            with_fill(x, bits), 0x11111111, 0x22222222, with_fill(y, bits),
                            BASE_FLAGS | carry,
                        ]
        for name in ("inc", "dec", "neg", "not"):
            for x in values(bits):
                for carry in (0, 1):
                    mask = ALL if name != "not" else ALL
                    yield f"{name} {a}", mask, [
                        with_fill(x, bits), 0x11111111, 0x22222222, 0x33333333, BASE_FLAGS | carry,
                    ]


def shift_mask(name, bits, count):
    masked = count & 31
    if masked == 0:
        return ALL
    if name in ("rol", "ror"):
        return CF | (OF if masked == 1 else 0) | AF | SZP
    if name in ("rcl", "rcr"):
        effective = masked % (bits + 1)
        if effective == 0:
            return ALL
        return CF | (OF if masked == 1 else 0) | AF | SZP
    mask = SZP
    if masked < bits:
        mask |= CF
    if masked == 1:
        mask |= OF
    return mask


def shift_cases():
    for bits in (8, 16, 32):
        a, _, _ = REG[bits]
        for name in ("rol", "ror", "rcl", "rcr", "shl", "shr", "sar"):
            for x in values(bits):
                for count in (0, 1, 2, 7, 8, 15, 16, 31):
                    for carry in (0, 1):
                        yield f"{name} {a},cl", shift_mask(name, bits, count), [
                            with_fill(x, bits), 0x11111100 | count, 0x22222222, 0x33333333,
                            BASE_FLAGS | carry,
                        ]
    for bits in (16, 32):
        a, b, _ = REG[bits]
        for name in ("shld", "shrd"):
            for x in values(bits):
                for y in (values(bits)[2], values(bits)[-1]):
                    for count in (0, 1, 2, 8, 15) + ((16, 31) if bits == 32 else ()):
                        mask = ALL if count == 0 else (CF | SZP | (OF if count == 1 else 0))
                        yield f"{name} {a},{b},cl", mask, [
                            with_fill(x, bits), 0x11111100 | count, 0x22222222, with_fill(y, bits),
                            BASE_FLAGS,
                        ]


def mul_cases():
    for bits in (8, 16, 32):
        _, b, _ = REG[bits]
        top = (1 << bits) - 1
        for name in ("mul", "imul"):
            for x in values(bits):
                for y in values(bits):
                    yield f"{name} {b}", CF | OF, [
                        with_fill(x, bits), 0x11111111, 0x22222222, with_fill(y, bits), BASE_FLAGS,
                    ]
        for x in values(bits):
            for y in values(bits):
                if y == 0:
                    continue
                high = 0
                dividend = x
                signed_x = x - (1 << bits) if x >> (bits - 1) else x
                signed_y = y - (1 << bits) if y >> (bits - 1) else y
                # DIV: the quotient always fits with no high part.
                edx = 0x22220000 if bits != 32 else 0
                eax = with_fill(x, bits)
                if bits == 8:
                    eax = (FILL & 0xFFFF0000) | x
                yield f"div {b}", 0, [eax, 0x11111111, edx, with_fill(y, bits), BASE_FLAGS]
                # IDIV: the sign extended into the high part, when it fits.
                quotient = abs(signed_x) // abs(signed_y)
                if quotient >= (1 << (bits - 1)):
                    continue
                high = top if signed_x < 0 else 0
                if bits == 8:
                    eax = (FILL & 0xFFFF0000) | (high << 8) | x
                    edx = 0x22222222
                elif bits == 16:
                    edx = 0x22220000 | high
                else:
                    edx = high
                yield f"idiv {b}", 0, [eax, 0x11111111, edx, with_fill(y, bits), BASE_FLAGS]
    for bits in (16, 32):
        a, b, _ = REG[bits]
        for x in values(bits):
            for y in values(bits):
                yield f"imul {a},{b}", CF | OF, [
                    with_fill(x, bits), 0x11111111, 0x22222222, with_fill(y, bits), BASE_FLAGS,
                ]


def bcd_cases():
    for name, mask in (
        ("daa", CF | AF | SZP),
        ("das", CF | AF | SZP),
        ("aaa", CF | AF),
        ("aas", CF | AF),
        ("aam", SZP),
        ("aad", SZP),
    ):
        for al in list(range(0, 256, 3)) + [0xFF, 0x0A, 0x99, 0x9A]:
            for flags in (0, CF, AF, CF | AF):
                yield name, mask, [
                    0xABCD0300 | al, 0x11111111, 0x22222222, 0x33333333, BASE_FLAGS | flags,
                ]


def bit_cases():
    for bits in (16, 32):
        a, b, _ = REG[bits]
        for name in ("bt", "bts", "btr", "btc"):
            for x in values(bits):
                for bit in (0, 1, 7, 15, 16, 31):
                    if bits == 16 and bit >= 16:
                        continue
                    yield f"{name} {a},{b}", CF, [
                        with_fill(x, bits), 0x11111111, 0x22222222, with_fill(bit, bits), BASE_FLAGS,
                    ]
        for name in ("bsf", "bsr"):
            for y in values(bits):
                if y == 0:
                    continue
                yield f"{name} {a},{b}", ZF, [
                    with_fill(0x5555, bits), 0x11111111, 0x22222222, with_fill(y, bits), BASE_FLAGS,
                ]
        for name in ("xadd", "cmpxchg"):
            for x in values(bits):
                for y in values(bits):
                    yield f"{name} {b},{a}" if name == "xadd" else f"{name} {b},{a}", ALL, [
                        with_fill(x, bits), 0x11111111, 0x22222222, with_fill(y, bits), BASE_FLAGS,
                    ]
    for name in ("cbw", "cwde", "cwd", "cdq"):
        for x in V16 + V32:
            yield name, ALL, [x, 0x11111111, 0x22222222, 0x33333333, BASE_FLAGS]
    for x in V32:
        yield "bswap eax", ALL, [x, 0x11111111, 0x22222222, 0x33333333, BASE_FLAGS]
    for name in ("movzx eax,bl", "movsx eax,bl", "movzx eax,bx", "movsx eax,bx", "movzx ax,bl", "movsx ax,bl"):
        for y in V8 + V16:
            yield name, ALL, [0x77777777, 0x11111111, 0x22222222, FILL | (y & 0xFF) if "bl" in name else y, BASE_FLAGS]
    for code in range(16):
        condition = ("o no b ae e ne be a s ns p np l ge le g".split())[code]
        for flags in (0, CF, ZF, SF, OF, PF, CF | ZF, SF | OF, ZF | SF):
            yield f"set{condition} al", ALL, [0x77777777, 0x11111111, 0x22222222, 0x33333333, BASE_FLAGS | flags]


GROUPS = {
    "alu": alu_cases,
    "shift": shift_cases,
    "muldiv": mul_cases,
    "bcd": bcd_cases,
    "bits": bit_cases,
}


def run(cases, work):
    """Returns the code of each instruction form and the final state of each case."""
    forms = sorted({case[0] for case in cases})
    source = ["bits 32", "section .text", "global _start", "_start:"]
    out = ["section .data", "stack: times 256 dd 0", "stack_top:", "section .bss", "results:"]
    source.append("mov esp, stack_top")
    for name, _, init in cases:
        eax, ecx, edx, ebx, flags = init
        source += [
            f"push dword {flags}", "popfd",
            f"mov eax,{eax}", f"mov ecx,{ecx}", f"mov edx,{edx}", f"mov ebx,{ebx}",
            name,
            "mov [save_a],eax", "pushfd", "pop eax",
            "mov edi,[cursor]", "mov [edi+16],eax", "mov eax,[save_a]",
            "mov [edi],eax", "mov [edi+4],ecx", "mov [edi+8],edx", "mov [edi+12],ebx",
            "add dword [cursor],20",
        ]
    source += [
        "mov eax,4", "mov ebx,1", "mov ecx,buffer", f"mov edx,{len(cases) * 20}", "int 0x80",
        "mov eax,1", "mov ebx,0", "int 0x80",
        "section .data", "save_a: dd 0", "cursor: dd buffer", "stack: times 256 dd 0", "stack_top:",
        "section .bss", f"buffer: resb {len(cases) * 20}",
    ]
    asm = os.path.join(work, "x.asm")
    with open(asm, "w") as file:
        file.write("\n".join(source))
    subprocess.check_call(["nasm", "-f", "elf32", "-O0", asm, "-o", os.path.join(work, "x.o")])
    subprocess.check_call(["ld", "-m", "elf_i386", os.path.join(work, "x.o"), "-o", os.path.join(work, "x")])
    data = subprocess.check_output([os.path.join(work, "x")])
    finals = [struct.unpack_from("<5I", data, i * 20) for i in range(len(cases))]

    codes = {}
    for index, form in enumerate(forms):
        path = os.path.join(work, "f.asm")
        with open(path, "w") as file:
            file.write(f"bits 32\n{form}\n")
        subprocess.check_call(["nasm", "-f", "bin", "-O0", path, "-o", os.path.join(work, "f.bin")])
        with open(os.path.join(work, "f.bin"), "rb") as file:
            codes[form] = file.read().hex().upper()
    return codes, finals


def main():
    target = sys.argv[1]
    os.makedirs(target, exist_ok=True)
    for group, make in GROUPS.items():
        cases = list(make())
        with tempfile.TemporaryDirectory() as work:
            codes, finals = run(cases, work)
        lines = []
        for (name, mask, init), final in zip(cases, finals):
            record = {"n": name, "c": codes[name], "m": mask, "i": init, "f": list(final)}
            lines.append(json.dumps(record, separators=(",", ":")))
        with open(os.path.join(target, f"{group}.json"), "w") as file:
            file.write("[\n" + ",\n".join(lines) + "\n]\n")
        print(group, len(cases), "cases")


if __name__ == "__main__":
    main()
