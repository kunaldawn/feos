#!/bin/sh
# Makes a FAT32 disk of large media files for scripts/media-boot-test.py, with ffmpeg,
# from nothing, so it can go somewhere that isn't the repository.
#
#     scripts/make-media-disk.sh <directory>      # writes <directory>/media-disk.img
#
# The tone in each file goes up by 100 Hz a minute from 300 Hz, so what a test hears says
# where playback is. There's noise under it, as files of pure tones compress to nothing.
set -eu

out="$1"
mkdir -p "$out"
img="$out/media-disk.img"
[ -f "$img" ] && exit 0
dir="$(mktemp -d "$out/files.XXXXXX")"
trap 'rm -rf "$dir"' EXIT

MINUTES=16
SECONDS_LONG=$((MINUTES * 60))
TONE="aevalsrc='0.3*sin(2*PI*(300+100*floor(t/60))*t)+0.02*random(0)|0.3*sin(2*PI*(300+100*floor(t/60))*t)+0.02*random(1)':s=48000:d=$SECONDS_LONG"

ffmpeg -nostdin -v error -f lavfi -i "$TONE" -c:a flac -compression_level 2 "$dir/big.flac"
ffmpeg -nostdin -v error -f lavfi -i "$TONE" -c:a libmp3lame -b:a 192k "$dir/big.mp3"
ffmpeg -nostdin -v error -f lavfi -i "$TONE" -c:a libvorbis -q:a 8 "$dir/big.ogg"
ffmpeg -nostdin -v error -f lavfi -i "$TONE" -c:a aac -b:a 192k "$dir/big.aac"
ffmpeg -nostdin -v error -f lavfi -i "$TONE" -t 480 -c:a pcm_s16le "$dir/big.wav"
# Twice the size of the kernel heap, nearly all of it sound, as frames this small take
# little to decode and the guest has no hardware to do it with.
ffmpeg -nostdin -v error -f lavfi -i "testsrc2=s=160x120:r=5" -f lavfi -i "$TONE" -t 720 \
    -c:v libx264 -preset ultrafast -g 25 -pix_fmt yuv420p -c:a pcm_s16le "$dir/big.mov"
ffmpeg -nostdin -v error -f lavfi -i "$TONE" -c:a libopus -b:a 256k "$dir/big.opus"
# VP8 and Opus in WebM, again small frames: most of this is sound.
ffmpeg -nostdin -v error -f lavfi -i "testsrc2=s=160x120:r=5" -f lavfi -i "$TONE" -t 720 \
    -c:v libvpx -deadline realtime -cpu-used 8 -b:v 100k -c:a libopus -b:a 256k "$dir/big.webm"
ffmpeg -nostdin -v error -f lavfi -i "testsrc2=s=64x48:r=25" -f lavfi -i "sine=f=440:r=44100,aformat=channel_layouts=stereo" -t 4 \
    -c:v mpeg1video -b:v 100k -c:a mp2 -b:a 128k -f mpeg "$dir/av.mpg"
ls -l "$dir"

size=$(du -sm "$dir" | cut -f1)
truncate -s $((size + 64))M "$img"
mkfs.fat -F 32 -n FEOSMEDIA "$img" >/dev/null
# mtools is in the project's image; the host may not have it.
if command -v mcopy >/dev/null; then
    mcopy -i "$img" "$dir"/* ::/
else
    docker run --rm --network none -v "$out:$out" --entrypoint mcopy localhost/feos \
        -i "$img" "$dir"/big.flac "$dir"/big.mp3 "$dir"/big.ogg "$dir"/big.aac "$dir"/big.wav "$dir"/big.mov "$dir"/av.mpg "$dir"/big.opus "$dir"/big.webm ::/
fi
