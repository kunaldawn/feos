#!/usr/bin/env python3
"""Generates libs/gfx/src/font_extended.rs, the 8x8 glyphs beyond ASCII.

    scripts/gen-font.py <directory of console fonts>

The glyphs come from the VGA8 console fonts of Debian's console-setup, normally in
/usr/share/consolefonts: see LICENSING.md. Each is an 8x8 PSF1 file with a Unicode table.
A few missing letters are composed or drawn here. The result is checked in, so building
needs none of this. ASCII isn't included: that's
font8x8 in font.rs, whose pixels are what the rest of the system was drawn with.
"""

import gzip
import os
import struct
import sys

# First listed wins, so the fuller, plainer fonts go first.
SOURCES = ["Uni2-VGA8", "Uni1-VGA8", "FullGreek-VGA8", "FullCyrSlav-VGA8", "Lat15-VGA8", "Lat2-VGA8", "Lat7-VGA8", "Lat38-VGA8"]

RANGES = [
    (0x00A0, 0x017F, "Latin-1, Latin Extended-A"),
    (0x0370, 0x03FF, "Greek"),
    (0x0400, 0x045F, "Cyrillic"),
    (0x2010, 0x2027, "punctuation"),
    (0x20A0, 0x20BF, "currency"),
    (0x2190, 0x2199, "arrows"),
    (0x2500, 0x259F, "box drawing and blocks"),
    (0x25A0, 0x25FF, "geometric shapes"),
    (0xFFFD, 0xFFFD, "replacement character"),
]


# Those fonts' Unicode tables point most of Greek at the wrong bitmaps (alpha at a sharp s,
# Omega at a box corner), and Cyrillic Pe and Ef with it, and their box drawing has one
# shape for light, heavy and double. So Greek and these are drawn here, and box drawing and
# block elements are generated from what each one is. See the checks in font.rs.
GREEK_DRAWN = {
    0x0393: ["#######.", "##...##.", "##......", "##......", "##......", "##......", "####....", "........"],
    0x0394: ["...#....", "..###...", ".##.##..", ".##.##..", "##...##.", "##...##.", "#######.", "........"],
    0x0398: [".#####..", "##...##.", "##...##.", "#######.", "##...##.", "##...##.", ".#####..", "........"],
    0x039B: ["...#....", "..###...", ".##.##..", ".##.##..", "##...##.", "##...##.", "##...##.", "........"],
    0x039E: ["#######.", "........", "........", ".#####..", "........", "........", "#######.", "........"],
    0x03A0: ["#######.", ".##.##..", ".##.##..", ".##.##..", ".##.##..", ".##.##..", ".##.##..", "........"],
    0x03A3: ["#######.", "##...##.", ".##.....", "..##....", ".##.....", "##...##.", "#######.", "........"],
    0x03A6: ["...##...", ".######.", "##.##.##", "##.##.##", "##.##.##", ".######.", "...##...", "........"],
    0x03A8: ["##.##.##", "##.##.##", "##.##.##", ".######.", "...##...", "...##...", "..####..", "........"],
    0x03A9: [".#####..", "##...##.", "##...##.", "##...##.", ".##.##..", ".##.##..", "###.###.", "........"],
    0x03B1: ["........", "........", ".###.##.", "##.###..", "##..##..", "##.###..", ".###.##.", "........"],
    0x03B2: ["..####..", ".##..##.", ".##..##.", ".#####..", ".##..##.", ".##..##.", ".#####..", "##......"],
    0x03B3: ["........", "........", "##...##.", ".##.##..", "..###...", "..##....", "..##....", "..##...."],
    0x03B4: ["..####..", ".##.....", "..##....", ".#####..", "##...##.", "##...##.", ".#####..", "........"],
    0x03B5: ["........", "........", ".#####..", "##......", ".####...", "##......", ".#####..", "........"],
    0x03B6: ["#######.", "...##...", "..##....", ".##.....", "##......", ".#####..", ".....##.", "...###.."],
    0x03B7: ["........", "........", "#.####..", ".##..##.", ".##..##.", ".##..##.", ".##..##.", ".....##."],
    0x03B8: ["..###...", ".##.##..", "##...##.", "#######.", "##...##.", ".##.##..", "..###...", "........"],
    0x03B9: ["........", "........", ".###....", "..##....", "..##....", "..##.##.", "...###..", "........"],
    0x03BA: ["........", "........", "##..##..", "##.##...", "####....", "##.##...", "##..##..", "........"],
    0x03BB: ["##......", ".##.....", "..##....", "..###...", ".##.##..", "##...##.", "##...##.", "........"],
    0x03BC: ["........", "........", ".##..##.", ".##..##.", ".##..##.", ".##..##.", ".#####.#", "##......"],
    0x03BD: ["........", "........", "##...##.", "##...##.", ".##.##..", ".##.##..", "..###...", "........"],
    0x03BE: [".#####..", "##......", ".####...", "##......", ".#####..", ".....##.", "...###..", "........"],
    0x03C0: ["........", "........", "#######.", ".##.##..", ".##.##..", ".##.##..", ".##..###", "........"],
    0x03C1: ["........", "........", ".#####..", "##...##.", "##...##.", "######..", "##......", "##......"],
    0x03C2: ["........", "........", ".#####..", "##......", "##......", ".#####..", ".....##.", "...###.."],
    0x03C3: ["........", "........", ".######.", "##.##...", "##..##..", "##..##..", ".####...", "........"],
    0x03C4: ["........", "........", "#######.", "...##...", "...##...", "...##.##", "....###.", "........"],
    0x03C5: ["........", "........", "##...##.", "##...##.", "##...##.", "##...##.", ".#####..", "........"],
    0x03C6: ["........", "...##...", ".######.", "##.##.##", "##.##.##", ".######.", "...##...", "...##..."],
    0x03C7: ["........", "........", "##...##.", ".##.##..", "..###...", ".##.##..", "##...##.", "........"],
    0x03C8: ["........", "...##...", "##.##.##", "##.##.##", "##.##.##", ".######.", "...##...", "...##..."],
    0x03C9: ["........", "........", ".##.##..", "##.#.##.", "##.#.##.", "##.#.##.", ".##.##..", "........"],
    # Cyrillic Pe and Ef, whose shapes these are.
    0x041F: ["#######.", ".##.##..", ".##.##..", ".##.##..", ".##.##..", ".##.##..", ".##.##..", "........"],
    0x0424: ["...##...", ".######.", "##.##.##", "##.##.##", "##.##.##", ".######.", "...##...", "........"],
}

