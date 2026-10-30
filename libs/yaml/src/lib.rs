//! A small, allocation-free YAML parser and writer.
//!
//! `feos-yaml` implements the practical subset of YAML used by configuration
//! files: block mappings and sequences, flow collections, plain and quoted
//! scalars, literal/folded block scalars, comments, and document markers.
//! Anchors, aliases, tags, and multiple documents are rejected explicitly
//! rather than being silently misparsed.
//!
//! Like the other feos libraries, parsing borrows the input and writes nodes
//! into a caller-provided arena. Nothing is allocated.
//!
//! ```
//! use feos_yaml::{YamlDocument, YamlNode};
//!
//! let mut nodes = [YamlNode::EMPTY; 32];
//! let doc = YamlDocument::parse(
//!     "name: feos\nfeatures:\n  - json\n  - yaml\n",
//!     &mut nodes,
//! )
//! .unwrap();
//! let root = doc.root();
//! let mut text = [0u8; 32];
//! assert_eq!(doc.as_str(doc.get(root, "name").unwrap(), &mut text), Some("feos"));
//! let features = doc.get(root, "features").unwrap();
//! assert_eq!(doc.len(features), Some(2));
//! ```
//!
//! # Scalar typing
//!
//! YAML scalars are untyped text. The parser keeps their style and can decode
//! escapes or block indentation on demand. `as_bool`, `as_i64`, and `as_f64`
//! apply the usual YAML core-schema conversions and return `None` when the
//! text does not match.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod document;
mod emit;
mod error;
mod parser;
mod scalar;

pub use document::{Kind, MappingIter, NodeId, SequenceIter, YamlDocument, YamlNode, NO_NODE};
pub use error::{Result, YamlError};
pub use scalar::{Chomp, ScalarStyle, YamlScalar};

/// Maximum nesting depth accepted by the parser.
pub const MAX_DEPTH: usize = 64;
