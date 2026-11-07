//! A small, allocation-free JSON parser and writer.
//!
//! `feos-json` follows RFC 8259. It is the data-format counterpart of
//! `feos-gfx`: the caller supplies the node arena, and every string and number
//! is borrowed from the input document. Nothing is allocated and the crate is
//! `no_std`.
//!
//! ```
//! use feos_json::{JsonDocument, JsonNode};
//!
//! let mut nodes = [JsonNode::EMPTY; 32];
//! let doc = JsonDocument::parse(r#"{"name":"feos","ok":true}"#, &mut nodes).unwrap();
//! let root = doc.root();
//! let name = doc.get(root, "name").unwrap();
//! assert_eq!(doc.as_str_raw(name), Some("feos"));
//! assert_eq!(doc.as_bool(doc.get(root, "ok").unwrap()), Some(true));
//! ```
//!
//! # Strings and escapes
//!
//! The parser stores strings as borrowed source spans. A span that contains
//! no backslash is already the decoded value and can be used with
//! [`JsonDocument::as_str_raw`]. Strings with escapes can be decoded into a
//! caller-provided buffer with [`JsonDocument::as_str`] or
//! [`JsonString::decode_into`].
//!
//! # Limits
//!
//! Nesting is limited to [`MAX_DEPTH`] levels and the arena is finite. Both
//! limits return an error rather than growing or aborting. Duplicate object
//! keys are accepted; lookup returns the first match.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod builder;
mod document;
mod encode;
mod error;
mod parser;
mod string;

pub use builder::{BuildError, JsonBuilder, SliceOutput};
pub use document::{JsonDocument, JsonNode};
pub use encode::{write_json, write_json_pretty};
pub use error::{JsonError, Result};
pub use string::JsonString;

/// Maximum nesting depth accepted by the parser.
pub const MAX_DEPTH: usize = 96;

/// A node slot in a [`JsonDocument`].
///
/// Slots are opaque; use the accessors on [`JsonDocument`].
pub type NodeId = u32;

/// Sentinel returned when a child list is empty or a lookup fails.
pub const NO_NODE: NodeId = u32::MAX;

/// The kind of a JSON value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind<'a> {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number in its original source form.
    Number(&'a str),
    /// A string span.
    String(JsonString<'a>),
    /// An array; iterate with [`JsonDocument::array_iter`].
    Array,
    /// An object; iterate with [`JsonDocument::object_iter`].
    Object,
}