# Greek capitals that are Latin ones, by their ASCII letter.
GREEK_AS_LATIN = {
    0x0391: "A", 0x0392: "B", 0x0395: "E", 0x0396: "Z", 0x0397: "H", 0x0399: "I", 0x039A: "K", 0x039C: "M",
    0x039D: "N", 0x039F: "O", 0x03A1: "P", 0x03A4: "T", 0x03A5: "Y", 0x03A7: "X", 0x03BF: "o",
}

TONOS = ("...##...", "..##....")
DIAERESIS = ".##.##.."
DIAERESIS_TONOS = ".#.#.#.."
# Small letters with an accent: its rows over the letter's lower six.
GREEK_SMALL_ACCENTED = {
    0x03AC: 0x03B1, 0x03AD: 0x03B5, 0x03AE: 0x03B7, 0x03AF: 0x03B9, 0x03CC: 0x03BF, 0x03CD: 0x03C5, 0x03CE: 0x03C9,
}
GREEK_SMALL_DIAERESIS = {0x03CA: (0x03B9, DIAERESIS), 0x03CB: (0x03C5, DIAERESIS), 0x0390: (0x03B9, DIAERESIS_TONOS), 0x03B0: (0x03C5, DIAERESIS_TONOS)}
# Capitals have no room: one row of accent, and the capital one row down.
GREEK_CAPITAL_ACCENTED = {
    0x0386: (0x0391, "##......"), 0x0388: (0x0395, "##......"), 0x0389: (0x0397, "##......"), 0x038A: (0x0399, "##......"),
    0x038C: (0x039F, "##......"), 0x038E: (0x03A5, "##......"), 0x038F: (0x03A9, "##......"),
    0x03AA: (0x0399, DIAERESIS), 0x03AB: (0x03A5, DIAERESIS),
}

