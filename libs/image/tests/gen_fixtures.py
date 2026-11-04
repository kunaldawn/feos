#!/usr/bin/env python3
"""Regenerates the feature fixtures in tests/data. See tests/data/README.md.

Needs python3 with Pillow, and ffmpeg. The files in tests/data are committed, so
running this is only needed to add or change a fixture.

The small JPEG encoder here exists for streams no tool will write: unusual
sampling factors, separate scans, YCCK. It emits exactly the same coefficients
whatever the scan layout, which lets the tests demand identical pixels between
the allocation-free path and the coefficient-buffer path.
"""
import io
import math
import os
import struct
import subprocess
import tempfile
import zlib

from PIL import Image

DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")
W, H = 37, 29


def write(name, data):
    with open(os.path.join(DATA, name), "wb") as f:
        f.write(data)


def source(x, y):
    """A smooth test picture with one hard edge, deliberately not MCU sized."""
    r = 40 + x * 180 // (W - 1)
    g = 220 - y * 170 // (H - 1)
    b = 60 + (x + y) * 120 // (W + H - 2)
    if 10 <= x < 20 and 8 <= y < 18:
        r, g, b = 235, 40, 30
    return r, g, b


def source_image():
    im = Image.new("RGB", (W, H))
    im.putdata([source(x, y) for y in range(H) for x in range(W)])
    return im


def to_ycc(r, g, b):
    y = 0.299 * r + 0.587 * g + 0.114 * b
    cb = -0.168736 * r - 0.331264 * g + 0.5 * b + 128
    cr = 0.5 * r - 0.418688 * g - 0.081312 * b + 128
    return y, cb, cr


# ---------------------------------------------------------------- JPEG encoder

ZIGZAG = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48,
    41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22,
    15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55,
    62, 63,
]
COS = [[math.cos((2 * x + 1) * u * math.pi / 16) for x in range(8)] for u in range(8)]
QUANT = [[3 + u + v for u in range(8)] for v in range(8)]


def standard_tables():
    """Reads T.81 K.3 tables back out of a default libjpeg encode."""
    out = io.BytesIO()
    source_image().save(out, "JPEG", quality=75, optimize=False)
    d = out.getvalue()
    tables, i = {}, 2
    while d[i + 1] != 0xDA:
        length = struct.unpack(">H", d[i + 2:i + 4])[0]
        if d[i + 1] == 0xC4:
            seg, j = d[i + 4 : i + 2 + length], 0
            while j < len(seg):
                n = sum(seg[j + 1 : j + 17])
                tables[seg[j]] = (seg[j + 1 : j + 17], seg[j + 17 : j + 17 + n])
                j += 17 + n
        i += 2 + length
    return tables


TABLES = standard_tables()


def codes(table):
    counts, symbols = table
    result, code, k = {}, 0, 0
    for length in range(1, 17):
        for _ in range(counts[length - 1]):
            result[symbols[k]] = (code, length)
            code += 1
            k += 1
        code <<= 1
    return result


class Bits:
    def __init__(self):
        self.out = bytearray()
        self.acc = 0
        self.n = 0

    def put(self, value, length):
        for i in range(length - 1, -1, -1):
            self.acc = (self.acc << 1) | ((value >> i) & 1)
            self.n += 1
            if self.n == 8:
                self.out.append(self.acc)
                if self.acc == 0xFF:
                    self.out.append(0)
                self.acc = self.n = 0

    def flush(self):
        while self.n:
            self.put(1, 1)


def category(v):
    return 0 if v == 0 else abs(v).bit_length()


def put_value(bits, v, size):
    if size:
        bits.put(v if v > 0 else v + (1 << size) - 1, size)


def put_block(bits, zz, pred, dc, ac):
    diff = zz[0] - pred
    size = category(diff)
    bits.put(*dc[size])
    put_value(bits, diff, size)
    run = 0
    for v in zz[1:]:
        if v == 0:
            run += 1
            continue
        while run > 15:
            bits.put(*ac[0xF0])
            run -= 16
        size = category(v)
        bits.put(*ac[(run << 4) | size])
        put_value(bits, v, size)
        run = 0
    if run:
        bits.put(*ac[0])
    return zz[0]


