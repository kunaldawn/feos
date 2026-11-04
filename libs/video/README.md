# feos-video

`feos-video` is a dependency-light video demuxer and decoder for the feos
kernel. It follows the same rules as `feos-image` and `feos-audio-codec`:
containers are parsed in place, decoded frames go into a caller-provided
RGBA8888 buffer, and the crate uses no `unsafe`.

`info` allocates next to nothing. `VideoDecoder` keeps a sample table and codec
state in `alloc`, all of it fallible and bounded by `DecoderLimits`, because
exhausting the kernel heap is fatal. H.264 reconstruction is delegated to the
vendored [`feos-h264`](../h264/) crate. `feos-video` is `no_std`.

## Formats

| Container | Demux | Pixel decode |
|-----------|-------|--------------|
| GIF (animated) | yes | yes (LZW, transparency, disposal) |
| AVI (RIFF), OpenDML, `rec ` lists | yes | MJPEG, uncompressed RGB/YUV/palette, RLE8/RLE4, H.264 |
| MP4 / MOV (ISO BMFF), fragmented | yes | MJPEG, uncompressed, H.264 |
| Matroska / WebM, unknown-size clusters, header-strip and zlib tracks | yes | MJPEG, uncompressed, H.264 |
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
use feos_video::VideoDecoder;

let mut decoder = VideoDecoder::open(video_bytes)?;
let mut rgba = vec![0u8; decoder.info().rgba_len()?];
for index in 0..decoder.frame_count() {
    let frame = decoder.decode(index, &mut rgba)?;
    println!("{} ms: {}x{}", frame.pts_ms, frame.width, frame.height);
}
```

The next frame costs one frame to decode, and a seek restarts from the closest
key frame. `advance` is `decode` with a limit on the samples decoded per call,
for a caller on a thread that must keep drawing. `decode_frame` is a stateless
wrapper, and restarts from a key frame on every call.

## API

- `detect(input) -> Option<ContainerFormat>`
- `info(input) -> Result<VideoInfo>`: decoded size, container, codec, frame count,
  frame rate, display metadata (aspect ratio, rotation) and colour signalling
- `VideoDecoder::{open, with_limits}`, `decode`, `advance`, `frame_count`,
  `duration_ms`, `frame_time_ms`, `frame_delay_ms`, `frame_at_time`,
  `key_frame_at_or_before`, `key_frame_after`
- `scratch_len(input) -> Result<usize>` and
  `decode_frame(input, index, out, scratch) -> Result<DecodedFrame>`
- `Codec::can_decode()` reports whether this build reconstructs pixels
- `VideoError::is_fatal()` separates what ends a video from one damaged frame
- `pixel` exposes the format helpers used by the raw-frame decoders

## Timing, geometry and colour

- Frame times come from `stts`, `ctts`, edit lists and `trun`, Matroska block
  timestamps, FLV timestamps with composition offsets, and GIF delays. Frames
  are numbered in display order.
- Frames decode at their stored size. `VideoInfo::display` gives the sample aspect
  ratio and clockwise rotation, from `pasp`, `tkhd`, Matroska display size and
  projection, or the H.264 VUI, and `display_size` the resulting shape. Drawing
  them that way is up to the caller.
- YUV conversion follows the stream's signalling (BT.601, BT.709 or BT.2020, limited
  or full range). If it's unspecified, video of 720 lines or more is taken as BT.709.

Key frames come from `stss`, `trun` flags, Matroska block flags and Cues, AVI `idx1`
and FLV frame types. For H.264 a frame is only used as a seek point if it's also an
IDR, by scanning its NAL headers: other "key frames" aren't self-contained.

## Scope limits

- H.264 support tracks `feos-h264`: 8-bit progressive 4:2:0 Baseline, Main and
  High profiles. Anything else returns an error, and a damaged sample loses only
  its picture.
- VP8, VP9, AV1, HEVC, MPEG-1/2/4, Theora, ProRes, VC-1, VP6, H.263 and
  Cinepak samples are locatable but not decoded to pixels.
- Only the first video track of a file is decoded, and audio isn't demuxed.
- An edit list is honoured for its start offset and leading delay, not its later
  segments.
- The whole file must be in memory.

## Attribution

The H.264 decoder is vendored from `z264` (MIT OR Apache-2.0), which adapts
`rust_h264` 0.4.x. See [`libs/h264/README.md`](../h264/README.md) and
[`libs/h264/UPSTREAM.md`](../h264/UPSTREAM.md) for provenance and
licensing. Containers, GIF, raw/RLE conversion and the H.264 glue in this
crate were written for feos.

## Tests

```sh
cargo test -p feos-video --target x86_64-unknown-linux-gnu
```

The integration tests decode small FFmpeg-generated fixtures from
`tests/data/` and compare them against RGBA reference frames. H.264 and MJPEG
allow a small tolerance for IDCT and colour-conversion rounding. `tests/regress.rs`
holds regression tests, and `tests/fuzz.rs` decodes mutated files under a 64 MiB
heap limit and a small stack.
