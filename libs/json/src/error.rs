//! JSON error reporting.

use core::fmt;

/// Errors returned by the parser and writer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsonError {
    /// The input ended in the middle of a value.
    UnexpectedEnd,
    /// A byte is not valid at this position.
    UnexpectedByte {
        /// Byte offset into the input.
        offset: usize,
        /// The offending byte.
        byte: u8,
    },
    /// A number does not follow the JSON grammar.
    InvalidNumber {
        /// Byte offset of the number.
        offset: usize,
    },
    /// A string escape is invalid.
    InvalidEscape {
        /// Byte offset of the escape.
        offset: usize,
    },
    /// A `\u` sequence is not a valid Unicode scalar value.
    InvalidUnicode {
        /// Byte offset of the escape.
        offset: usize,
    },
    /// A control character appeared unescaped in a string.
    ControlCharacter {
        /// Byte offset of the character.
        offset: usize,
    },
    /// Nesting exceeded [`MAX_DEPTH`](crate::MAX_DEPTH).
    DepthLimit,
    /// The node arena is too small for the document.
    NodeLimit,
    /// Data follows the root value.
    TrailingData {
        /// Byte offset of the trailing data.
        offset: usize,
    },
    /// A borrowed output buffer is too small.
    BufferTooSmall,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of input"),
            Self::UnexpectedByte { offset, byte } => {
                write!(f, "unexpected byte {byte:#04x} at offset {offset}")
            }
            Self::InvalidNumber { offset } => write!(f, "invalid number at offset {offset}"),
            Self::InvalidEscape { offset } => write!(f, "invalid escape at offset {offset}"),
            Self::InvalidUnicode { offset } => {
                write!(f, "invalid Unicode escape at offset {offset}")
            }
            Self::ControlCharacter { offset } => {
                write!(f, "unescaped control character at offset {offset}")
            }
            Self::DepthLimit => write!(f, "nesting depth limit exceeded"),
            Self::NodeLimit => write!(f, "JSON node arena is full"),
            Self::TrailingData { offset } => write!(f, "trailing data at offset {offset}"),
            Self::BufferTooSmall => write!(f, "output buffer is too small"),
        }
    }
}

/// Result type used throughout the crate.
pub type Result<T> = core::result::Result<T, JsonError>;
