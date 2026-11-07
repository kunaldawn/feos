#!/usr/bin/env python3
"""Regenerates the conformance fixtures with FFmpeg and libx264.

Each clip is a tiny Annex-B stream. FFmpeg's decode of it, in I420, is hashed
into conformance.txt, so the raw frames need not be stored. FFmpeg's H.264
decoder is bit-exact, so tests/conformance.rs compares for equality.

Run from this directory: python3 generate.py
"""
import subprocess

# name, size, frames, pix_fmt, x264-params, extra encoder args
CLIPS = [
    ("baseline_cavlc", "64x48", 12, "yuv420p", "cabac=0:bframes=0:ref=2", ["-profile:v", "baseline"]),
    ("main_cabac_b", "64x48", 24, "yuv420p", "bframes=3:b-adapt=0:b-pyramid=none:ref=3:weightb=1", ["-profile:v", "main"]),
    ("main_cabac_pyramid", "64x48", 24, "yuv420p", "bframes=3:b-adapt=0:b-pyramid=normal:ref=4:weightp=2", ["-profile:v", "main"]),
    ("main_cavlc_b", "64x48", 24, "yuv420p", "cabac=0:bframes=2:b-adapt=0:ref=3:direct=temporal", ["-profile:v", "main"]),
    ("main_spatial_direct", "64x48", 24, "yuv420p", "bframes=2:b-adapt=0:ref=2:direct=spatial", ["-profile:v", "main"]),
    ("high_8x8dct", "64x48", 16, "yuv420p", "bframes=2:8x8dct=1:ref=2", ["-profile:v", "high"]),
    ("high_cqm_jvt", "64x48", 12, "yuv420p", "bframes=1:8x8dct=1:cqm=jvt", ["-profile:v", "high"]),
    ("high_chroma_offset", "64x48", 12, "yuv420p", "bframes=1:8x8dct=1:chroma-qp-offset=4", ["-profile:v", "high"]),
    ("multi_slice", "64x64", 12, "yuv420p", "slices=3:bframes=1:ref=2", []),
    ("multi_slice_cavlc", "64x64", 12, "yuv420p", "slices=3:cabac=0:bframes=0", []),
    ("sliced_deblock_idc2", "64x64", 12, "yuv420p", "sliced-threads=1:threads=3:slices=3:bframes=0", []),
    ("odd_size_crop", "34x30", 8, "yuv420p", "bframes=1", []),
    ("crop_rect_left_top", "64x64", 8, "yuv420p", "bframes=1:crop-rect=4,8,6,20", []),
    ("intra_only", "64x48", 6, "yuv420p", "keyint=1:bframes=0", []),
    ("idr_every_4", "64x48", 16, "yuv420p", "keyint=4:min-keyint=4:bframes=2:scenecut=0", []),
    ("open_gop", "64x48", 24, "yuv420p", "keyint=8:min-keyint=8:bframes=2:open-gop=1:scenecut=0", []),
    ("high_cavlc_8x8dct", "64x64", 12, "yuv420p", "cabac=0:bframes=0:8x8dct=1", []),
    ("high_cavlc_intra_8x8", "64x48", 6, "yuv420p", "cabac=0:keyint=1:bframes=0:8x8dct=1", []),
]

