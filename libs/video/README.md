# feos-video

`feos-video` is a dependency-light video demuxer and decoder for the feos
kernel. It follows the same rules as `feos-image` and `feos-audio-codec`:
containers are parsed in place, decoded frames go into a caller-provided
RGBA8888 buffer, and the crate itself never allocates or uses `unsafe`.

H.264 reconstruction is delegated to the vendored [`feos-h264`](../h264/)
crate, which uses `alloc` for its reference-picture buffer. `feos-video` is
`no_std` and safe Rust apart from that dependency.

## Formats

| Container | Demux | Pixel decode |
|-----------|-------|--------------|
| GIF (animated) | yes | yes (LZW, transparency, disposal) |
| AVI (RIFF) | yes | MJPEG, uncompressed RGB/YUV, RLE8/RLE4, H.264 |
| MP4 / MOV (ISO BMFF) | yes | MJPEG, uncompressed, H.264 |
| Matroska / WebM | yes | MJPEG, uncompressed, H.264 |
| FLV | yes | H.264 |
| IVF | yes | metadata only (VP8/VP9/AV1 payload) |
| Annex-B H.264 | yes | yes |
| Motion JPEG | yes | yes |

The demuxers recognise HEVC, VP8, VP9, AV1, MPEG-1/2/4, Theora, ProRes, VC-1,
VP6, H.263 and Cinepak as well. Their dimensions and frame counts are reported
by `info`, but pixel reconstruction is not implemented for them yet;
`decode_frame` returns `VideoError::UnsupportedCodec`.

## Example

```rust
use feos_video::{decode_frame, info, scratch_len};

let props = info(video_bytes)?;
let mut rgba = vec![0u8; props.rgba_len()?];
let mut scratch = vec![0u8; scratch_len(video_bytes)?];
let frame = decode_frame(video_bytes, 0, &mut rgba, &mut scratch)?;

println!("{}x{}, delay {} ms", frame.width, frame.height, frame.delay_ms);
```

`scratch_len` is non-zero only for animated GIF, which needs one index plane
plus one saved canvas. Every other container decodes directly from the input
buffer.

Colour conversion uses the limited-range BT.601 matrix, matching FFmpeg's
default YUV to RGB conversion for H.264 and raw YUV samples.

## API

- `detect(input) -> Option<ContainerFormat>`
- `info(input) -> Result<VideoInfo>` with dimensions, container, codec, frame
  count and (where known) frame rate
- `scratch_len(input) -> Result<usize>`
- `decode_frame(input, index, out, scratch) -> Result<DecodedFrame>`
- `Codec::can_decode()` reports whether this build reconstructs pixels
- `pixel` exposes the format helpers used by the raw-frame decoders

## Scope limits

- H.264 support tracks `feos-h264`: 8-bit progressive 4:2:0 Baseline, Main and
  High profiles. Unsupported profiles or features return an error.
- VP8, VP9, AV1, HEVC, MPEG-1/2/4, Theora, ProRes, VC-1, VP6, H.263 and
  Cinepak samples are locatable but not decoded to pixels.
- MP4 support covers the sample tables needed for the common video track:
  `stsd`, `stsz`, `stsc`, `stco`/`co64` and `stts`. Fragmented MP4 does not
  have an `stbl` sample table and is not supported.
- JPEG inside Motion JPEG is baseline/extended-sequential; progressive JPEG is
  rejected by `feos-image`.

## Attribution

The H.264 decoder is vendored from `z264` (MIT OR Apache-2.0), which adapts
`rust_h264` 0.4.x. See [`libs/h264/README.md`](../h264/README.md) and
[`libs/h264/docs/UPSTREAM.md`](../h264/docs/UPSTREAM.md) for provenance and
licensing. Containers, GIF, raw/RLE conversion and the H.264 glue in this
crate were written for feos.

## Tests

```sh
cargo test -p feos-video --target x86_64-unknown-linux-gnu
```

The integration tests decode small FFmpeg-generated fixtures from
`tests/data/` and compare them against RGBA reference frames. H.264 and MJPEG
allow a small tolerance for IDCT and colour-conversion rounding.
