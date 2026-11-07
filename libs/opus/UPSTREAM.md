# Provenance of feos-opus

## Origin

A vendored copy of the [`opus-decoder`](https://crates.io/crates/opus-decoder) crate,
version 0.1.1, from crates.io (repository `github.com/TadeuszWolfGang/Rusopus`), under
`MIT OR Apache-2.0`. By its own description it's a port of libopus, keeping close to
it, so libopus's BSD notice comes along in `LICENSE-libopus`. libopus 1.5.2 was used here
as the reference for reading, and none of its code was copied.

The crate file's SHA-256 is
`15ae32c275dabb0cd2c863460bf349e9abc3568ba2abf0f9eb7b7d2edeeed07e`.
No upstream commit is recorded: the crate doesn't carry one.

## Local changes

- `no_std` with `alloc`. `thiserror` is gone. `src/math.rs` provides the float functions
  that are in `std` and not `core`, with host tests against `std`.
- **Removed debugging that wrote files.** Published 0.1.1 has about 90 `// #region agent
  log` blocks and others that append to `.cursor/debug-*.log` in the working directory, or to
  a path on the author's machine, during ordinary decoding. All of those are gone, along
  with `compare.rs`, which only tests used. Some inert trace branches remain, as they
  don't change behaviour and removing them all by hand risks more than it saves.
- **A wrong picture of the collapse mask.** `extract_collapse_mask` returned `u8`. Blocks
  can number 16 while `tf_change` has time resolution raised, which panicked in a debug
  build and gave the wrong mask in a release one, audibly: 860 away from libopus on hybrid and
  40 ms CELT fixtures. It's a `u32` now, as in libopus.
- **The NLSF interpolation flag.** `decode_core` guessed it by comparing the two
  coefficient sets. Those can match with interpolation on, and then libopus still
  re-whitens the history at subframe 2. That was 963 away on a narrow-band SILK fixture. It
  comes from the stream now.
- Malformed packets: redundancy that claims more than the frame has is `BadPacket`.
- **Multistream framing.** Published 0.1.1 read a bare length before each stream's packet.
  RFC 6716 appendix B's self-delimited form is the packet itself with the size of its
  last frame added, for all four frame count codes.
- **Mode changes.** SILK was reset coming from CELT only for SILK-only packets, not hybrid,
  and the fade in from the old mode's concealment wasn't there, or faded in from silence.
  Both are as in libopus now, and a frame with redundant data has that instead.
- **Speed.** The inverse MDCT did its FFT as a direct sum, with a sine and cosine for each
  term. It's a real FFT now, with the same results within rounding: that was the decoder's cost,
  as it was 1.2 times real time at 256 kbit/s natively.
- **Hybrid folding in mono.** Only the stereo path did libopus's `special_hybrid_folding`, the
  fix RFC 8251 made for a hybrid frame's second CELT band, so mono hybrid frames differed in
  that band (9.6 to 12 kHz), up to 242 away.

## What's been checked

See `libs/audio-codec/tests/opus.rs`. Against libopus 1.6.1 by way of FFmpeg, on fixtures from
its encoder: SILK narrow and wide band, mono, are identical; everything else is within 1 of
16 bits. All 12 RFC 8251 vectors decode with no final range error, which checks the entropy
decoding exactly. Their PCM is within 1 for vectors 1, 2, 3, 4, 7, 9 and 11, 2 for 5, 4 for 8, 10
for 6 and 12 and 33 for 10 (76.6 dB SNR). What's left in 6, 10 and 12 is a few packets after some
switches to hybrid, not tracked down.

Mapping family 1 is compared with libopus through FFmpeg, and the rest of the families are
refused. Concealment does not match libopus: 12 vectors with every 23rd packet dropped, against
a libopus 1.5.2 build doing the same, are -25 to 16 dB. It's only reached when a container has
an empty packet, as files lose nothing. FEC isn't implemented: asking for it conceals.