# Compared in their own bit depth and chroma format, in native.txt.
# name, size, frames, pix_fmt, x264-params, extra encoder args
NATIVE = [
    ("hi10_cabac", "64x48", 16, "yuv420p10le", "bframes=2:ref=3:weightp=2:weightb=1", ["-profile:v", "high10"]),
    ("hi10_cavlc", "64x48", 16, "yuv420p10le", "cabac=0:bframes=2:ref=2", ["-profile:v", "high10"]),
    ("hi10_intra", "64x48", 2, "yuv420p10le", "keyint=1:bframes=0", ["-profile:v", "high10"]),
    ("hi422_cabac", "64x48", 16, "yuv422p", "bframes=2:ref=3:weightp=2:weightb=1", ["-profile:v", "high422"]),
    ("hi422_cavlc", "64x48", 16, "yuv422p", "cabac=0:bframes=2:ref=2:no-8x8dct=1", ["-profile:v", "high422"]),
    ("hi422_intra", "64x48", 2, "yuv422p", "keyint=1:bframes=0", ["-profile:v", "high422"]),
    ("hi42210_cabac", "64x48", 16, "yuv422p10le", "bframes=2:ref=3:direct=temporal", ["-profile:v", "high422"]),
    ("hi42210_cavlc", "64x48", 16, "yuv422p10le", "cabac=0:bframes=2:ref=2", ["-profile:v", "high422"]),
    ("mbaff_cabac", "64x64", 16, "yuv420p", "interlaced=1:bframes=2:ref=3:direct=spatial", []),
    ("mbaff_temporal", "64x64", 16, "yuv420p", "interlaced=1:bframes=2:ref=3:direct=temporal", []),
    ("mbaff_cavlc", "64x64", 16, "yuv420p", "interlaced=1:cabac=0:bframes=2:ref=2", []),
    ("mbaff_hi422", "64x64", 16, "yuv422p10le", "interlaced=1:bframes=2:ref=3", ["-profile:v", "high422"]),
]

# Streams the decoder must refuse rather than decode to garbage.
UNSUPPORTED = [
    ("high444", "yuv444p", "bframes=0", ["-profile:v", "high444"]),
]


class Bits:
    """An H.264 RBSP, for what no encoder here will make."""

    def __init__(self):
        self.bits = []

    def u(self, count, value):
        self.bits += [(value >> (count - 1 - i)) & 1 for i in range(count)]

    def ue(self, value):
        size = (value + 1).bit_length()
        self.u(2 * size - 1, value + 1)

    def se(self, value):
        self.ue(2 * value - 1 if value > 0 else -2 * value)

    def align(self):
        while len(self.bits) % 8:
            self.bits.append(0)

    def nal(self, header):
        """The NAL unit with its start code, trailing bits and emulation prevention."""
        bits = self.bits + [1]
        bits += [0] * (-len(bits) % 8)
        out = bytearray([0, 0, 0, 1, header])
        zeros = 0
        for at in range(0, len(bits), 8):
            byte = int("".join(map(str, bits[at:at + 8])), 2)
            if zeros >= 2 and byte <= 3:
                out.append(3)
                zeros = 0
            out.append(byte)
            zeros = zeros + 1 if byte == 0 else 0
        return bytes(out)


def paff_stream():
    """Field pictures by hand, as x264 won't make them: I_PCM top fields of a moving picture, and
    P fields that skip, two frames of each in CAVLC, with frame_num and POC going on. All of it is
    small enough to reason about: a skipped field copies the first field in its list: the top
    field just decoded in the first frame, and the same parity's in the second."""
    width_mbs, field_height_mbs = 2, 1
    sps = Bits()
    sps.u(8, 77)  # Main
    sps.u(8, 0)
    sps.u(8, 30)
    sps.ue(0)  # sps id
    sps.ue(0)  # log2_max_frame_num - 4
    sps.ue(0)  # POC type 0
    sps.ue(2)  # log2_max_poc_lsb - 4
    sps.ue(2)  # references
    sps.u(1, 0)
    sps.ue(width_mbs - 1)
    sps.ue(field_height_mbs - 1)
    sps.u(1, 0)  # frame_mbs_only
    sps.u(1, 0)  # no MBAFF
    sps.u(1, 1)  # direct_8x8_inference
    sps.u(1, 0)  # no crop
    sps.u(1, 0)  # no VUI
    pps = Bits()
    pps.ue(0)
    pps.ue(0)
    pps.u(1, 0)  # CAVLC
    pps.u(1, 0)
    pps.ue(0)
    pps.ue(0)
    pps.ue(0)
    pps.u(1, 0)
    pps.u(2, 0)
    pps.se(0)
    pps.se(0)
    pps.se(0)
    pps.u(1, 0)  # no deblocking control: filtered, with I_PCM's QP of 0, which does nothing
    pps.u(1, 0)
    pps.u(1, 0)
    out = sps.nal(0x67) + pps.nal(0x68)

    def header(first_type, frame_num, bottom, poc, idr):
        s = Bits()
        s.ue(0)
        s.ue(first_type)
        s.ue(0)
        s.u(4, frame_num)
        s.u(1, 1)  # field_pic_flag
        s.u(1, bottom)
        if idr:
            s.ue(0)
        s.u(6, poc)
        return s

    for frame in range(2):
        s = header(7, frame, 0, frame * 4, frame == 0)
        if frame == 0:
            s.u(1, 0)
            s.u(1, 0)
        else:
            s.u(1, 0)  # sliding window
        s.se(0)
        for mb in range(width_mbs * field_height_mbs):
            s.ue(25)  # I_PCM
            s.align()
            for y in range(16):
                for x in range(16):
                    s.u(8, (frame * 40 + mb * 16 + x * 3 + y * 5) & 255)
            for plane in range(2):
                for y in range(8):
                    for x in range(8):
                        s.u(8, (64 + plane * 90 + frame * 20 + x * 4 + y * 2) & 255)
        out += s.nal(0x65 if frame == 0 else 0x41)
        s = header(5, frame, 1, frame * 4 + 1, False)
        s.u(1, 0)  # no override of the one reference
        s.u(1, 0)  # no list modification
        s.u(1, 0)  # sliding window
        s.se(0)
        s.ue(width_mbs * field_height_mbs)  # all skipped
        out += s.nal(0x41)
    return out


