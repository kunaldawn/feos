# Implementation provenance

The initial AVC implementation was adapted from the pure-Rust `rust_h264`
0.4.x implementation. It is vendored here so the published `z264` crate has
no runtime crate dependencies and can be compiled with `#![no_std]` plus
`alloc`.

The adaptation replaces standard-library containers and synchronization with
`alloc`/`core` equivalents, removes the standard error trait, and adds fixed
size CABAC scratch storage, shared DPB frame planes, and regression tests.

The upstream implementation is available under the same MIT OR Apache-2.0
license. See `docs/LICENSE-MIT` and `docs/LICENSE-APACHE`.
