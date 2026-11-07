#!/bin/sh
# Makes a FAT32 disk of H.264 streams for the h264 phase of scripts/media-boot-test.py.
#
#     scripts/make-h264-disk.sh <directory> [more files...]   # writes <directory>/h264-disk.img
#
# Clips of each mode x264 can make, from nothing, and any further files given, which is how
# field-coded conformance streams, that aren't for the repository, get on it.
set -eu

out="$1"
shift
mkdir -p "$out"
img="$out/h264-disk.img"
dir="$(mktemp -d "$out/files.XXXXXX")"
trap 'rm -rf "$dir"' EXIT

clip() {
    name="$1"; size="$2"; frames="$3"; format="$4"; params="$5"
    shift 5
    ffmpeg -nostdin -v error -f lavfi -i "testsrc2=s=$size:r=25" -frames:v "$frames" -pix_fmt "$format" \
        -c:v libx264 -x264-params "$params:log=-1" "$@" -f h264 "$dir/$name"
}
clip sp8.264 640x360 30 yuv420p bframes=2:ref=3
clip hi10.264 320x240 30 yuv420p10le bframes=2:ref=3 -profile:v high10
clip hi422.264 320x240 30 yuv422p bframes=2:ref=3 -profile:v high422
clip hi42210.264 320x240 30 yuv422p10le bframes=2:ref=3 -profile:v high422
clip mbaff.264 320x240 30 yuv420p interlaced=1:bframes=2:ref=3
# Larger than the player will hold, so they're read from the disk as they're needed.
ffmpeg -nostdin -v error -f lavfi -i "testsrc2=s=320x240:r=5" -t 600 -pix_fmt yuv420p \
    -c:v libx264 -preset ultrafast -crf 12 -g 25 -f h264 "$dir/big.264"
ffmpeg -nostdin -v error -f lavfi -i "testsrc2=s=320x240:r=5" -t 320 -c:v mjpeg -q:v 3 -f mjpeg "$dir/big.mjpeg"
# In lower case without underscores, which is what the test can type at the desktop.
for extra in "$@"; do
    cp "$extra" "$dir/$(basename "$extra" | tr 'A-Z' 'a-z' | tr -d '_')"
done
ls -l "$dir"

size=$(du -sm "$dir" | cut -f1)
truncate -s $((size + 64))M "$img"
mkfs.fat -F 32 -n FEOSH264 "$img" >/dev/null
# mtools is in the project's image; the host may not have it.
if command -v mcopy >/dev/null; then
    mcopy -i "$img" "$dir"/* ::/
else
    docker run --rm --network none -v "$out:$out" --entrypoint mcopy localhost/feos \
        -i "$img" "$dir"/* ::/
fi