def fnv64(data):
    value = 0xcbf29ce484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    return value


def native_table(name):
    """Dimensions, frames and hash of FFmpeg's decode of a clip, in its own format."""
    path = name + ".h264"
    raw = subprocess.run(
        ["ffmpeg", "-loglevel", "error", "-threads", "1", "-i", path, "-f", "rawvideo", "-"],
        check=True, capture_output=True).stdout
    probe = subprocess.run(
        ["ffprobe", "-loglevel", "error", "-select_streams", "v", "-show_entries",
         "stream=width,height", "-of", "csv=p=0", path],
        check=True, capture_output=True, text=True).stdout.strip().split(",")
    return f"{name} {probe[0]} {probe[1]} {fnv64(raw):016x}"


def encode(name, size, frames, pix_fmt, params, extra):
    subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error", "-f", "lavfi", "-i",
         f"testsrc2=size={size}:rate=10:duration=10", "-frames:v", str(frames),
         "-pix_fmt", pix_fmt, "-c:v", "libx264", "-x264-params", params + ":log=-1",
         *extra, "-bsf:v", "filter_units=remove_types=6", "-f", "h264", name],
        check=True)


lines = []
for name, size, frames, pix_fmt, params, extra in CLIPS:
    path = name + ".h264"
    encode(path, size, frames, pix_fmt, params, extra)
    yuv = subprocess.run(
        ["ffmpeg", "-loglevel", "error", "-i", path, "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"],
        check=True, capture_output=True).stdout
    w, h = map(int, subprocess.run(
        ["ffprobe", "-loglevel", "error", "-select_streams", "v", "-show_entries",
         "stream=width,height", "-of", "csv=p=0", path],
        check=True, capture_output=True, text=True).stdout.strip().split(","))
    count = len(yuv) // (w * h + 2 * (w // 2) * (h // 2))
    lines.append(f"{name} {w} {h} {count} {fnv64(yuv):016x}")
for name, pix_fmt, params, extra in UNSUPPORTED:
    encode("unsupported_" + name + ".h264", "32x32", 1, pix_fmt, params, extra)
with open("conformance.txt", "w") as out:
    out.write("\n".join(lines) + "\n")

native = []
for name, size, frames, pix_fmt, params, extra in NATIVE:
    encode(name + ".h264", size, frames, pix_fmt, params, extra)
    native.append(native_table(name))
with open("paff_pcm.h264", "wb") as out:
    out.write(paff_stream())
native.append(native_table("paff_pcm"))
with open("native.txt", "w") as out:
    out.write("\n".join(native) + "\n")
