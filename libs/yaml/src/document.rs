//! Parsed YAML documents and accessors.

use crate::{parser, Result, YamlScalar};

/// A parsed YAML document backed by a caller-provided node arena.
pub struct YamlDocument<'a, 's> {
    nodes: &'s [YamlNode<'a>],
    root: NodeId,
}

/// A node in a [`YamlDocument`] arena.
#[derive(Clone, Copy)]
pub struct YamlNode<'a> {
    pub(crate) kind: Kind<'a>,
    pub(crate) next: NodeId,
    pub(crate) first: NodeId,
    pub(crate) key: Option<YamlScalar<'a>>,
}

/// A node id.
pub type NodeId = u32;

/// Sentinel used for an empty child list.
pub const NO_NODE: NodeId = u32::MAX;

/// The kind of a YAML node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind<'a> {
    /// A scalar value.
    Scalar(YamlScalar<'a>),
    /// A block or flow sequence.
    Sequence,
    /// A block or flow mapping.
    Mapping,
}

impl<'a> YamlNode<'a> {
    /// An empty arena slot.
    pub const EMPTY: Self = Self {
        kind: Kind::Scalar(YamlScalar::plain("")),
        next: NO_NODE,
        first: NO_NODE,
        key: None,
    };
}

impl<'a, 's> YamlDocument<'a, 's> {
    /// Parses `input`, filling `nodes`, and returns a document over the arena.
    pub fn parse(input: &'a str, nodes: &'s mut [YamlNode<'a>]) -> Result<Self> {
        let root = parser::parse(input, nodes)?;
        Ok(Self { nodes, root })
    }

    /// The document root.
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// The node with `id`.
    pub fn node(&self, id: NodeId) -> Option<&'s YamlNode<'a>> {
        self.nodes.get(id as usize)
    }

    /// The kind of `id`.
    pub fn kind(&self, id: NodeId) -> Option<Kind<'a>> {
        self.node(id).map(|node| node.kind)
    }