# Arms of box drawing characters: up, right, down, left; 0 none, 1 light, 2 heavy, 3 double.
BOX = {
    0x2500: (0, 1, 0, 1), 0x2501: (0, 2, 0, 2), 0x2502: (1, 0, 1, 0), 0x2503: (2, 0, 2, 0),
    0x250C: (0, 1, 1, 0), 0x250D: (0, 2, 1, 0), 0x250E: (0, 1, 2, 0), 0x250F: (0, 2, 2, 0),
    0x2510: (0, 0, 1, 1), 0x2511: (0, 0, 1, 2), 0x2512: (0, 0, 2, 1), 0x2513: (0, 0, 2, 2),
    0x2514: (1, 1, 0, 0), 0x2515: (1, 2, 0, 0), 0x2516: (2, 1, 0, 0), 0x2517: (2, 2, 0, 0),
    0x2518: (1, 0, 0, 1), 0x2519: (1, 0, 0, 2), 0x251A: (2, 0, 0, 1), 0x251B: (2, 0, 0, 2),
    0x251C: (1, 1, 1, 0), 0x251D: (1, 2, 1, 0), 0x251E: (2, 1, 1, 0), 0x251F: (1, 1, 2, 0),
    0x2520: (2, 1, 2, 0), 0x2521: (2, 2, 1, 0), 0x2522: (1, 2, 2, 0), 0x2523: (2, 2, 2, 0),
    0x2524: (1, 0, 1, 1), 0x2525: (1, 0, 1, 2), 0x2526: (2, 0, 1, 1), 0x2527: (1, 0, 2, 1),
    0x2528: (2, 0, 2, 1), 0x2529: (2, 0, 1, 2), 0x252A: (1, 0, 2, 2), 0x252B: (2, 0, 2, 2),
    0x252C: (0, 1, 1, 1), 0x252D: (0, 1, 1, 2), 0x252E: (0, 2, 1, 1), 0x252F: (0, 2, 1, 2),
    0x2530: (0, 1, 2, 1), 0x2531: (0, 1, 2, 2), 0x2532: (0, 2, 2, 1), 0x2533: (0, 2, 2, 2),
    0x2534: (1, 1, 0, 1), 0x2535: (1, 1, 0, 2), 0x2536: (1, 2, 0, 1), 0x2537: (1, 2, 0, 2),
    0x2538: (2, 1, 0, 1), 0x2539: (2, 1, 0, 2), 0x253A: (2, 2, 0, 1), 0x253B: (2, 2, 0, 2),
    0x253C: (1, 1, 1, 1), 0x253D: (1, 1, 1, 2), 0x253E: (1, 2, 1, 1), 0x253F: (1, 2, 1, 2),
    0x2540: (2, 1, 1, 1), 0x2541: (1, 1, 2, 1), 0x2542: (2, 1, 2, 1), 0x2543: (2, 1, 1, 2),
    0x2544: (2, 2, 1, 1), 0x2545: (1, 1, 2, 2), 0x2546: (1, 2, 2, 1), 0x2547: (2, 2, 1, 2),
    0x2548: (1, 2, 2, 2), 0x2549: (2, 1, 2, 2), 0x254A: (2, 2, 2, 1), 0x254B: (2, 2, 2, 2),
    0x2550: (0, 3, 0, 3), 0x2551: (3, 0, 3, 0), 0x2552: (0, 3, 1, 0), 0x2553: (0, 1, 3, 0),
    0x2554: (0, 3, 3, 0), 0x2555: (0, 0, 1, 3), 0x2556: (0, 0, 3, 1), 0x2557: (0, 0, 3, 3),
    0x2558: (1, 3, 0, 0), 0x2559: (3, 1, 0, 0), 0x255A: (3, 3, 0, 0), 0x255B: (1, 0, 0, 3),
    0x255C: (3, 0, 0, 1), 0x255D: (3, 0, 0, 3), 0x255E: (1, 3, 1, 0), 0x255F: (3, 1, 3, 0),
    0x2560: (3, 3, 3, 0), 0x2561: (1, 0, 1, 3), 0x2562: (3, 0, 3, 1), 0x2563: (3, 0, 3, 3),
    0x2564: (0, 3, 1, 3), 0x2565: (0, 1, 3, 1), 0x2566: (0, 3, 3, 3), 0x2567: (1, 3, 0, 3),
    0x2568: (3, 1, 0, 1), 0x2569: (3, 3, 0, 3), 0x256A: (1, 3, 1, 3), 0x256B: (3, 1, 3, 1),
    0x256C: (3, 3, 3, 3),
    # Rounded corners, which at this size are the corners.
    0x256D: (0, 1, 1, 0), 0x256E: (0, 0, 1, 1), 0x256F: (1, 0, 0, 1), 0x2570: (1, 1, 0, 0),
    0x2574: (0, 0, 0, 1), 0x2575: (1, 0, 0, 0), 0x2576: (0, 1, 0, 0), 0x2577: (0, 0, 1, 0),
    0x2578: (0, 0, 0, 2), 0x2579: (2, 0, 0, 0), 0x257A: (0, 2, 0, 0), 0x257B: (0, 0, 2, 0),
    0x257C: (0, 2, 0, 1), 0x257D: (1, 0, 2, 0), 0x257E: (0, 1, 0, 2), 0x257F: (2, 0, 1, 0),
}

