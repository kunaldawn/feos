//! Video decoding errors.

use core::fmt;

/// Errors returned while demuxing or decoding a video.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoError {
    /// The input ended before a complete structure was available.
    UnexpectedEnd,
    /// The input is not a container or elementary stream we recognise.
    UnknownFormat,
    /// The container is known but this build does not implement it.
    UnsupportedFormat,
    /// A valid stream used a feature that is not implemented.
    UnsupportedFeature(&'static str),
    /// The stream uses a codec whose pixels we cannot reconstruct yet.
    UnsupportedCodec(&'static str),
    /// The file is malformed.
    InvalidData(&'static str),
    /// A caller-provided buffer is too small.
    BufferTooSmall {
        /// Bytes required.
        required: usize,
        /// Bytes available.
        available: usize,
    },
    /// The requested frame is past the end of the stream.
    FrameOutOfRange {
        /// Requested frame index.
        index: u32,
        /// Number of frames known to exist.
        count: u32,
    },
    /// A conversion or index computation overflowed `usize`.
    Overflow,
    /// The video needs more memory than is available.
    OutOfMemory,
    /// The video exceeds a configured limit.
    LimitExceeded(&'static str),
}

impl VideoError {
    /// Whether later frames of the same video can't succeed either. Other
    /// errors concern one damaged frame, and playback can carry on past it.
    pub const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::UnknownFormat
                | Self::UnsupportedFormat
                | Self::UnsupportedFeature(_)
                | Self::UnsupportedCodec(_)
                | Self::OutOfMemory
                | Self::LimitExceeded(_)
                | Self::Overflow
        )
    }
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of video data"),
            Self::UnknownFormat => write!(f, "unknown video format"),
            Self::UnsupportedFormat => write!(f, "unsupported video container"),
            Self::UnsupportedFeature(feature) => {
                write!(f, "unsupported video feature: {feature}")
            }
            Self::UnsupportedCodec(codec) => {
                write!(f, "unsupported video codec: {codec}")
            }
            Self::InvalidData(what) => write!(f, "invalid video data: {what}"),
            Self::BufferTooSmall {
                required,
                available,
            } => write!(
                f,
                "buffer too small: need {required} bytes, have {available}"
            ),
            Self::FrameOutOfRange { index, count } => {
                write!(f, "frame {index} out of range (stream has {count} frames)")
            }
            Self::Overflow => write!(f, "video dimensions overflow"),
            Self::OutOfMemory => write!(f, "not enough memory for this video"),
            Self::LimitExceeded(what) => write!(f, "video too large: {what}"),
        }
    }
}

/// Result type used throughout the crate.
pub type Result<T> = core::result::Result<T, VideoError>;

impl From<feos_image::ImageError> for VideoError {
    fn from(error: feos_image::ImageError) -> Self {
        use feos_image::ImageError;
        match error {
            ImageError::UnexpectedEnd => Self::UnexpectedEnd,
            ImageError::UnknownFormat => Self::UnknownFormat,
            ImageError::UnsupportedFormat => Self::UnsupportedFormat,
            ImageError::UnsupportedFeature(feature) => Self::UnsupportedFeature(feature),
            ImageError::InvalidData(what) => Self::InvalidData(what),
            ImageError::BufferTooSmall {
                required,
                available,
            } => Self::BufferTooSmall {
                required,
                available,
            },
            ImageError::Checksum => Self::InvalidData("image checksum"),
            ImageError::Overflow => Self::Overflow,
        }
    }
}

impl From<feos_h264::DecodeError> for VideoError {
    fn from(error: feos_h264::DecodeError) -> Self {
        use feos_h264::DecodeError;
        match error {
            DecodeError::UnexpectedEof => Self::UnexpectedEnd,
            DecodeError::InvalidSyntax(what) => Self::InvalidData(what),
            DecodeError::Unsupported(what) => Self::UnsupportedFeature(what),
            DecodeError::OutOfMemory => Self::OutOfMemory,
        }
    }
}

/// Helper for checking that a slice has at least `n` remaining bytes.
pub(crate) fn need(len: usize, index: usize, n: usize) -> Result<()> {
    if index.checked_add(n).is_none_or(|end| end > len) {
        Err(VideoError::UnexpectedEnd)
    } else {
        Ok(())
    }
}

/// Checks that a caller buffer can hold one RGBA frame.
pub(crate) fn require_rgba(out: &[u8], width: u32, height: u32) -> Result<usize> {
    let required = crate::pixel::rgba_len(width, height)?;
    if out.len() < required {
        return Err(VideoError::BufferTooSmall {
            required,
            available: out.len(),
        });
    }
    Ok(required)
}
