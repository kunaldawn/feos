//! Dependency-free, allocation-free archive readers and writers.
//!
//! `feos-archive` understands ZIP, TAR (ustar, GNU and PAX) and gzip. All data
//! is borrowed from the input slice and all output goes to caller-provided
//! buffers, so the crate works in a `no_std` kernel without an allocator.
//!
//! ```no_run
//! let data: &[u8] = &[];
//! if feos_archive::detect(data) == Some(feos_archive::Format::Zip) {
//!     let mut reader = feos_archive::zip::ZipReader::open(data).unwrap();
//!     while let Some(entry) = reader.next().unwrap() {
//!         let mut out = [0u8; 4096];
//!         let len = entry.extract(&mut out).unwrap();
//!         let _ = &out[..len];
//!     }
//! }
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod error;
pub mod tar;
pub mod zip;

pub use error::{ArchiveError, Result};
pub use feos_compress::gzip;
pub use tar::{Entry, EntryType, TarReader, TarWriter};
pub use zip::{ZipEntry, ZipReader, ZipWriter};

/// Archive container formats recognised by [`detect`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// A ZIP archive.
    Zip,
    /// An uncompressed TAR archive.
    Tar,
    /// A gzip-compressed stream.
    Gzip,
}

/// Guesses the archive format from its leading bytes.
pub fn detect(data: &[u8]) -> Option<Format> {
    if data.starts_with(&[0x50, 0x4b, 0x03, 0x04]) || data.starts_with(&[0x50, 0x4b, 0x05, 0x06]) {
        return Some(Format::Zip);
    }
    if data.starts_with(&[0x1f, 0x8b]) {
        return Some(Format::Gzip);
    }
    if data.len() >= 512 {
        if &data[257..262] == b"ustar" {
            return Some(Format::Tar);
        }
        if tar::TarReader::new(data)
            .next()
            .map(|entry| entry.is_some())
            .unwrap_or(false)
        {
            return Some(Format::Tar);
        }
    }
    None
}