# Dashed lines: the solid one, and which pixels of it to keep along its length.
DASHED = {
    0x2504: (0x2500, 0b11011011), 0x2505: (0x2501, 0b11011011), 0x2506: (0x2502, 0b11011011), 0x2507: (0x2503, 0b11011011),
    0x2508: (0x2500, 0b10101010), 0x2509: (0x2501, 0b10101010), 0x250A: (0x2502, 0b10101010), 0x250B: (0x2503, 0b10101010),
    0x254C: (0x2500, 0b11001100), 0x254D: (0x2501, 0b11001100), 0x254E: (0x2502, 0b11001100), 0x254F: (0x2503, 0b11001100),
}


def box_glyph(arms):
    """Rows of a box character, leftmost pixel in bit 7, as PSF has them."""
    up, right, down, left = arms
    grid = [[False] * 8 for _ in range(8)]

    def across(row, first, last):
        for x in range(first, last + 1):
            grid[row][x] = True

    def along(column, first, last):
        for y in range(first, last + 1):
            grid[y][column] = True

    # Light is the centre line, heavy is two wide, and double is two lines a pixel apart.
    def horizontal(weight, first, last):
        for row in {1: (3,), 2: (3, 4)}.get(weight, ()):
            across(row, first, last)

    def vertical(weight, first, last):
        for column in {1: (3,), 2: (3, 4)}.get(weight, ()):
            along(column, first, last)

    horizontal(right, 3, 7)
    horizontal(left, 0, 4 if left == 2 else 3)
    vertical(up, 0, 4 if up == 2 else 3)
    vertical(down, 3, 7)
    # A double line turns inside the other: its inner line stops at the inner corner.
    if right == 3:
        across(2, 4 if up == 3 else 2, 7)
        across(4, 4 if down == 3 else 2, 7)
    if left == 3:
        across(2, 0, 2 if up == 3 else 4)
        across(4, 0, 2 if down == 3 else 4)
    if up == 3:
        along(2, 0, 2 if left == 3 else 4)
        along(4, 0, 2 if right == 3 else 4)
    if down == 3:
        along(2, 4 if left == 3 else 2, 7)
        along(4, 4 if right == 3 else 2, 7)
    return bytes(sum(0x80 >> x for x in range(8) if grid[y][x]) for y in range(8))


def dashed_glyph(solid, keep):
    rows = list(box_glyph(BOX[solid]))
    if solid in (0x2500, 0x2501):
        return bytes(row & keep for row in rows)
    return bytes(row if keep >> (7 - index) & 1 else 0 for index, row in enumerate(rows))


