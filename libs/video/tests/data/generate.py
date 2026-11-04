#!/usr/bin/env python3
"""Regenerates the robustness fixtures with FFmpeg and libx264.

The *.ref files are FFmpeg's RGBA for every frame of a clip, concatenated. All
the bframes.* and fragmented.mp4 clips hold the same encode, so they share one.

Run from this directory: python3 generate.py
"""
import subprocess


def run(*args):
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", *args], check=True)


def source(size, frames=8):
    return ["-f", "lavfi", "-i", f"testsrc2=size={size}:rate=10:duration=4", "-frames:v", str(frames)]


def x264(params):
    return ["-pix_fmt", "yuv420p", "-c:v", "libx264", "-x264-params", params + ":log=-1"]


GOP = "bframes=2:b-adapt=0:ref=3:keyint=4:min-keyint=4:scenecut=0"
PALETTE = "split[a][b];[a]palettegen=max_colors={}[p];[b][p]paletteuse=dither=none"

for name, fmt in [("mp4", "mp4"), ("mkv", "matroska"), ("flv", "flv"), ("avi", "avi"), ("h264", "h264")]:
    run(*source("48x32", 12), *x264(GOP), "-f", fmt, "bframes." + name)
run(*source("48x32", 12), *x264(GOP), "-movflags", "frag_keyframe+empty_moov+default_base_moof",
    "-f", "mp4", "fragmented.mp4")
run("-display_rotation", "90", "-i", "bframes.mp4", "-c", "copy", "-f", "mp4", "rotated.mp4")
run("-display_rotation", "90", "-i", "bframes.mkv", "-c", "copy", "-f", "matroska", "rotated.mkv")
for name, fmt in [("mp4", "mp4"), ("mkv", "matroska")]:
    run(*source("48x32", 2), *x264("bframes=0"), "-vf", "setsar=2/1", "-f", fmt, "anamorphic." + name)
run(*source("48x32", 2), "-pix_fmt", "yuvj420p", "-c:v", "libx264", "-x264-params",
    "bframes=0:colormatrix=bt709:range=pc:colorprim=bt709:transfer=bt709:log=-1",
    "-f", "h264", "bt709_full.h264")
run("-f", "lavfi", "-i", "testsrc2=size=32x32:rate=10:duration=3",
    "-vf", r"select='not(eq(n\,3)+eq(n\,4)+eq(n\,5))',settb=1/1000", "-fps_mode", "vfr",
    "-frames:v", "8", *x264("bframes=0"), "-f", "mp4", "vfr.mp4")

# Widths not divisible by 4: DIB rows are padded.
for name, pix_fmt in [("bgr24", "bgr24"), ("gray", "gray"), ("bgr0", "bgra"), ("yuy2", "yuyv422"),
                      ("rgb555", "rgb555le")]:
    run(*source("30x18", 2), "-c:v", "rawvideo", "-pix_fmt", pix_fmt, "-f", "avi", f"raw_{name}_w30.avi")
run(*source("30x18", 2), "-vf", PALETTE.format(32), "-c:v", "rawvideo", "-pix_fmt", "pal8",
    "-f", "avi", "raw_pal8_w30.avi")
for tag in ["YV12", "I420"]:
    run(*source("30x18", 2), "-c:v", "rawvideo", "-pix_fmt", "yuv420p", "-vtag", tag,
        "-f", "avi", f"raw_{tag.lower()}_w30.avi")
run(*source("30x18", 2), "-c:v", "rawvideo", "-pix_fmt", "yuv420p", "-f", "matroska", "raw_i420.mkv")
run(*source("30x18", 2), "-c:v", "rawvideo", "-pix_fmt", "bgr24", "-allow_raw_vfw", "1",
    "-f", "matroska", "raw_bgr24_vfw.mkv")
run(*source("32x32", 6), "-vf", PALETTE.format(16), "-c:v", "msrle", "-g", "3", "-f", "avi", "rle8.avi")

for clip in ["bframes.h264", "anamorphic.mp4", "bt709_full.h264", "vfr.mp4", "rle8.avi", "raw_i420.mkv",
             "raw_bgr24_vfw.mkv", "raw_bgr24_w30.avi", "raw_gray_w30.avi", "raw_bgr0_w30.avi",
             "raw_yuy2_w30.avi", "raw_rgb555_w30.avi", "raw_pal8_w30.avi", "raw_yv12_w30.avi",
             "raw_i420_w30.avi"]:
    run("-i", clip, "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "rgba",
        clip.replace(".", "_") + ".ref")
