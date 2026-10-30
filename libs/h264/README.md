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
display-order reordering. Unsupported H.264 features return an error.

## Attribution

- Original decoder: `z264` by p14c31355, MIT OR Apache-2.0
  (<https://github.com/p14c31355/z264>).
- Upstream implementation: `rust_h264` 0.4.x, MIT OR Apache-2.0.
- NEON motion-compensation kernels are retained from the vendored source and
  are the only `unsafe` code in this crate.

The public crate name, documentation and package metadata were rebranded for
feos; the decoding algorithms are unchanged. See `docs/UPSTREAM.md` for the
provenance note copied from the upstream crate.

## Example

```rust,no_run
use feos_h264::{Decoder, parse_annex_b};

let bitstream = std::fs::read("input.h264").unwrap();
let nals = parse_annex_b(&bitstream);
let mut decoder = Decoder::new();

for nal in &nals {
    if let Ok(Some(frame)) = decoder.decode_nal(nal) {
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
display order. The Y plane is `width * height` bytes and the U/V planes are
`(width / 2) * (height / 2)` bytes each.

## Tests

```sh
cargo test -p feos-h264 --target x86_64-unknown-linux-gnu
```

The test corpus (`testdata/mini.h264`) is checked against the matching raw YUV
output produced by FFmpeg.
