#!/bin/sh
# Makes the fixtures for tests/opus.rs, each with libopus: encoded by it through FFmpeg,
# then decoded by it. The references are FLAC, as PCM this long is large.
set -eu
cd "$(dirname "$0")/data/opus"
SRC='aevalsrc=0.35*sin(2*PI*(220+40*sin(2*PI*0.7*t))*t)+0.15*sin(2*PI*1400*t+2*sin(2*PI*3*t))+0.03*random(0)|0.3*sin(2*PI*(330+30*sin(2*PI*0.5*t))*t)+0.12*sin(2*PI*2200*t)+0.03*random(1):s=48000:d=1'
make() {
    name="$1"
    shift
    ffmpeg -nostdin -y -v error -f lavfi -i "$SRC" "$@" -c:a libopus "$name.opus"
    ffmpeg -nostdin -y -v error -c:a libopus -i "$name.opus" -ar 48000 -c:a flac -compression_level 12 "$name.flac"
}
make celt-128 -b:a 128k -application audio
make celt-low -b:a 64k -application lowdelay -frame_duration 5
make celt-2.5 -b:a 96k -application lowdelay -frame_duration 2.5
make celt-40 -b:a 64k -application audio -frame_duration 40
make silk-nb -b:a 8k -ac 1 -application voip -ar 8000
make silk-wb -b:a 16k -ac 1 -application voip -ar 16000
make silk-stereo -b:a 24k -application voip -ar 16000
make silk-60 -b:a 12k -ac 1 -application voip -frame_duration 60 -ar 16000
make hybrid-swb -b:a 24k -ac 1 -application voip -ar 24000
make hybrid-fb -b:a 32k -application voip
