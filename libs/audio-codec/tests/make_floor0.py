#!/usr/bin/env python3
"""Writes data/floor0.ogg: a Vorbis stream that uses floor type 0.

No encoder has made these since 2002, so this builds one by hand: one channel,
two seconds' worth of 256-sample blocks with pseudo-random LSP vectors and residue.
ffmpeg's decoder is the reference:

    python3 make_floor0.py && ffmpeg -i data/floor0.ogg -f s16le data/floor0.ogg.s16
"""

import random
import struct


class Bits:
    def __init__(self):
        self.value = 0
        self.count = 0

    def put(self, value, bits):
        self.value |= (value & ((1 << bits) - 1)) << self.count
        self.count += bits

    def bytes(self):
        return self.value.to_bytes((self.count + 7) // 8, "little")


def float32(value):
    """Vorbis's own packing."""
    sign = 0x80000000 if value < 0 else 0
    value = abs(value)
    exponent = 788 - 768 + 1
    mantissa = 0
    if value:
        exponent = 788
        while value < (1 << 20):
            value *= 2
            exponent -= 1
        mantissa = int(value)
    return sign | (exponent << 21) | mantissa


def codebook(bits, dims, length_list, lookup=None):
    bits.put(0x564342, 24)
    bits.put(dims, 16)
    bits.put(len(length_list), 24)
    bits.put(0, 1)  # not ordered
    bits.put(0, 1)  # not sparse
    for length in length_list:
        bits.put(length - 1, 5)
    if lookup is None:
        bits.put(0, 4)
        return
    minimum, delta, value_bits, multiplicands, sequence = lookup
    bits.put(1, 4)
    bits.put(float32(minimum), 32)
    bits.put(float32(delta), 32)
    bits.put(value_bits - 1, 4)
    bits.put(sequence, 1)
    for value in multiplicands:
        bits.put(value, value_bits)


def identification():
    return (
        b"\x01vorbis"
        + struct.pack("<IBIiii", 0, 1, 22050, 0, 64000, 0)
        + bytes([0x88, 1])
    )


def comment():
    vendor = b"feos test"
    return b"\x03vorbis" + struct.pack("<I", len(vendor)) + vendor + struct.pack("<I", 0) + b"\x01"


def setup():
    bits = Bits()
    bits.put(3 - 1, 8)
    # Book 0: LSP vectors. Book 1: residue classes. Book 2: residue values.
    # Cumulative values, so each vector continues to rise, as LSP positions must.
    codebook(bits, 2, [2] * 4, (0.25, 0.12, 1, [0, 1], 1))
    codebook(bits, 1, [1, 1])
    codebook(bits, 2, [4] * 16, (-1.5, 1.0, 2, [0, 1, 2, 3], 0))
    bits.put(0, 6)  # one time transform
    bits.put(0, 16)
    bits.put(0, 6)  # one floor
    bits.put(0, 16)  # type 0
    bits.put(8, 8)  # order
    bits.put(22050, 16)  # rate
    bits.put(64, 16)  # bark map size
    bits.put(6, 6)  # amplitude bits
    bits.put(40, 8)  # amplitude offset
    bits.put(0, 4)  # one book
    bits.put(0, 8)
    bits.put(0, 6)  # one residue
    bits.put(1, 16)  # type 1
    bits.put(0, 24)
    bits.put(128, 24)
    bits.put(4 - 1, 24)
    bits.put(0, 6)  # one classification
    bits.put(1, 8)  # classbook
    bits.put(1, 3)  # cascade: pass 0
    bits.put(0, 1)
    bits.put(2, 8)  # the book for pass 0
    bits.put(0, 6)  # one mapping
    bits.put(0, 16)
    bits.put(0, 1)  # one submap
    bits.put(0, 1)  # no coupling
    bits.put(0, 2)
    bits.put(0, 8)
    bits.put(0, 8)
    bits.put(0, 8)
    bits.put(0, 6)  # one mode
    bits.put(0, 1)
    bits.put(0, 16)
    bits.put(0, 16)
    bits.put(0, 8)
    bits.put(1, 1)  # framing
    return b"\x05vorbis" + bits.bytes()


def audio(rng):
    bits = Bits()
    bits.put(0, 1)
    bits.put(rng.randrange(1, 4), 6)  # amplitude
    bits.put(0, 1)  # book number: ilog(1) bits
    # Four vectors of two: order 8.
    for _ in range(4):
        bits.put(rng.randrange(4), 2)
    for _ in range(32):  # partitions
        bits.put(0, 1)  # class
        for _ in range(2):
            bits.put(rng.choice([5, 6, 9, 10, 0, 15]), 4)
    return bits.bytes()


CRC = []
for i in range(256):
    r = i << 24
    for _ in range(8):
        r = (r << 1) ^ 0x04c11db7 if r & 0x80000000 else r << 1
    CRC.append(r & 0xffffffff)


def page(payloads, sequence, granule, flags):
    table = bytearray()
    body = b""
    for payload in payloads:
        size = len(payload)
        while size >= 255:
            table.append(255)
            size -= 255
        table.append(size)
        body += payload
    head = b"OggS\x00" + bytes([flags]) + struct.pack("<qIII", granule, 0x46454f53, sequence, 0)
    head += bytes([len(table)]) + bytes(table)
    data = bytearray(head + body)
    crc = 0
    for byte in data:
        crc = ((crc << 8) & 0xffffffff) ^ CRC[((crc >> 24) ^ byte) & 0xff]
    data[22:26] = struct.pack("<I", crc)
    return bytes(data)


def main():
    rng = random.Random(1234)
    out = page([identification()], 0, 0, 2)
    out += page([comment(), setup()], 1, 0, 0)
    packets = [audio(rng) for _ in range(120)]
    sequence = 2
    granule = 0
    for at in range(0, len(packets), 20):
        chunk = packets[at : at + 20]
        granule += 128 * (len(chunk) - (1 if at == 0 else 0))
        last = at + 20 >= len(packets)
        out += page(chunk, sequence, granule, 4 if last else 0)
        sequence += 1
    with open("data/floor0.ogg", "wb") as f:
        f.write(out)


main()
