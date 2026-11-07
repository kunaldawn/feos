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

## What's been checked

See `libs/audio-codec/tests/opus.rs`. Against libopus 1.6.1 by way of FFmpeg, on fixtures from
its encoder: SILK narrow and wide band, mono, are identical; everything else is within 1 of
16 bits. All 12 RFC 8251 vectors decode with no final range error, which checks the entropy
decoding exactly. Their PCM is within 1 for vectors 1, 2, 3, 4, 7, 9 and 11 and 4 for 8, but up to
242 (43 dB SNR) for 5, 6, 10 and 12. Those have SILK and hybrid switching, and that wasn't
tracked down.

PLC and FEC exist but weren't compared, nor multistream, and `feos-audio-codec` refuses
anything but one or two channels with mapping family 0.
