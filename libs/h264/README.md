# feos-h264

`feos-h264` is a dependency-free, `no_std` H.264/AVC decoder for the feos
kernel. It is a vendored port of [`z264`](https://github.com/p14c31355/z264),
which in turn adapts the pure-Rust `rust_h264` 0.4 implementation. The crate is
kept as its own workspace member so the rest of the kernel does not have to
link an allocator-heavy codec unless it needs one.

It uses `alloc` for dynamic picture storage and is intended to run in native
applications, `wasm32-unknown-unknown`, and WASI hosts as well as on bare
metal with a suitable global allocator. The decoder itself is `#![no_std]`.

The licensing of the implementation code and the handling of video codec
patents are separate matters. See `docs/LICENSE-MIT` and `docs/LICENSE-APACHE`.

The decoder targets 8-bit progressive 4:2:0 Baseline, Main, and High-profile
streams, including CAVLC/CABAC, P/B pictures, reference pictures, and
display-order reordering. High 10, 4:2:2, 4:4:4, monochrome, lossless, MBAFF,
field pictures, FMO and SP/SI slices return an error.

It uses `alloc` under `Limits`: a stream is refused with
`DecodeError::OutOfMemory` if it can't fit, and picture allocation is fallible.

## Attribution

- Original decoder: `z264` by p14c31355, MIT OR Apache-2.0
  (<https://github.com/p14c31355/z264>).
- Upstream implementation: `rust_h264` 0.4.x, MIT OR Apache-2.0.
- NEON motion-compensation kernels are retained from the vendored source. They
  and the lazy table cell in `src/once.rs` are the only `unsafe` code in this crate.

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
stack.