    /// The scalar at `id`.
    pub fn scalar(&self, id: NodeId) -> Option<YamlScalar<'a>> {
        match self.node(id)?.kind {
            Kind::Scalar(scalar) => Some(scalar),
            _ => None,
        }
    }

    /// The value of a scalar that needs no decoding.
    pub fn as_str_raw(&self, id: NodeId) -> Option<&'a str> {
        self.scalar(id)?.as_borrowed()
    }

    /// Decodes a scalar into `out`.
    pub fn as_str<'b>(&self, id: NodeId, out: &'b mut [u8]) -> Option<&'b str> {
        self.scalar(id)?.decode_into(out)
    }

    /// Returns `true` for a null scalar (`null`, `Null`, `NULL`, `~`, or empty).
    pub fn is_null(&self, id: NodeId) -> bool {
        let Some(scalar) = self.scalar(id) else {
            return false;
        };
        if scalar.style() == crate::ScalarStyle::Plain {
            let raw = scalar.raw().trim();
            return raw.is_empty() || raw == "~" || raw.eq_ignore_ascii_case("null");
        }
        false
    }

    /// A boolean scalar (`true`/`false`, with YAML 1.1 yes/no accepted).
    pub fn as_bool(&self, id: NodeId) -> Option<bool> {
        let mut buffer = [0u8; 16];
        let text = self.as_str(id, &mut buffer)?;
        if text.eq_ignore_ascii_case("true")
            || text.eq_ignore_ascii_case("yes")
            || text.eq_ignore_ascii_case("on")
        {
            Some(true)
        } else if text.eq_ignore_ascii_case("false")
            || text.eq_ignore_ascii_case("no")
            || text.eq_ignore_ascii_case("off")
        {
            Some(false)
        } else {
            None
        }
    }

    /// Parses a scalar as `i64`.
    pub fn as_i64(&self, id: NodeId) -> Option<i64> {
        self.parse_number(id)?.parse().ok()
    }

    /// Parses a scalar as `u64`.
    pub fn as_u64(&self, id: NodeId) -> Option<u64> {
        self.parse_number(id)?.parse().ok()
    }

    /// Parses a scalar as `f64`.
    pub fn as_f64(&self, id: NodeId) -> Option<f64> {
        self.parse_number(id)?.parse().ok()
    }

    fn parse_number(&self, id: NodeId) -> Option<&str> {
        // Parsing with underscores requires a temporary buffer. Keep the
        // common case zero-copy.
        let scalar = self.scalar(id)?;
        if let Some(raw) = scalar.as_borrowed() {
            if !raw.contains('_') {
                return Some(raw.trim());
            }
        }
        None
    }

    /// Returns `true` for a sequence.
    pub fn is_sequence(&self, id: NodeId) -> bool {
        matches!(self.node(id).map(|node| node.kind), Some(Kind::Sequence))
    }

    /// Returns `true` for a mapping.
    pub fn is_mapping(&self, id: NodeId) -> bool {
        matches!(self.node(id).map(|node| node.kind), Some(Kind::Mapping))
    }

    /// Number of sequence items or mapping entries.
    pub fn len(&self, id: NodeId) -> Option<usize> {
        match self.node(id)?.kind {
            Kind::Sequence | Kind::Mapping => {
                let mut count = 0usize;
                let mut child = self.node(id)?.first;
                while child != NO_NODE {
                    count += 1;
                    child = self.node(child)?.next;
                }
                Some(count)
            }
            _ => None,
        }
    }

    /// Returns `true` when a sequence or mapping is empty.
    pub fn is_empty(&self, id: NodeId) -> Option<bool> {
        Some(self.len(id)? == 0)
    }

    /// The sequence item at `index`.
    pub fn at(&self, id: NodeId, index: usize) -> Option<NodeId> {
        self.sequence_iter(id)?.nth(index)
    }

    /// Looks up a mapping key.
    pub fn get(&self, id: NodeId, key: &str) -> Option<NodeId> {
        if !self.is_mapping(id) {
            return None;
        }
        let mut child = self.node(id)?.first;
        while child != NO_NODE {
            let node = self.node(child)?;
            if let Some(node_key) = node.key {
                if self.scalar_matches(node_key, key) {
                    return Some(child);
                }
            }
            child = node.next;
        }
        None
    }

    fn scalar_matches(&self, scalar: YamlScalar<'a>, needle: &str) -> bool {
        if let Some(raw) = scalar.as_borrowed() {
            raw == needle
        } else {
            let mut buffer = [0u8; 256];
            scalar.decode_into(&mut buffer) == Some(needle)
        }
    }

    /// Iterates over sequence items.
    pub fn sequence_iter(&self, id: NodeId) -> Option<SequenceIter<'_, 'a, 's>> {
        if !self.is_sequence(id) {
            return None;
        }
        Some(SequenceIter {
            doc: self,
            next: self.node(id)?.first,
        })
    }

    /// Iterates over mapping entries.
    pub fn mapping_iter(&self, id: NodeId) -> Option<MappingIter<'_, 'a, 's>> {
        if !self.is_mapping(id) {
            return None;
        }
        Some(MappingIter {
            doc: self,
            next: self.node(id)?.first,
        })
    }

    /// Writes the document in block style.
    pub fn encode_to<W: core::fmt::Write>(&self, id: NodeId, out: &mut W) -> core::fmt::Result {
        crate::emit::write_yaml(self, id, out)
    }
}

/// Iterator over sequence items.
pub struct SequenceIter<'d, 'a, 's> {
    doc: &'d YamlDocument<'a, 's>,
    next: NodeId,
}

impl Iterator for SequenceIter<'_, '_, '_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == NO_NODE {
            return None;
        }
        let id = self.next;
        self.next = self.doc.node(id)?.next;
        Some(id)
    }
}

/// Iterator over mapping entries.
pub struct MappingIter<'d, 'a, 's> {
    doc: &'d YamlDocument<'a, 's>,
    next: NodeId,
}

impl<'a> Iterator for MappingIter<'_, 'a, '_> {
    type Item = (YamlScalar<'a>, NodeId);

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == NO_NODE {
            return None;
        }
        let id = self.next;
        let node = self.doc.node(id)?;
        self.next = node.next;
        node.key.map(|key| (key, id))
    }
}
