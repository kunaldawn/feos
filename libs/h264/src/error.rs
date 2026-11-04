//! Error type returned by the decoder.

use core::fmt;

/// Errors produced by the H.264 decoder.
///
/// Returned by [`Decoder::decode_nal`](crate::decoder::Decoder::decode_nal)
/// when a NAL unit cannot be parsed or uses an unsupported feature.
///
/// Implements [`Display`](fmt::Display) without requiring the standard
/// library, so callers can use it in both native and `no_std` environments.
#[derive(Debug)]
pub enum DecodeError {
    /// The bitstream ended in the middle of a syntax element. Usually
    /// indicates a truncated NAL unit or a parser bug.
    UnexpectedEof,
    /// A syntactic element in the bitstream has an invalid value (out of
    /// range, reserved value, etc.). Usually indicates a malformed bitstream.
    InvalidSyntax(&'static str),
    /// The stream uses an H.264 feature this decoder does not yet implement
    /// (e.g. interlaced coding, High 10/4:2:2/4:4:4 profiles, SP/SI slices,
    /// slice groups / FMO).
    Unsupported(&'static str),
    /// The stream needs more memory than the configured
    /// [`Limits`](crate::decoder::Limits) allow, or an allocation failed.
    OutOfMemory,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEof => write!(f, "unexpected end of bitstream"),
            DecodeError::InvalidSyntax(msg) => write!(f, "invalid syntax: {}", msg),
            DecodeError::Unsupported(msg) => write!(f, "unsupported: {}", msg),
            DecodeError::OutOfMemory => write!(f, "not enough memory for this stream"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Allow `?` to convert `&'static str` errors from internal parsing functions
/// into `DecodeError` automatically.
impl From<&'static str> for DecodeError {
    fn from(msg: &'static str) -> Self {
        if msg == "end of bitstream" {
            DecodeError::UnexpectedEof
        } else if msg.contains("not yet supported")
            || msg.contains("not supported")
            || msg.starts_with("only ")
            || msg.starts_with("unsupported")
        {
            DecodeError::Unsupported(msg)
        } else {
            DecodeError::InvalidSyntax(msg)
        }
    }
}
