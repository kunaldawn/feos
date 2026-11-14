# adler32furious

[![crates.io](https://img.shields.io/crates/v/adler32furious.svg)](https://crates.io/crates/adler32furious)
[![docs.rs](https://img.shields.io/docsrs/adler32furious)](https://docs.rs/adler32furious)
[![CI](https://github.com/srijs/adler32furious/actions/workflows/ci.yml/badge.svg)](https://github.com/srijs/adler32furious/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/adler32furious)](Cargo.toml)
[![License](https://img.shields.io/crates/l/adler32furious.svg)](#license)

Fast [Adler-32](https://datatracker.ietf.org/doc/html/rfc1950#section-9)
checksums for Rust. It has SIMD versions for x86 and AArch64, chooses the best
one for the CPU at runtime, and has a fast portable version for everything
else.

```rust
use adler32furious::{adler32, Adler32};

assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);

let mut a = Adler32::new();
a.update(b"Wiki");
a.update(b"pedia");
assert_eq!(a.checksum(), 0x11E6_0398);
```

`Adler32` also supports `combine`, which joins the checksums of adjacent
pieces computed separately, for example in parallel. It implements `Hasher`
and `io::Write`.

## Performance

Throughput in GiB/s on 1 MiB of data:

| CPU | Implementation used | adler32furious | [simd-adler32] | [adler2] |
| --- | --- | ---: | ---: | ---: |
| Intel Sapphire Rapids | AVX-512 VNNI, 512-bit | **100.9** | 41.0 | 9.6 |
| AMD Zen 4 | AVX-512 VNNI, 512-bit | **83.8** | 50.0 | 10.8 |
| Intel Ice Lake | AVX-512 VNNI, 256-bit | **64.9** | 29.6 | 8.7 |
| Apple M4 | NEON with dot product | **66.7** | 42.0 | 7.2 |
| AMD Zen 3 | AVX2 | **51.0** | 45.7 | 10.0 |

On these CPUs that is 1.1× (Zen 3, where both crates use AVX2) to 2.5×
(Sapphire Rapids) the speed of simd-adler32, and 5–10× the speed of adler2.

By input size, on an Apple M4:

| Crate | 16 B | 64 B | 1 KiB | 16 KiB | 1 MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| **adler32furious** | **6.2** | 18.6 | **60.5** | **66.4** | **66.7** |
| [simd-adler32] | 2.4 | **19.8** | 42.1 | 42.1 | 42.0 |
| [adler2] | 3.3 | 6.5 | 7.5 | 7.2 | 7.2 |

Inputs that are not a multiple of the vector size are fast too. From 16
bytes on, the last partial vector is loaded so that it ends where the input
does, with the bytes already summed masked off, so a 63-byte input costs
about the same as a 64-byte one. The AVX-512 versions use masked loads for
any length.

The portable version is used on CPUs without a SIMD version. It processes a
machine word at a time, treating it as several 16-bit lanes, and runs at
7–15 GiB/s on these machines. That is well ahead of adler2 up to 16 KiB, and
about level with it on larger inputs.

The x86 numbers are from AWS `c7i`, `c7a`, `c6i` and `c6a` instances. To
reproduce, run `cargo bench -p adler32furious-bench`. It also benchmarks each
implementation the CPU supports on its own.

[simd-adler32]: https://crates.io/crates/simd-adler32
[adler2]: https://crates.io/crates/adler2

## Implementations

The fastest implementation the CPU supports is used:

| Arch | Implementations, fastest first |
| --- | --- |
| x86 / x86_64 | AVX-512 VNNI (512- and 256-bit vectors), AVX-VNNI, AVX2, SSE2 |
| AArch64 (little-endian) | NEON with dot product, NEON |
| anything else | portable, a machine word at a time |

512-bit vectors are not used on Intel CPUs that slow down too much when they
run them. AVX-512 is never used on macOS.

Choosing costs at most one load of a cached value per call. When the target
features enabled at compile time already decide it, as with dot product on
Apple targets, the implementation is called directly.

Every implementation is tested against a reference C implementation and
fuzzed. In CI, every x86 implementation runs under Intel's SDE emulator.

## Features

- `std` (enabled by default): detects CPU features at runtime.

  Without `std`, the crate is `no_std` and picks the implementation at
  compile time, from the enabled target features. By default that is the
  baseline: SSE2 on x86_64 and NEON on AArch64. To use a faster
  implementation, build with `-C target-cpu=native` or
  `-C target-feature=+avx2`. One check is still made at runtime, once:
  whether AVX-512 should use 512-bit vectors.

Under Miri the portable version is always used, so crates that depend on
this one can run their tests under Miri.

The minimum supported Rust version is 1.89, which stabilized the AVX-512
intrinsics.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

This crate is ported from [libdeflate](https://github.com/ebiggers/libdeflate),
which is MIT licensed. Its copyright notices (Eric Biggers, and Google LLC)
are kept in [LICENSE-MIT](LICENSE-MIT) and apply to the ported code
whichever license you choose.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
