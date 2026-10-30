//! Dependency-free, allocation-free compression primitives.
//!
//! The crate implements raw DEFLATE (RFC 1951), zlib (RFC 1950) and gzip
//! (RFC 1952) using caller-provided input and output slices. Decompression
//! writes into `out` and uses the already-written bytes as the LZ77 window, so
//! the decompressed stream must fit in the provided buffer.
//!
//! ```no_run
//! let mut compressed = [0u8; 128];
//! let len = feos_compress::gzip::compress(b"hello", &mut compressed).unwrap();
//! let mut plain = [0u8; 64];
//! let written = feos_compress::gzip::decompress(&compressed[..len], &mut plain).unwrap();
//! assert_eq!(&plain[..written], b"hello");
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod checksum;
pub mod deflate;
mod error;
pub mod gzip;
pub mod inflate;
pub mod zlib;

pub use checksum::{adler32, crc32, Crc32};
pub use deflate::{compress as deflate_compress, Deflater};
pub use error::{CompressError, Result};
pub use inflate::{inflate, inflate_with_consumed, InflateResult};