def block_glyphs():
    """Block elements, which are all rectangles and quadrants of the cell, and shades."""
    def rect(x0, x1, y0, y1):
        return bytes(sum(0x80 >> x for x in range(x0, x1)) if y0 <= y < y1 else 0 for y in range(8))

    def join(*parts):
        return bytes(a | b | c for a, b, c in zip(*(list(parts) + [bytes(8)] * 2)))

    out = {0x2580: rect(0, 8, 0, 4), 0x2590: rect(4, 8, 0, 8), 0x2594: rect(0, 8, 0, 1), 0x2595: rect(7, 8, 0, 8)}
    for eighths in range(1, 9):
        out[0x2580 + eighths] = rect(0, 8, 8 - eighths, 8)
    for index, eighths in enumerate(range(7, 0, -1)):
        out[0x2589 + index] = rect(0, eighths, 0, 8)
    out[0x2591] = bytes(0x88 if y % 2 == 0 else 0x22 for y in range(8))
    out[0x2592] = bytes(0xAA if y % 2 == 0 else 0x55 for y in range(8))
    out[0x2593] = bytes(0xEE if y % 2 == 0 else 0xBB for y in range(8))
    quadrants = {"a": rect(0, 4, 0, 4), "b": rect(4, 8, 0, 4), "c": rect(0, 4, 4, 8), "d": rect(4, 8, 4, 8)}
    for code, parts in {0x2596: "c", 0x2597: "d", 0x2598: "a", 0x2599: "acd", 0x259A: "ad", 0x259B: "abc",
                        0x259C: "abd", 0x259D: "b", 0x259E: "bc", 0x259F: "bcd"}.items():
        out[code] = bytes(a | b | c for a, b, c in zip(*[quadrants[p] for p in parts], *[bytes(8)] * (3 - len(parts))))
    return out


def art(rows):
    return bytes(int("".join("1" if ch in "#X" else "0" for ch in row), 2) for row in rows)


def font8x8_letter(letter):
    """An ASCII letter from font.rs, which is what's on the screen for it, as PSF rows."""
    code = ord(letter)
    with open("libs/gfx/src/font.rs") as source:
        for line in source:
            if line.rstrip().endswith(f"// U+{code:04X}"):
                rows = [int(value, 16) for value in line[line.index("[") + 1:line.index("]")].split(",")]
                return bytes(reverse(row) for row in rows)
    sys.exit(f"no {letter!r} in font.rs")


def add_drawn(merged):
    """Everything that isn't taken from the console fonts. Overrides what they said."""
    for code in list(merged):
        if 0x0370 <= code <= 0x03FF or 0x2500 <= code <= 0x259F:
            del merged[code]
    for code, letter in GREEK_AS_LATIN.items():
        merged[code] = font8x8_letter(letter)
    for code, rows in GREEK_DRAWN.items():
        merged[code] = art(rows)
    for code, base in GREEK_SMALL_ACCENTED.items():
        merged[code] = art(TONOS) + merged[base][2:]
    for code, (base, accent) in GREEK_SMALL_DIAERESIS.items():
        merged[code] = art(("........", accent)) + merged[base][2:]
    for code, (base, accent) in GREEK_CAPITAL_ACCENTED.items():
        merged[code] = art((accent,)) + merged[base][:7]
    for code, arms in BOX.items():
        merged[code] = box_glyph(arms)
    for code, (solid, keep) in DASHED.items():
        merged[code] = dashed_glyph(solid, keep)
    merged[0x2571] = bytes(0x80 >> (7 - y) for y in range(8))
    merged[0x2572] = bytes(0x80 >> y for y in range(8))
    merged[0x2573] = bytes((0x80 >> (7 - y)) | (0x80 >> y) for y in range(8))
    merged.update(block_glyphs())


# What those fonts lack of the blocks promised. Each takes its top two rows, the accent,
# from one glyph and the rest from another that has the same shape under its own accent.
COMPOSED = {
    0x0114: (0x0102, 0x0116),  # E breve: A breve's accent, E dot's body
    0x0115: (0x0103, 0x0117),
    0x012C: (0x0102, 0x0130),  # I breve
    0x012D: (0x0103, 0x0131),
    0x014E: (0x0102, 0x00D6),  # O breve
    0x014F: (0x0103, 0x00F6),
    0x0176: (0x00C2, 0x0178),  # Y circumflex
    0x0177: (0x00E2, 0x00FF),
    0x0175: (0x00E2, 0x0077),  # w circumflex, over a plain w, which starts low enough
    0x0400: (0x00C8, 0x00C8),  # Cyrillic E grave looks like Latin's
    0x0450: (0x00E8, 0x00E8),
    0x040D: (0x00C0, 0x0419),  # Cyrillic I grave: A grave's accent, short I's body
    0x045D: (0x00E0, 0x0439),
}

