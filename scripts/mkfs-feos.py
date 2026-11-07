#!/usr/bin/env python3
"""Formats an image with FEOS's native filesystem, for a test's disk.

    scripts/mkfs-feos.py IMAGE [CREATED]

The format is libs/fs/src/native.rs, and `format` there is the real one: a host test checks
that this writes the same bytes. It's here because what boots a test has Python, not Rust.
"""

import os
import struct
import sys
import zlib

BLOCK = 4096
JOURNAL_MAX = (BLOCK - 24) // 4
INODES_PER_BLOCK = BLOCK // 128
VERSION = 1


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__)
    path = sys.argv[1]
    created = int(sys.argv[2]) if len(sys.argv) == 3 else 1
    blocks = os.path.getsize(path) // BLOCK
    if blocks < 64:
        sys.exit("too small")
    blocks = min(blocks, 0xFFFFFFFF)
    bitmap_blocks = -(-blocks // (BLOCK * 8))
    inode_blocks = max(blocks // 64, 4)
    journal_blocks = max(32, min(blocks // 32, JOURNAL_MAX + 2))
    bitmap_start = 1
    inode_start = bitmap_start + bitmap_blocks
    journal_start = inode_start + inode_blocks
    data_start = journal_start + journal_blocks
    inodes = inode_blocks * INODES_PER_BLOCK

    with open(path, "r+b") as image:
        def write(block, data):
            image.seek(block * BLOCK)
            image.write(data)

        zero = bytes(BLOCK)
        write(journal_start, zero)
        for block in range(inode_start, inode_start + inode_blocks):
            write(block, zero)
        for index in range(bitmap_blocks):
            first = index * BLOCK * 8
            bitmap = bytearray(BLOCK)
            for bit in range(BLOCK * 8):
                block = first + bit
                if block < data_start or block >= blocks:
                    bitmap[bit // 8] |= 1 << (bit % 8)
            write(bitmap_start + index, bytes(bitmap))

        root = bytearray(128)
        struct.pack_into("<H", root, 0, 0o040000 | 0o755)
        struct.pack_into("<I", root, 12, 1)
        struct.pack_into("<QQQ", root, 24, created, created, created)
        struct.pack_into("<I", root, 124, zlib.crc32(bytes(root[:124])))
        write(inode_start, bytes(root) + bytes(BLOCK - 128))

        superblock = bytearray(BLOCK)
        superblock[:8] = b"FEOSNFS1"
        struct.pack_into("<II", superblock, 8, VERSION, BLOCK)
        struct.pack_into("<9I", superblock, 16, blocks, inodes, bitmap_start, bitmap_blocks,
                         inode_start, inode_blocks, journal_start, journal_blocks, data_start)
        struct.pack_into("<Q", superblock, 52, created)
        struct.pack_into("<I", superblock, 508, zlib.crc32(bytes(superblock[:508])))
        write(0, bytes(superblock))


if __name__ == "__main__":
    main()
