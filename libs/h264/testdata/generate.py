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

# Streams the decoder must refuse rather than decode to garbage.
UNSUPPORTED = [
    ("high10", "yuv420p10le", "bframes=0", ["-profile:v", "high10"]),
    ("high422", "yuv422p", "bframes=0", ["-profile:v", "high422"]),
    ("high444", "yuv444p", "bframes=0", ["-profile:v", "high444"]),
    ("mono", "gray", "bframes=0", []),
    ("mbaff_cabac", "yuv420p", "interlaced=1:bframes=0", []),
    ("mbaff_cavlc", "yuv420p", "interlaced=1:cabac=0:bframes=0", []),
]


def fnv64(data):
    value = 0xcbf29ce484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    return value


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
