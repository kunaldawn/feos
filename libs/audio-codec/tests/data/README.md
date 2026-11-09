# Test fixtures

Everything generated here came from ffmpeg 8.0.1. Reference output files
(`*.s16`) are ffmpeg's own decode: `ffmpeg -i <file> -f s16le <file>.s16`.
They are decode references, and so must be regenerated with the file they go with.
`tests/fuzz.rs` seeds from every file in this directory except `*.s16` and `*.md`.

Tolerances are stated in `tests/audio.rs`: FLAC and G.711 are exact; Vorbis and
AAC allow 2 steps at worst and 0.5 RMS against ffmpeg's float decoder.

```sh
F="-v error -y -bitexact"

# G.711: every code, in AU. References are ffmpeg's expansion of each code.
python3 -c "
import struct
for name, enc in (('mulaw_all.au', 1), ('alaw_all.au', 27)):
    open(name, 'wb').write(b'.snd' + struct.pack('>5I', 24, 256, enc, 8000, 1) + bytes(range(256)))"
for f in mulaw_all alaw_all; do ffmpeg -v error -y -i $f.au -f s16le $f.s16; done

# MPEG Layer II, MPEG-1 and MPEG-2, with ffmpeg's decode as the reference. Layer I
# is random frames from make_layer1.py, as nothing here encodes it. Vorbis floor 0
# is the same, from make_floor0.py.
ffmpeg $F -f lavfi -i "sine=f=440:r=44100:d=0.2" -c:a mp2 -b:a 64k -f mp2 tone.mp2
ffmpeg -v error -y -i tone.mp2 -f s16le tone.mp2.s16
ffmpeg -v error -y -f lavfi -i "sine=f=330:r=22050:d=0.3" -f lavfi -i "sine=f=550:r=22050:d=0.3" -filter_complex "[0][1]amerge=2[a]" -map "[a]" -c:a mp2 -b:a 64k -f mp2 mpeg2-stereo.mp2
ffmpeg -v error -y -i mpeg2-stereo.mp2 -f s16le mpeg2-stereo.mp2.s16
(cd .. && python3 make_layer1.py && python3 make_floor0.py)
ffmpeg -v error -y -f mp3 -i layer1.mp1 -f s16le layer1.mp1.s16
ffmpeg -v error -y -i floor0.ogg -f s16le floor0.ogg.s16

# MP3 gapless reference: ffmpeg drops the Info frame, the encoder delay and padding.
ffmpeg -v error -y -i tone.mp3 -f s16le tone.mp3.s16

# FLAC. Hashes of the decoded output are in tests/audio.rs.
SRC='sine=f=440:r=44100:d=0.15[a];sine=f=1171:r=44100:d=0.15[b];anoisesrc=d=0.15:r=44100:c=pink:a=0.01:s=7[n];[a][b]amerge=2[ab];[n]asplit[n1][n2];[n1][n2]amerge=2[nn];[ab][nn]amix=normalize=0[o]'
ffmpeg $F -filter_complex "$SRC" -map '[o]' -c:a flac -compression_level 8 -sample_fmt s16 -fflags +bitexact -f flac tone.flac
ffmpeg $F -filter_complex "$SRC" -map '[o]' -c:a flac -compression_level 0 -sample_fmt s16 -ac 1 -ar 8000 -f flac fixed8k.flac
ffmpeg $F -filter_complex "$SRC" -map '[o]' -c:a flac -compression_level 5 -sample_fmt s32 -bits_per_raw_sample 24 -f flac deep24.flac
ffmpeg $F -filter_complex "$SRC" -map '[o]' -c:a flac -compression_level 5 -sample_fmt s16 -f ogg -fflags +bitexact tone-flac.oga
# The three native files then had their PADDING metadata block removed, which
# is mostly zeros and half of the file, keeping the other blocks and the frames.

# Vorbis, from libvorbis.
ffmpeg $F -filter_complex 'sine=f=440:r=44100:d=0.25[a];sine=f=1171:r=44100:d=0.25[b];[a][b]amerge=2[o]' -map '[o]' -c:a libvorbis -q:a 2 -f ogg -fflags +bitexact tone-stereo.ogg
ffmpeg $F -f lavfi -i 'anoisesrc=d=0.3:r=22050:c=pink:a=0.3:s=3' -c:a libvorbis -q:a 0 -ac 1 -f ogg -fflags +bitexact noise-mono.ogg
ffmpeg $F -f lavfi -i "aevalsrc='0.8*sin(2*PI*700*t)*exp(-40*mod(t,0.08))|0.5*sin(2*PI*2300*t)*exp(-90*mod(t+0.03,0.08))':s=32000:d=0.2" -c:a libvorbis -q:a 8 -f ogg -fflags +bitexact clicks-q8.ogg

# AAC-LC in ADTS, from ffmpeg's encoder. Noise substitution is off except in
# lc-noise.aac, since it is random and cannot match sample for sample.
ffmpeg $F -f lavfi -i "aevalsrc='0.7*sin(2*PI*700*t)*exp(-40*mod(t,0.08))+0.05*random(0)|0.5*sin(2*PI*1900*t)*exp(-30*mod(t+0.05,0.1))+0.05*random(1)':s=44100:d=0.24" -c:a aac -b:a 128k -aac_pns 0 -fflags +bitexact -f adts lc-stereo.aac
ffmpeg $F -f lavfi -i "anoisesrc=d=0.25:r=16000:c=pink:a=0.5:s=9" -c:a aac -b:a 24k -aac_pns 0 -fflags +bitexact -f adts lc-mono16k.aac
ffmpeg $F -f lavfi -i "anoisesrc=d=0.2:r=48000:c=pink:a=0.5:s=1" -filter_complex "[0]asplit[a][b];[a]highpass=f=3000[c];[b]highpass=f=3000,volume=0.5[d];[c][d]amerge=2" -c:a aac -b:a 32k -aac_is 1 -aac_pns 0 -aac_ms 1 -fflags +bitexact -f adts lc-intensity.aac
ffmpeg $F -f lavfi -i "anoisesrc=d=0.2:r=44100:c=white:a=0.3:s=5" -c:a aac -b:a 40k -aac_coder fast -aac_pns 1 -fflags +bitexact -f adts lc-noise.aac
```

