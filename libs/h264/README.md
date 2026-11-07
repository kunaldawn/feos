# feos-h264

`feos-h264` is a dependency-free, `no_std` H.264/AVC decoder for the feos
kernel. Its macroblock decoding, in `src/ext/`, is written for feos. Its parameter set
parsing and tables, and some unused macroblock paths, are a vendored port of
[`z264`](https://github.com/p14c31355/z264), which in turn adapts the pure-Rust
`rust_h264` 0.4 implementation. The crate is kept as its own workspace member so the rest of
the kernel does not have to link an allocator-heavy codec unless it needs one.

It uses `alloc` for dynamic picture storage and is intended to run in native
applications, `wasm32-unknown-unknown`, and WASI hosts as well as on bare
metal with a suitable global allocator. The decoder itself is `#![no_std]`.

The licensing of the implementation code and the handling of video codec
patents are separate matters. See `docs/LICENSE-MIT` and `docs/LICENSE-APACHE`.

The decoder takes Baseline without FMO, Main, High, High 10 and High 4:2:2 streams,
progressive, field-coded or MBAFF, monochrome, 4:2:0 or 4:2:2, with CAVLC or CABAC, and
reorders for display. 4:4:4, lossless, FMO and SP/SI slices return an error. Macroblocks a
damaged stream doesn't deliver are copied from the latest reference picture, and
`Frame::concealed_mbs` counts them.

A `Frame`'s `y`, `u` and `v` are always 8-bit 4:2:0, for display: deeper samples are rounded, and
4:2:2 chroma averaged, within each field of an interlaced stream. Fields are woven into
frames. `Decoder::set_keep_native` also gives the samples as coded, in `Frame::native`.

It uses `alloc` under `Limits`: a stream is refused with
`DecodeError::OutOfMemory` if it can't fit, and picture allocation is fallible.

## Attribution

- Original decoder: `z264` by p14c31355, MIT OR Apache-2.0
  (<https://github.com/p14c31355/z264>).
- Upstream implementation: `rust_h264` 0.4.x, MIT OR Apache-2.0.
- NEON motion-compensation kernels are retained from the vendored source. They
  and the lazy table cell in `src/once.rs` are the only `unsafe` code in this crate.
  `src/ext/` has none.

See [`UPSTREAM.md`](UPSTREAM.md) for provenance and the local patches.

## Example

```rust,no_run
use feos_h264::{Decoder, annex_b_units};

let bitstream = std::fs::read("input.h264").unwrap();
let mut decoder = Decoder::new();

for nal in annex_b_units(&bitstream) {
    if let Ok(Some(frame)) = decoder.decode_nal(&nal) {
        println!(
            "decoded {}x{} frame, POC={}",
            frame.width, frame.height, frame.pic_order_cnt
        );
    }
}
if let Some(frame) = decoder.flush() {
    println!("final frame, POC={}", frame.pic_order_cnt);
}
```

Frames are returned in decode order; sort by `pic_order_cnt` within a GOP for
display order, or use `OrderedDecoder`. Planes hold the whole coded picture: read
the visible part with `y_row`, `u_row` and `v_row`, or copy it with `append_i420`.

## Tests

```sh
cargo test -p feos-h264 --target x86_64-unknown-linux-gnu
```

`tests/conformance.rs` checks clips made with libx264 (CABAC, CAVLC, B-frames and
pyramids, High profile, custom scaling, multiple slices, crops) against FFmpeg's
output, which is bit-exact. `testdata/generate.py` documents and regenerates them.
`tests/malformed.rs` decodes mutated streams under a 64 MiB heap limit and a small
stack. `tests/fate.rs` compares every frame with FFmpeg on real conformance streams, which
aren't in this repository, and says it was skipped if not pointed at them.
