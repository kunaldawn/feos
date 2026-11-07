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

# Video with audio, a second of each, for the audio track tests.
AV = ["-f", "lavfi", "-i", "testsrc=size=64x48:rate=10:duration=1",
      "-f", "lavfi", "-i", "sine=f=440:r=44100:d=1", "-ac", "2"]
H264 = ["-c:v", "libx264", "-profile:v", "baseline", "-bf", "0", "-pix_fmt", "yuv420p",
        "-preset", "ultrafast", "-crf", "40"]
run(*AV, *H264, "-c:a", "aac", "-b:a", "64k", "av-aac.mp4")
run(*AV, *H264, "-c:a", "aac", "-b:a", "64k", "av-aac.flv")
run(*AV, *H264, "-c:a", "libmp3lame", "-b:a", "64k", "av-mp3.mkv")
run(*AV, *H264, "-c:a", "flac", "av-flac.mkv")
run(*AV, *H264, "-c:a", "libvorbis", "-q:a", "1", "av-vorbis.mkv")
run(*AV, "-c:v", "mjpeg", "-c:a", "pcm_s16le", "av-pcm.avi")
run(*AV, "-c:v", "mjpeg", "-c:a", "pcm_s16le", "av-pcm.mov")

# MPEG-1, with B pictures, as a raw stream and a program stream. Each reference
# is ffmpeg's decoding to RGBA.
M1V = ["-f", "lavfi", "-i", "testsrc=size=64x48:rate=25:duration=1", "-c:v", "mpeg1video", "-q:v", "4"]
run(*M1V, "-bf", "2", "-g", "8", "-f", "mpeg1video", "mpeg1.m1v")
run("-f", "lavfi", "-i", "testsrc=size=64x48:rate=25:duration=0.5", "-c:v", "mpeg1video", "-q:v", "4",
    "-bf", "1", "-g", "4", "-f", "mpeg", "mpeg1.mpg")
for clip in ["mpeg1.m1v", "mpeg1.mpg"]:
    run("-i", clip, "-f", "rawvideo", "-pix_fmt", "rgba", clip.replace(".", "_") + ".ref")

# MPEG-1 video and Layer II sound in a program stream, and ffmpeg's decoding of the sound.
run("-f", "lavfi", "-i", "testsrc2=s=64x48:r=25", "-f", "lavfi", "-i", "sine=f=440:r=44100,aformat=channel_layouts=stereo",
    "-t", "2", "-c:v", "mpeg1video", "-b:v", "100k", "-c:a", "mp2", "-b:a", "128k", "-f", "mpeg", "av-mp2.mpg")
run("-i", "av-mp2.mpg", "-vn", "-c:a", "pcm_s16le", "-f", "s16le", "av-mp2.mpg.s16")

# VP8, made by libvpx, for libs/video/tests/vp8.rs. Each .framecrc is ffmpeg's decoding,
# one Adler-32 per shown frame in yuv420p. Each file is there for a feature of the format,
# and the test checks it really has it.
SRC = ["-f", "lavfi", "-i", "testsrc2=s=176x144:r=25", "-t", "1.2", "-pix_fmt", "yuv420p", "-c:v", "libvpx"]
def vp8(name, *options):
    run(*SRC, *options, name)
    subprocess.run(["ffmpeg", "-y", "-v", "error", "-i", name, "-f", "framecrc", "-pix_fmt", "yuv420p", name + ".framecrc"], check=True)
vp8("vp8-inter.webm", "-b:v", "150k", "-g", "12")
# Hidden alternate references take two passes, and enough frames to look ahead over.
for number in ["1", "2"]:
    run("-f", "lavfi", "-i", "testsrc2=s=96x80:r=25", "-t", "3", "-pix_fmt", "yuv420p", "-c:v", "libvpx", "-b:v", "60k",
        "-auto-alt-ref", "1", "-lag-in-frames", "25", "-g", "75", "-pass", number, "-passlogfile", "/tmp/vp8-altref-pass", "vp8-altref.webm")
subprocess.run(["ffmpeg", "-y", "-v", "error", "-i", "vp8-altref.webm", "-f", "framecrc", "-pix_fmt", "yuv420p", "vp8-altref.webm.framecrc"], check=True)
vp8("vp8-parts.webm", "-b:v", "150k", "-slices", "8", "-g", "20")
vp8("vp8-seg.webm", "-b:v", "120k", "-deadline", "realtime", "-cpu-used", "5", "-error-resilient", "1", "-g", "30")
vp8("vp8-profile1.webm", "-b:v", "150k", "-profile:v", "1")
vp8("vp8-profile2.webm", "-b:v", "150k", "-profile:v", "2")
vp8("vp8-profile3.webm", "-b:v", "150k", "-profile:v", "3")
vp8("vp8-sharp.webm", "-b:v", "60k", "-sharpness", "5", "-g", "10")
run("-f", "lavfi", "-i", "testsrc2=s=33x17:r=25", "-t", "1", "-pix_fmt", "yuv420p", "-c:v", "libvpx", "-b:v", "40k", "-f", "ivf", "vp8-odd.ivf")
subprocess.run(["ffmpeg", "-y", "-v", "error", "-i", "vp8-odd.ivf", "-f", "framecrc", "-pix_fmt", "yuv420p", "vp8-odd.ivf.framecrc"], check=True)

# VP8 and Opus in WebM, and libopus's decoding of the sound.
run("-f", "lavfi", "-i", "testsrc2=s=64x48:r=10", "-f", "lavfi", "-i", "sine=f=440:r=48000", "-t", "1",
    "-c:v", "libvpx", "-b:v", "40k", "-c:a", "libopus", "-b:a", "32k", "av-opus.webm")
run("-c:a", "libopus", "-i", "av-opus.webm", "-vn", "-ar", "48000", "-f", "s16le", "av-opus.webm.s16")
