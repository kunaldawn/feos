//! Image decoding errors.

use core::fmt;

/// Errors returned while decoding or encoding an image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageError {
    /// The input ended before a complete image was available.
    UnexpectedEnd,
    /// The input does not start with a signature we recognise.
    UnknownFormat,
    /// The format is known but this build does not implement it.
    UnsupportedFormat,
    /// A valid file used a feature that is not implemented (for example a
    /// progressive JPEG).
    UnsupportedFeature(&'static str),
    /// The file is malformed.
    InvalidData(&'static str),
    /// A caller-provided buffer is too small.
    BufferTooSmall {
        /// Bytes required.
        required: usize,
        /// Bytes available.
        available: usize,
    },
    /// A checksum (CRC, Adler-32) did not match.
    Checksum,
    /// The requested integer overflowed the platform's `usize`.
    Overflow,
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of image data"),
            Self::UnknownFormat => write!(f, "unknown image format"),
            Self::UnsupportedFormat => write!(f, "unsupported image format"),
            Self::UnsupportedFeature(feature) => {
                write!(f, "unsupported image feature: {feature}")
            }
            Self::InvalidData(what) => write!(f, "invalid image data: {what}"),
            Self::BufferTooSmall {
                required,
                available,
            } => write!(
                f,
                "buffer too small: need {required} bytes, have {available}"
            ),
            Self::Checksum => write!(f, "checksum mismatch"),
            Self::Overflow => write!(f, "image dimensions overflow"),
        }
    }
}

/// Result type used throughout the crate.
pub type Result<T> = core::result::Result<T, ImageError>;

impl From<feos_compress::CompressError> for ImageError {
    fn from(error: feos_compress::CompressError) -> Self {
        use feos_compress::CompressError;
        match error {
            CompressError::UnexpectedEnd => Self::UnexpectedEnd,
            CompressError::InvalidData(what) => Self::InvalidData(what),
            CompressError::UnsupportedFeature(feature) => Self::UnsupportedFeature(feature),
            CompressError::Checksum => Self::Checksum,
            CompressError::BufferTooSmall {
                required,
                available,
            } => Self::BufferTooSmall {
                required,
                available,
            },
            CompressError::Overflow => Self::Overflow,
        }
    }
}

/// Helper for checking that a slice has at least `n` remaining bytes.
pub(crate) fn need(len: usize, index: usize, n: usize) -> Result<()> {
    if index.checked_add(n).is_none_or(|end| end > len) {
        Err(ImageError::UnexpectedEnd)
    } else {
        Ok(())
    }
}
