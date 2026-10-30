//! Archive errors.

use core::fmt;
use feos_compress::CompressError;

/// Errors returned by the archive readers and writers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchiveError {
    /// The input ended before the archive structure was complete.
    UnexpectedEnd,
    /// The archive is malformed.
    InvalidData(&'static str),
    /// A valid archive uses a feature that is not implemented.
    UnsupportedFeature(&'static str),
    /// A CRC or size check did not match.
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
    /// An embedded compressed stream failed to decode.
    Compression(CompressError),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of archive"),
            Self::InvalidData(what) => write!(f, "invalid archive: {what}"),
            Self::UnsupportedFeature(feature) => {
                write!(f, "unsupported archive feature: {feature}")
            }
            Self::Checksum => write!(f, "archive checksum mismatch"),
            Self::BufferTooSmall {
                required,
                available,
            } => write!(
                f,
                "buffer too small: need {required} bytes, have {available}"
            ),
            Self::Overflow => write!(f, "archive size overflow"),
            Self::Compression(error) => write!(f, "compressed member: {error}"),
        }
    }
}

impl From<CompressError> for ArchiveError {
    fn from(error: CompressError) -> Self {
        match error {
            CompressError::BufferTooSmall {
                required,
                available,
            } => Self::BufferTooSmall {
                required,
                available,
            },
            CompressError::UnexpectedEnd => Self::UnexpectedEnd,
            CompressError::Overflow => Self::Overflow,
            CompressError::Checksum => Self::Checksum,
            other => Self::Compression(other),
        }
    }
}

/// Result type used throughout the crate.
pub type Result<T> = core::result::Result<T, ArchiveError>;

pub(crate) fn need(len: usize, index: usize, n: usize) -> Result<()> {
    if index.checked_add(n).is_none_or(|end| end > len) {
        Err(ArchiveError::UnexpectedEnd)
    } else {
        Ok(())
    }
}

pub(crate) fn buffer_error(required: usize, available: usize) -> ArchiveError {
    ArchiveError::BufferTooSmall {
        required,
        available,
    }
}