# Drawn for this project, for the rest of them.
DRAWN = {
    0x0132: ["XXX..XXX", ".X....X.", ".X....X.", ".X....X.", ".X.X..X.", ".X.X..X.", "XXX.XX..", "........"],
    0x0133: [".X...X..", "........", "XX..XX..", ".X...X..", ".X...X..", ".X...X..", "XXX..X..", "....XX.."],
    0x013F: ["XXXX....", ".XX.....", ".XX..XX.", ".XX..XX.", ".XX.....", ".XX...XX", "XXXXXXXX", "........"],
    0x0140: ["XXX.....", ".XX.....", ".XX..XX.", ".XX..XX.", ".XX.....", ".XX.....", "XXXX....", "........"],
    0x0149: ["XX......", "XX......", "X.XXXX..", "..XX.XX.", "..XX.XX.", "..XX.XX.", "..XX.XX.", "........"],
    0x0174: ["..XX....", ".X..X...", "XX...XX.", "XX...XX.", "XX.X.XX.", "XXXXXXX.", "XXX.XXX.", "XX...XX."],
    0x017F: ["...XXX..", "..XX.XX.", "..XX....", "..XX....", "..XX....", "..XX....", ".XXXX...", "........"],
}


def read_psf(path):
    data = gzip.open(path).read()
    if data[:2] != b"\x36\x04":
        sys.exit(f"{path}: not PSF1")
    mode, height = data[2], data[3]
    if height != 8:
        sys.exit(f"{path}: {height} rows")
    count = 512 if mode & 1 else 256
    bitmaps = data[4:4 + count * 8]
    table = data[4 + count * 8:]
    glyphs = {}
    glyph = 0
    at = 0
    in_sequence = False
    while at + 1 < len(table) and glyph < count:
        value = struct.unpack_from("<H", table, at)[0]
        at += 2
        if value == 0xFFFF:
            glyph += 1
            in_sequence = False
        elif value == 0xFFFE:
            in_sequence = True
        elif not in_sequence:
            glyphs.setdefault(value, bitmaps[glyph * 8:(glyph + 1) * 8])
    return glyphs


def reverse(byte):
    # PSF has the leftmost pixel in bit 7, and font8x8 in bit 0.
    return int(f"{byte:08b}"[::-1], 2)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    merged = {}
    for name in SOURCES:
        for code, bitmap in read_psf(os.path.join(sys.argv[1], name + ".psf.gz")).items():
            if any(low <= code <= high for low, high, _ in RANGES):
                merged.setdefault(code, bitmap)
    # Letters of plain ASCII, for what's composed over them.
    ascii_w = read_psf(os.path.join(sys.argv[1], SOURCES[0] + ".psf.gz"))[0x77]
    for code, (accent, body) in COMPOSED.items():
        if code not in merged:
            lower = ascii_w if body == 0x77 else merged[body]
            merged[code] = merged[accent][:2] + lower[2:]
    for code, rows in DRAWN.items():
        if code not in merged:
            merged[code] = bytes(int(row.replace("X", "1").replace(".", "0"), 2) for row in rows)
    add_drawn(merged)
    out = ["//! Generated by scripts/gen-font.py. Don't edit: see LICENSING.md for where it's from.", ""]
    out.append("/// `(code point, rows)`, sorted by code point. Bit 0 of a row is its leftmost pixel.")
    out.append("#[rustfmt::skip]")
    out.append(f"pub(crate) static GLYPHS: [(u16, [u8; 8]); {len(merged)}] = [")
    for code in sorted(merged):
        rows = ", ".join(f"0x{reverse(byte):02x}" for byte in merged[code])
        out.append(f"    (0x{code:04x}, [{rows}]),")
    out.append("];")
    with open("libs/gfx/src/font_extended.rs", "w") as target:
        target.write("\n".join(out) + "\n")
    for low, high, label in RANGES:
        have = sum(1 for code in merged if low <= code <= high)
        print(f"{label}: {have} of {high - low + 1}")
    print(f"{len(merged)} glyphs, {len(merged) * 10} bytes")


if __name__ == "__main__":
    main()
