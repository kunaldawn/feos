//! Audio decoding errors.

use core::fmt;

/// Errors returned while decoding or encoding audio.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioError {
    /// The input ended before a complete header or frame was available.
    UnexpectedEnd,
    /// The container signature is not recognised.
    UnknownFormat,
    /// The container is known but not implemented.
    UnsupportedFormat,
    /// A valid file uses an unsupported feature.
    UnsupportedFeature(&'static str),
    /// The file is malformed.
    InvalidData(&'static str),
    /// A caller-provided buffer is too small.
    BufferTooSmall {
        /// Elements required.
        required: usize,
        /// Elements available.
        available: usize,
    },
    /// An integer overflowed the platform's `usize`.
    Overflow,
    /// A working buffer could not be allocated. The stream may be fine; the
    /// allocation is refused rather than aborting the kernel.
    OutOfMemory,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of audio data"),
            Self::UnknownFormat => write!(f, "unknown audio format"),
            Self::UnsupportedFormat => write!(f, "unsupported audio format"),
            Self::UnsupportedFeature(feature) => {
                write!(f, "unsupported audio feature: {feature}")
            }
            Self::InvalidData(what) => write!(f, "invalid audio data: {what}"),
            Self::BufferTooSmall {
                required,
                available,
            } => write!(
                f,
                "buffer too small: need {required} elements, have {available}"
            ),
            Self::Overflow => write!(f, "audio size overflow"),
            Self::OutOfMemory => write!(f, "out of memory while decoding audio"),
        }
    }
}

/// Result type used throughout the crate.
pub type Result<T> = core::result::Result<T, AudioError>;

pub(crate) fn need(len: usize, index: usize, n: usize) -> Result<()> {
    if index.checked_add(n).is_none_or(|end| end > len) {
        Err(AudioError::UnexpectedEnd)
    } else {
        Ok(())
    }
}