`pcm16-sowt.aifc` is a hand-built AIFF-C file: 4 mono frames, little-endian
(`sowt`), to reach the compression-type parsing.

The older fixtures here (`pcm*.wav`, `pcm16.aiff`, `pcm16.au`, `float32.wav`,
`extensible.wav`, `tone.mp3`, `tone.opus`) predate this file.

## `src/aac_tables.rs`

The Huffman codebooks and scalefactor band offsets are the normative tables of
ISO/IEC 14496-3. `scripts/gen-aac-tables.py` generates them from
`libxaac/decoder/ixheaacd_aac_rom.c` in the `libxaac-sys` 0.1.0 crate: libxaac, the
Android Open Source Project's AAC decoder, under Apache-2.0. The script checks that
each codebook is a complete prefix code (its Kraft sum is exactly 1), and decoding
matches ffmpeg as above at each sampling rate from 8 to 96 kHz. The windows are
computed from the standard's
formula for the Kaiser-Bessel-derived window (alpha 4 long, alpha 6 short).

`lc-pce.aac` is 5.1 AAC-LC with a program config element, from FFmpeg's encoder with
`-aac_pce 1`, and `lc-pce.aac.s16` is FFmpeg's decoding of it. `opus/` is explained by
`make_opus.sh`, beside this folder, but for `opus/fec-*.bit`: 1.5 s from `silk-wb.s16`, encoded
by a small program over libopus 1.5.2 with in-band FEC on and 30% loss expected, in `opus_demo`'s
framing, at 16 kbit/s mono and 24 kbit/s stereo forced to SILK, and 40 kbit/s stereo forced to hybrid.

# HE-AAC with nothing to say so but its first frame, from a fraction of a second of an ISO
# conformance stream: ../../../video/tests/data/he-sbr.mp4, which is described in
# ../../../video/tests/data/README.md.
ffmpeg -v error -y -i he-sbr.mp4 -c copy -f adts he-implicit.aac
ffmpeg -v error -y -i he-implicit.aac -f s16le -c:a pcm_s16le he-implicit.aac.s16
```
