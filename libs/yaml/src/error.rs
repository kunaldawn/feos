//! YAML error reporting.

use core::fmt;

/// Errors returned by the YAML parser.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum YamlError {
    /// Input ended in the middle of a construct.
    UnexpectedEnd,
    /// A tab appeared in indentation.
    TabIndentation {
        /// Byte offset of the tab.
        offset: usize,
    },
    /// A line is indented inconsistently.
    InvalidIndentation {
        /// Byte offset of the line.
        offset: usize,
    },
    /// A token was not understood.
    UnexpectedToken {
        /// Byte offset of the token.
        offset: usize,
        /// The token byte.
        byte: u8,
    },
    /// A mapping entry has no colon.
    MissingColon {
        /// Byte offset of the line.
        offset: usize,
    },
    /// A flow collection was not closed.
    UnterminatedFlow {
        /// Byte offset where the collection started.
        offset: usize,
    },
    /// A quoted scalar was not closed.
    UnterminatedQuote {
        /// Byte offset of the opening quote.
        offset: usize,
    },
    /// A backslash escape is invalid.
    InvalidEscape {
        /// Byte offset of the escape.
        offset: usize,
    },
    /// Nesting exceeded [`MAX_DEPTH`](crate::MAX_DEPTH).
    DepthLimit,
    /// The node arena is too small.
    NodeLimit,
    /// More than one document was supplied.
    MultipleDocuments,
    /// An anchor alias was used. Aliases are not supported.
    UnsupportedAlias {
        /// Byte offset of the alias.
        offset: usize,
    },
    /// A buffer supplied by the caller is too small.
    BufferTooSmall,
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of input"),
            Self::TabIndentation { offset } => {
                write!(f, "tab in indentation at offset {offset}")
            }
            Self::InvalidIndentation { offset } => {
                write!(f, "invalid indentation at offset {offset}")
            }
            Self::UnexpectedToken { offset, byte } => {
                write!(f, "unexpected byte {byte:#04x} at offset {offset}")
            }
            Self::MissingColon { offset } => write!(f, "missing ':' at offset {offset}"),
            Self::UnterminatedFlow { offset } => {
                write!(f, "unterminated flow collection at offset {offset}")
            }
            Self::UnterminatedQuote { offset } => {
                write!(f, "unterminated quoted scalar at offset {offset}")
            }
            Self::InvalidEscape { offset } => write!(f, "invalid escape at offset {offset}"),
            Self::DepthLimit => write!(f, "nesting depth limit exceeded"),
            Self::NodeLimit => write!(f, "YAML node arena is full"),
            Self::MultipleDocuments => write!(f, "multiple YAML documents are not supported"),
            Self::UnsupportedAlias { offset } => {
                write!(f, "anchors and aliases are not supported (offset {offset})")
            }
            Self::BufferTooSmall => write!(f, "output buffer is too small"),
        }
    }
}

/// Result type used throughout the crate.
pub type Result<T> = core::result::Result<T, YamlError>;
