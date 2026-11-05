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

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Turns a member's stored name into a relative path that can't leave the
/// directory it's extracted into.
///
/// Archives are untrusted, and every extractor needs this check, so it lives
/// here. A leading `/` goes, as `tar` conventionally strips it; `.`, empty parts
/// and backslashes (a Windows archiver's separator) are normalised. Returns `None`
/// for anything that would escape (`..`) or that names no file: control
/// characters, a drive prefix, or nothing at all.
pub fn safe_path(name: &str) -> Option<String> {
    let mut path = String::new();
    for (index, part) in name.split(['/', '\\']).enumerate() {
        match part {
            "" | "." => continue,
            ".." => return None,
            _ => {}
        }
        if part.chars().any(char::is_control) || (index == 0 && part.ends_with(':')) {
            return None;
        }
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(part);
    }
    (!path.is_empty()).then_some(path)
}

/// Unpacks a `.tar.gz` into memory, ready for [`TarReader`], refusing to grow past
/// `limit` bytes. A small file can claim to inflate to any size, and this finds
/// out by trying, not by believing it.
pub fn gunzip_tar(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut window = Vec::new();
    window
        .try_reserve_exact(feos_compress::WINDOW_SIZE)
        .map_err(|_| ArchiveError::Overflow)?;
    window.resize(feos_compress::WINDOW_SIZE, 0);
    let mut rest = data;
    let mut input = |buffer: &mut [u8]| -> feos_compress::Result<usize> {
        let take = buffer.len().min(rest.len());
        buffer[..take].copy_from_slice(&rest[..take]);
        rest = &rest[take..];
        Ok(take)
    };
    let mut out = Vec::new();
    let mut output = |chunk: &[u8]| -> feos_compress::Result<()> {
        let fits = out.len() + chunk.len() <= limit && out.try_reserve(chunk.len()).is_ok();
        if !fits {
            return Err(feos_compress::CompressError::BufferTooSmall {
                required: out.len() + chunk.len(),
                available: limit,
            });
        }
        out.extend_from_slice(chunk);
        Ok(())
    };
    feos_compress::gzip::decompress_stream(&mut input, &mut window, &mut output)?;
    Ok(out)
}

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

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn safe_path_keeps_inside_and_refuses_the_rest() {
        for (name, expected) in [
            ("a/b.txt", Some("a/b.txt")),
            ("/etc/motd", Some("etc/motd")),
            ("./a//b/./c", Some("a/b/c")),
            ("dir\\file.txt", Some("dir/file.txt")),
            ("folder/", Some("folder")),
            ("../evil", None),
            ("a/../../evil", None),
            ("a\\..\\..\\evil", None),
            ("C:\\windows\\system.ini", None),
            ("name\u{1b}[2J", None),
            ("", None),
            ("/", None),
            ("./", None),
        ] {
            assert_eq!(safe_path(name).as_deref(), expected, "{name:?}");
        }
    }

    #[test]
    fn gunzip_tar_unpacks_within_its_limit() {
        let mut tar = std::vec![0u8; 8192];
        let mut writer = TarWriter::new(&mut tar);
        writer.write_file("a.txt", b"inside").unwrap();
        let len = writer.finish().unwrap();
        let mut packed = std::vec![0u8; len + 1024];
        let packed_len = gzip::compress(&tar[..len], &mut packed).unwrap();
        packed.truncate(packed_len);

        let unpacked = gunzip_tar(&packed, 1 << 20).unwrap();
        let mut reader = TarReader::new(&unpacked);
        assert_eq!(reader.next().unwrap().unwrap().data, b"inside");
        assert!(gunzip_tar(&packed, 100).is_err());
    }
}