def fdct(samples):
    out = [0] * 64
    for v in range(8):
        for u in range(8):
            total = 0.0
            for y in range(8):
                for x in range(8):
                    total += (samples[y * 8 + x] - 128) * COS[u][x] * COS[v][y]
            cu = 1 / math.sqrt(2) if u == 0 else 1
            cv = 1 / math.sqrt(2) if v == 0 else 1
            out[v * 8 + u] = round(total * cu * cv / 4 / QUANT[v][u])
    return [out[ZIGZAG[k]] for k in range(64)]


def segment(code, payload):
    return bytes([0xFF, code]) + struct.pack(">H", len(payload) + 2) + payload


def encode(components, scans, *, app=b"", dht=True, restart=0, ids=None):
    """components: list of (h, v, plane(x, y) -> sample). scans: lists of indices."""
    hmax = max(c[0] for c in components)
    vmax = max(c[1] for c in components)
    mcus_x = math.ceil(W / (hmax * 8))
    mcus_y = math.ceil(H / (vmax * 8))
    ids = ids or list(range(1, len(components) + 1))

    grids = []
    for h, v, plane in components:
        # Box-filter down to the component's resolution, then replicate the edge.
        w_s, h_s = math.ceil(W * h / hmax), math.ceil(H * v / vmax)
        rows = []
        for sy in range(h_s):
            row = []
            for sx in range(w_s):
                total = count = 0
                for y in range(sy * vmax // v, min(H, (sy + 1) * vmax // v)):
                    for x in range(sx * hmax // h, min(W, (sx + 1) * hmax // h)):
                        total += plane(x, y)
                        count += 1
                row.append(total / count if count else 128)
            rows.append(row)
        grid = {}
        for by in range(mcus_y * v):
            for bx in range(mcus_x * h):
                samples = []
                for y in range(8):
                    src = rows[min(by * 8 + y, h_s - 1)]
                    for x in range(8):
                        samples.append(src[min(bx * 8 + x, w_s - 1)])
                grid[bx, by] = fdct(samples)
        grids.append((grid, w_s, h_s))

    out = bytearray(b"\xff\xd8") + app
    out += segment(0xDB, bytes([0]) + bytes(QUANT[ZIGZAG[k] // 8][ZIGZAG[k] % 8] for k in range(64)))
    sof = bytes([8]) + struct.pack(">HH", H, W) + bytes([len(components)])
    for i, (h, v, _) in enumerate(components):
        sof += bytes([ids[i], (h << 4) | v, 0])
    out += segment(0xC0, sof)
    if dht:
        for key, (counts, symbols) in TABLES.items():
            out += segment(0xC4, bytes([key]) + counts + symbols)
    if restart:
        out += segment(0xDD, struct.pack(">H", restart))

    dc, ac = codes(TABLES[0x00]), codes(TABLES[0x10])
    for scan in scans:
        header = bytes([len(scan)])
        for i in scan:
            header += bytes([ids[i], 0])
        out += segment(0xDA, header + bytes([0, 63, 0]))
        if len(scan) == 1:
            grid, w_s, h_s = grids[scan[0]]
            units = [
                [(scan[0], x, y)]
                for y in range(math.ceil(h_s / 8))
                for x in range(math.ceil(w_s / 8))
            ]
        else:
            units = []
            for my in range(mcus_y):
                for mx in range(mcus_x):
                    unit = []
                    for i in scan:
                        h, v, _ = components[i]
                        for by in range(v):
                            for bx in range(h):
                                unit.append((i, mx * h + bx, my * v + by))
                    units.append(unit)
        bits = Bits()
        preds = {}
        rst = 0
        for n, unit in enumerate(units):
            if restart and n and n % restart == 0:
                bits.flush()
                bits.out += bytes([0xFF, 0xD0 + rst])
                rst = (rst + 1) % 8
                preds = {}
            for i, x, y in unit:
                preds[i] = put_block(bits, grids[i][0][x, y], preds.get(i, 0), dc, ac)
        bits.flush()
        out += bits.out
    out += b"\xff\xd9"
    return bytes(out)


def plane(channel):
    return lambda x, y: to_ycc(*source(x, y))[channel]


def luma_gray(x, y):
    return to_ycc(*source(x, y))[0]


YCC420 = [(2, 2, plane(0)), (1, 1, plane(1)), (1, 1, plane(2))]
JFIF = segment(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0")


def adobe(transform):
    return segment(0xEE, b"Adobe\0\x64\0\0\0\0" + bytes([transform]))


def strip_dht(data):
    out, i = bytearray(data[:2]), 2
    while data[i + 1] != 0xDA:
        length = struct.unpack(">H", data[i + 2 : i + 4])[0]
        if data[i + 1] != 0xC4:
            out += data[i : i + 2 + length]
        i += 2 + length
    return bytes(out + data[i:])


def pil_jpeg(image, **options):
    out = io.BytesIO()
    image.save(out, "JPEG", **options)
    return out.getvalue()


def reference(data):
    """libjpeg's own RGB decode, for fixtures with no upsampling involved."""
    return Image.open(io.BytesIO(data)).convert("RGB").tobytes()


def jpegs():
    gray = [(1, 1, luma_gray)]
    write("gray_h1v1.jpg", encode(gray, [[0]], app=JFIF))
    write("gray_h2v2.jpg", encode([(2, 2, luma_gray)], [[0]], app=JFIF))

    write("interleaved_420.jpg", encode(YCC420, [[0, 1, 2]], app=JFIF))
    write("restart_420.jpg", encode(YCC420, [[0, 1, 2]], app=JFIF, restart=2))
    write("multiscan_420.jpg", encode(YCC420, [[0], [1], [2]], app=JFIF))
    write("multiscan_restart_420.jpg", encode(YCC420, [[0], [1], [2]], app=JFIF, restart=3))
    write("multiscan_y_cbcr_420.jpg", encode(YCC420, [[0], [1, 2]], app=JFIF))
    write("reversed_scan_420.jpg", encode(YCC420, [[2, 1, 0]], app=JFIF))

    # h*v > 4 on one component: 10 blocks an MCU, and then 18, over the T.81 limit.
    write("sampling_h4v2.jpg", encode([(4, 2, plane(0)), (1, 1, plane(1)), (1, 1, plane(2))], [[0, 1, 2]], app=JFIF))
    write("sampling_h4v4.jpg", encode([(4, 4, plane(0)), (1, 1, plane(1)), (1, 1, plane(2))], [[0, 1, 2]], app=JFIF))
    # A 2:1 ratio written with the factors doubled, 12 blocks an MCU.
    write("sampling_doubled_444.jpg", encode([(2, 2, plane(0)), (2, 2, plane(1)), (2, 2, plane(2))], [[0, 1, 2]], app=JFIF))
    write("interleaved_444.jpg", encode([(1, 1, plane(0)), (1, 1, plane(1)), (1, 1, plane(2))], [[0, 1, 2]], app=JFIF))

    # Colour signalling.
    rgb = [(1, 1, lambda x, y, c=c: source(x, y)[c]) for c in range(3)]
    write("rgb_ids.jpg", encode(rgb, [[0, 1, 2]], ids=list(b"RGB")))
    data = pil_jpeg(source_image(), keep_rgb=True, subsampling=0, quality=90)
    write("adobe_rgb.jpg", data)
    write("adobe_rgb.rgb", reference(data))

    def inks(x, y):
        r, g, b = source(x, y)
        k = min(255 - r, 255 - g, 255 - b) // 2
        return 255 - r, 255 - g, 255 - b, k

    cmyk = Image.new("CMYK", (W, H))
    cmyk.putdata([inks(x, y) for y in range(H) for x in range(W)])
    data = pil_jpeg(cmyk, quality=95)
    write("cmyk.jpg", data)
    write("cmyk.rgb", reference(data))
    ycck = [(1, 1, lambda x, y, c=c: to_ycc(*inks(x, y)[:3])[c]) for c in range(3)]
    ycck.append((1, 1, lambda x, y: 255 - inks(x, y)[3]))
    data = encode(ycck, [[0, 1, 2, 3]], app=adobe(2))
    write("ycck.jpg", data)
    write("ycck.rgb", reference(data))

    # Standard Huffman tables, from Pillow and from ffmpeg's MJPEG encoder.
    data = pil_jpeg(source_image(), quality=75, optimize=False, subsampling=0)
    write("with_dht.jpg", data)
    write("no_dht.jpg", strip_dht(data))
    write("no_dht_ffmpeg.jpg", strip_dht(ffmpeg_mjpeg()))

    # Progressive, with libjpeg's default scan script (spectral selection and
    # successive approximation). Same settings as the baseline twin.
    for name, options in (
        ("420", dict(subsampling=2)),
        ("444", dict(subsampling=0)),
    ):
        write(f"baseline_{name}.jpg", pil_jpeg(source_image(), quality=80, **options))
        write(f"progressive_{name}.jpg", pil_jpeg(source_image(), quality=80, progressive=True, **options))
    write("baseline_444.rgb", reference(pil_jpeg(source_image(), quality=80, subsampling=0)))
    write(
        "progressive_restart_420.jpg",
        pil_jpeg(source_image(), quality=80, subsampling=2, progressive=True, restart_marker_blocks=2),
    )
    write("progressive_gray.jpg", pil_jpeg(source_image().convert("L"), quality=80, progressive=True))
    write("baseline_gray.jpg", pil_jpeg(source_image().convert("L"), quality=80))


def ffmpeg_mjpeg():
    with tempfile.TemporaryDirectory() as d:
        src = os.path.join(d, "s.png")
        dst = os.path.join(d, "o.jpg")
        source_image().save(src)
        subprocess.check_call(
            ["ffmpeg", "-v", "error", "-y", "-i", src, "-c:v", "mjpeg", "-q:v", "5",
             "-huffman", "0", "-pix_fmt", "yuvj420p", "-frames:v", "1", dst]
        )
        with open(dst, "rb") as f:
            return f.read()


# ------------------------------------------------------------------------ BMP

SMALL_W, SMALL_H = 8, 6
PALETTE = [(10, 20, 30), (200, 0, 0), (0, 200, 0), (0, 0, 200), (250, 250, 0), (0, 250, 250)]


def indices(x, y):
    """y counts from the top. The bottom row has a gap the RLE delta skips."""
    if y == 5:
        return 1 if x < 2 else 0 if x < 5 else 2
    return (x + y * 3) % 6 if y % 2 else 3


def bmp(dib, raster, palette=b""):
    offset = 14 + len(dib) + len(palette)
    return b"BM" + struct.pack("<IHHI", offset + len(raster), 0, 0, offset) + dib + palette + raster


def dib40(width, height, bpp, compression, size, colors=0):
    return struct.pack("<IiiHHIIiiII", 40, width, height, 1, bpp, compression, size, 2835, 2835, colors, 0)


def pal(colors=16):
    data = b"".join(bytes((b, g, r, 0)) for r, g, b in PALETTE)
    return data + bytes(4 * (colors - len(PALETTE)))


def rle8():
    # Rows run bottom-up, so the first command paints the bottom row (y == 5).
    out = bytearray([2, 1])  # run of index 1
    out += bytes([0, 2, 3, 0])  # delta over three background pixels
    out += bytes([3, 2, 0, 0])  # run of index 2, end of line
    for y in range(4, -1, -1):
        row = [indices(x, y) for x in range(SMALL_W)]
        if y % 2:
            # Literal of odd length, padded to 16 bits, then a run for the rest.
            out += bytes([0, 7]) + bytes(row[:7]) + b"\0" + bytes([1, row[7]])
        else:
            out += bytes([8, row[0]])
        out += bytes([0, 0])
    return bytes(out + bytes([0, 1]))


def rle4():
    out = bytearray([2, 0x11])
    out += bytes([0, 2, 3, 0])
    out += bytes([3, 0x22, 0, 0])
    for y in range(4, -1, -1):
        row = [indices(x, y) for x in range(SMALL_W)]
        if y % 2:
            # Five nibbles in three bytes, padded to 16 bits.
            lit = row[:5]
            out += bytes([0, 5, lit[0] << 4 | lit[1], lit[2] << 4 | lit[3], lit[4] << 4, 0])
            # Two-colour run for the rest, or three separate runs when unequal.
            for v in row[5:]:
                out += bytes([1, v << 4])
        else:
            out += bytes([8, row[0] << 4 | row[0]])
        out += bytes([0, 0])
    return bytes(out + bytes([0, 1]))


def color_at(x, y):
    return (x * 30 + 10, y * 40 + 5, 255 - x * 20)


def bmps():
    raw8 = bytearray()
    raw4 = bytearray()
    for y in range(SMALL_H - 1, -1, -1):
        row = [indices(x, y) for x in range(SMALL_W)]
        raw8 += bytes(row)
        raw4 += bytes(row[i] << 4 | row[i + 1] for i in range(0, SMALL_W, 2))
    write("pal8.bmp", bmp(dib40(SMALL_W, SMALL_H, 8, 0, len(raw8), 256), bytes(raw8), pal(256)))
    write("rle8.bmp", bmp(dib40(SMALL_W, SMALL_H, 8, 1, len(rle8()), 256), rle8(), pal(256)))
    write("pal4.bmp", bmp(dib40(SMALL_W, SMALL_H, 4, 0, len(raw4), 16), bytes(raw4), pal(16)))
    write("rle4.bmp", bmp(dib40(SMALL_W, SMALL_H, 4, 2, len(rle4()), 16), rle4(), pal(16)))

    rows = [[color_at(x, y) for x in range(SMALL_W)] for y in range(SMALL_H)]

    # 16-bit: default 5-5-5, and 5-6-5 through BI_BITFIELDS (bottom-up).
    def r16(shift_r, bits_g, shift_g, order):
        out = bytearray()
        for row in order:
            for r, g, b in row:
                value = (r >> 3) << shift_r | (g >> (8 - bits_g)) << shift_g | b >> 3
                out += struct.pack("<H", value)
        return bytes(out)

    bottom_up = rows[::-1]
    data = r16(10, 5, 5, bottom_up)
    write("rgb555.bmp", bmp(dib40(SMALL_W, SMALL_H, 16, 0, len(data)), data))
    data = r16(11, 6, 5, bottom_up)
    masks = struct.pack("<III", 0xF800, 0x07E0, 0x001F)
    write("bf565.bmp", bmp(dib40(SMALL_W, SMALL_H, 16, 3, len(data)) + masks, data))

    # 32-bit with the channels moved: R in the top byte and A in the low byte.
    def r32():
        out = bytearray()
        for y, row in zip(range(SMALL_H - 1, -1, -1), bottom_up):
            for x, (r, g, b) in enumerate(row):
                out += struct.pack("<I", r << 24 | g << 16 | b << 8 | (x * 36))
        return bytes(out)

    data = r32()
    masks3 = struct.pack("<III", 0xFF000000, 0x00FF0000, 0x0000FF00)
    # A V3 header (56 bytes) carries all four masks, including alpha.
    v3 = dib40(SMALL_W, SMALL_H, 32, 3, len(data))
    v3 = struct.pack("<I", 56) + v3[4:] + masks3 + struct.pack("<I", 0xFF)
    write("bf32_rgba.bmp", bmp(v3, data))
    # A 40-byte header with BI_BITFIELDS has three masks and no alpha.
    write("bf32_noalpha.bmp", bmp(dib40(SMALL_W, SMALL_H, 32, 3, len(data)) + masks3, data))
    # BI_ALPHABITFIELDS: four masks after a 40-byte header.
    write("bf32_alpha6.bmp", bmp(dib40(SMALL_W, SMALL_H, 32, 6, len(data)) + masks3 + struct.pack("<I", 0xFF), data))

    # Top-down 24-bit: negative height, rows padded to four bytes.
    raster = bytearray()
    for row in rows:
        for r, g, b in row:
            raster += bytes((b, g, r))
    write("topdown24.bmp", bmp(dib40(SMALL_W, -SMALL_H, 24, 0, len(raster)), bytes(raster)))


# ------------------------------------------------------------------------ PNG


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def png(width, height, depth, color, raw, extra=b"", interlace=0, idat=None):
    ihdr = struct.pack(">IIBBBBB", width, height, depth, color, 0, 0, interlace)
    body = idat if idat is not None else chunk(b"IDAT", zlib.compress(raw, 9))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + extra + body + chunk(b"IEND", b"")


def packed_gray(depth):
    width, height = 8, 4
    raw = bytearray()
    for y in range(height):
        raw.append(0)
        acc = n = 0
        for x in range(width):
            acc = acc << depth | (x + y) % (1 << depth)
            n += depth
            if n == 8:
                raw.append(acc)
                acc = n = 0
        if n:
            raw.append(acc << (8 - n))
    return png(width, height, depth, 0, bytes(raw))


def pngs():
    for depth in (1, 2, 4):
        write(f"gray{depth}.png", packed_gray(depth))

    # 16-bit RGB and RGBA, where the high byte is what must survive.
    w, h = 6, 5
    rgb16 = bytearray()
    rgba16 = bytearray()
    for y in range(h):
        rgb16.append(0)
        rgba16.append(0)
        for x in range(w):
            r, g, b = color_at(x, y)
            rgb16 += struct.pack(">HHH", r << 8 | 0x55, g << 8 | 0xAA, b << 8)
            rgba16 += struct.pack(">HHHH", r << 8 | 0x55, g << 8 | 0xAA, b << 8, (x * 40) << 8)
    write("rgb16.png", png(w, h, 16, 2, bytes(rgb16)))
    write("rgba16.png", png(w, h, 16, 6, bytes(rgba16)))

    # tRNS on gray and RGB: exactly one sample value turns transparent. The two
    # 16-bit colours differ only in the low byte, so the match must use all 16.
    write("trns_gray8.png", png(3, 1, 8, 0, bytes([0, 10, 11, 12]), chunk(b"tRNS", struct.pack(">H", 11))))
    a, b = (0x1234, 0x5678, 0x9ABC), (0x1234, 0x5678, 0x9ABD)
    raw = bytes([0]) + struct.pack(">6H", *a, *b)
    write("trns_rgb16.png", png(2, 1, 16, 2, raw, chunk(b"tRNS", struct.pack(">3H", *b))))

    # Adam7 at 16 bits; decodes to the same pixels as rgb16.png.
    passes = [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]
    raw = bytearray()
    for xs, ys, xd, yd in passes:
        for y in range(ys, h, yd):
            row = bytearray()
            for x in range(xs, w, xd):
                r, g, b = color_at(x, y)
                row += struct.pack(">HHH", r << 8 | 0x55, g << 8 | 0xAA, b << 8)
            if row:
                raw += b"\0" + row
    write("adam7_rgb16.png", png(w, h, 16, 2, bytes(raw), interlace=1))

    # IDAT split around another chunk.
    z = zlib.compress(bytes(rgb16), 9)
    split = chunk(b"IDAT", z[:9]) + chunk(b"tEXt", b"a\0b") + chunk(b"IDAT", z[9:])
    write("idat_split.png", png(w, h, 16, 2, b"", idat=split))

    # Valid zlib, wrong amount of image data. Both must be rejected.
    write("short_idat.png", png(w, h, 16, 2, bytes(rgb16[:-7])))
    write("long_idat.png", png(w, h, 16, 2, bytes(rgb16) + bytes(7)))


# ---------------------------------------------------------------- Netpbm, QOI


def pnm():
    w, h = 5, 3
    gray = [[(x * 60 + y * 5) for x in range(w)] for y in range(h)]
    color = [[color_at(x, y) for x in range(w)] for y in range(h)]
    bits = [[(x + y) % 2 for x in range(w)] for y in range(h)]

    text = "P1\n# a comment\n5 # width\n3\n"
    # P1 digits may run together.
    text += "\n".join("".join(str(v) for v in row) for row in bits) + "\n"
    write("p1.pbm", text.encode())
    data = b"P4\n# comment\n5 3\n"
    for row in bits:
        data += bytes([sum(v << (7 - i) for i, v in enumerate(row))])
    write("p4.pbm", data)

    text = "P2 # gray\n5 3\n# maxval next\n255\n"
    text += "\n".join(" ".join(str(v) for v in row) for row in gray) + "\n"
    write("p2.pgm", text.encode())
    write("p5.pgm", b"P5\n5 3\n255\n" + b"".join(bytes(row) for row in gray))

    text = "P3\n5 3\n255\n# pixels\n"
    text += "\n".join(" ".join(f"{r} {g} {b}" for r, g, b in row) for row in color) + "\n"
    write("p3.ppm", text.encode())
    write("p6.ppm", b"P6\n5 3\n255\n" + b"".join(bytes(c) for row in color for c in row))
    # Same picture at 16 bits, maxval 1023.
    data = b"P5\n5 3\n1023\n"
    for row in gray:
        for v in row:
            data += struct.pack(">H", v * 1023 // 255)
    write("p5_16.pgm", data)


def qoi():
    for name, src, fmt in (("rgb.qoi", "test.ppm", "rgb24"), ("rgba.qoi", "rgba.png", "rgba")):
        subprocess.check_call(
            ["ffmpeg", "-v", "error", "-y", "-i", os.path.join(DATA, src), "-c:v", "qoi",
             "-pix_fmt", fmt, "-frames:v", "1", "-f", "image2", "-update", "1", os.path.join(DATA, name)]
        )


if __name__ == "__main__":
    jpegs()
    bmps()
    pngs()
    pnm()
    qoi()
