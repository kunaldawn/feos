//! Compression errors.

use core::fmt;

/// Errors returned by the compression and decompression routines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompressError {
    /// The input ended before the stream was complete.
    UnexpectedEnd,
    /// The stream is malformed.
    InvalidData(&'static str),
    /// A valid stream uses a feature that is not implemented.
    UnsupportedFeature(&'static str),
    /// A CRC or Adler-32 checksum did not match.
    Checksum,
    /// A caller-provided buffer is too small.
    BufferTooSmall {
        /// Bytes required.
        required: usize,
        /// Bytes available.
        available: usize,
    },
    /// A size overflowed `usize`.
    Overflow,
}

impl fmt::Display for CompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of compressed data"),
            Self::InvalidData(what) => write!(f, "invalid compressed data: {what}"),
            Self::UnsupportedFeature(feature) => {
                write!(f, "unsupported compression feature: {feature}")
            }
            Self::Checksum => write!(f, "checksum mismatch"),
            Self::BufferTooSmall {
                required,
                available,
            } => write!(
                f,
                "buffer too small: need {required} bytes, have {available}"
            ),
            Self::Overflow => write!(f, "compressed size overflow"),
        }
    }
}

/// Result type used throughout the crate.
pub type Result<T> = core::result::Result<T, CompressError>;

pub(crate) fn need(len: usize, index: usize, n: usize) -> Result<()> {
    if index.checked_add(n).is_none_or(|end| end > len) {
        Err(CompressError::UnexpectedEnd)
    } else {
        Ok(())
    }
}
