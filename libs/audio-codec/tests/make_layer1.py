#!/usr/bin/env python3
"""Writes data/layer1.mp1: MPEG-1 Layer I frames with random contents.

Nothing on the host encodes Layer I, but ffmpeg decodes it, and any bits at all
are a valid frame once the header is right. The reference:

    python3 make_layer1.py && ffmpeg -i data/layer1.mp1 -f s16le data/layer1.mp1.s16
"""

import random

rng = random.Random(77)
out = bytearray()
for _ in range(60):
    bits = []

    def put(value, count):
        for at in range(count - 1, -1, -1):
            bits.append((value >> at) & 1)

    # Layer I, MPEG-1, no CRC, 192 kbps at 44.1 kHz, no padding, joint stereo
    # with bound 8: 11 sync bits, version 11, layer 11, protection 1.
    put(0x7ff, 11)
    put(3, 2)
    put(3, 2)
    put(1, 1)
    put(6, 4)  # 192 kbps
    put(0, 2)  # 44.1 kHz
    put(0, 1)
    put(0, 1)
    put(1, 2)  # joint stereo
    put(1, 2)  # bound 8
    put(0, 4)
    frame_bytes = 12 * 192000 // 44100 * 4
    # Allocation: stereo bands below the bound, then shared. Small steps only.
    alloc = []
    for band in range(32):
        channels = 2 if band < 8 else 1
        row = []
        for _ch in range(channels):
            code = rng.choice([0, 3, 4]) if band < 8 else 0
            put(code, 4)
            row.append(code)
        alloc.append(row)
    for band in range(32):
        row = alloc[band] if band < 8 else alloc[band] * 2
        for code in row:
            if code:
                put(rng.randrange(14, 40), 6)  # scale factor
    for _sample in range(12):
        for band in range(32):
            for code in alloc[band]:
                if code:
                    width = code + 1
                    put(rng.randrange(1, (1 << width) - 1), width)
    while len(bits) < frame_bytes * 8:
        bits.append(0)
    frame = bytearray()
    for at in range(0, len(bits), 8):
        value = 0
        for bit in bits[at : at + 8]:
            value = value * 2 + bit
        frame.append(value)
    out += frame[:frame_bytes]
open("data/layer1.mp1", "wb").write(out)
