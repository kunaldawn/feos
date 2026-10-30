//! Borrowed JSON documents and accessors.

use crate::{parser, JsonString, Kind, NodeId, NO_NODE};

/// A parsed JSON document backed by a caller-provided node arena.
pub struct JsonDocument<'a, 's> {
    nodes: &'s [JsonNode<'a>],
    root: NodeId,
}

/// One node in a [`JsonDocument`] arena.
///
/// Nodes are normally created by the parser. [`JsonNode::EMPTY`] exists so
/// callers can create an arena:
///
/// ```
/// use feos_json::{JsonDocument, JsonNode};
/// let mut arena = [JsonNode::EMPTY; 16];
/// let doc = JsonDocument::parse("null", &mut arena).unwrap();
/// assert!(doc.is_null(doc.root()));
/// ```
#[derive(Clone, Copy)]
pub struct JsonNode<'a> {
    pub(crate) kind: Kind<'a>,
    pub(crate) next: NodeId,
    pub(crate) first: NodeId,
    pub(crate) key: Option<JsonString<'a>>,
}

impl<'a> JsonNode<'a> {
    /// An empty arena slot.
    pub const EMPTY: Self = Self {
        kind: Kind::Null,
        next: NO_NODE,
        first: NO_NODE,
        key: None,
    };
}

impl<'a, 's> JsonDocument<'a, 's> {
    /// Parses `input`, filling `nodes`, and returns a document over the arena.
    ///
    /// The arena must contain one slot for the root plus one slot per array
    /// element, object member, and nested container.
    pub fn parse(input: &'a str, nodes: &'s mut [JsonNode<'a>]) -> crate::Result<Self> {
        let root = parser::parse(input, nodes)?;
        Ok(Self { nodes, root })
    }

    /// The root value.
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// Returns the node for `id`, or `None` when the id is out of range.
    pub fn node(&self, id: NodeId) -> Option<&'s JsonNode<'a>> {
        self.nodes.get(id as usize)
    }

    /// The kind of `id`.
    pub fn kind(&self, id: NodeId) -> Option<Kind<'a>> {
        self.node(id).map(|node| node.kind)
    }

    /// The source text of an escape-free string.
    ///
    /// Use [`as_str`](Self::as_str) for strings that contain escapes.
    pub fn as_str_raw(&self, id: NodeId) -> Option<&'a str> {
        match self.node(id)?.kind {
            Kind::String(string) => string.as_borrowed(),
            _ => None,
        }
    }

    /// Decodes a string into `out`, applying JSON escapes.
    pub fn as_str<'b>(&self, id: NodeId, out: &'b mut [u8]) -> Option<&'b str> {
        match self.node(id)?.kind {
            Kind::String(string) => string.decode_into(out),
            _ => None,
        }
    }

    /// The raw span of a string, including escapes.
    pub fn string(&self, id: NodeId) -> Option<JsonString<'a>> {
        match self.node(id)?.kind {
            Kind::String(string) => Some(string),
            _ => None,
        }
    }

    /// A boolean value.
    pub fn as_bool(&self, id: NodeId) -> Option<bool> {
        match self.node(id)?.kind {
            Kind::Bool(value) => Some(value),
            _ => None,
        }
    }

    /// The original source text of a number.
    pub fn number(&self, id: NodeId) -> Option<&'a str> {
        match self.node(id)?.kind {
            Kind::Number(text) => Some(text),
            _ => None,
        }
    }

    /// Parses a number as `i64`.
    pub fn as_i64(&self, id: NodeId) -> Option<i64> {
        self.number(id)?.parse().ok()
    }

    /// Parses a number as `u64`.
    pub fn as_u64(&self, id: NodeId) -> Option<u64> {
        self.number(id)?.parse().ok()
    }

    /// Parses a number as `f64`.
    pub fn as_f64(&self, id: NodeId) -> Option<f64> {
        self.number(id)?.parse().ok()
    }

    /// Returns `true` for `null`.
    pub fn is_null(&self, id: NodeId) -> bool {
        matches!(self.node(id).map(|node| node.kind), Some(Kind::Null))
    }

    /// Returns `true` for an array.
    pub fn is_array(&self, id: NodeId) -> bool {
        matches!(self.node(id).map(|node| node.kind), Some(Kind::Array))
    }

    /// Returns `true` for an object.
    pub fn is_object(&self, id: NodeId) -> bool {
        matches!(self.node(id).map(|node| node.kind), Some(Kind::Object))
    }

    /// Number of array elements or object members.
    pub fn len(&self, id: NodeId) -> Option<usize> {
        match self.node(id)?.kind {
            Kind::Array | Kind::Object => {
                let mut count = 0usize;
                let mut child = self.first_child(id)?;
                while child != NO_NODE {
                    count += 1;
                    child = self.node(child)?.next;
                }
                Some(count)
            }
            _ => None,
        }
    }

    /// Returns `true` when an array or object is empty.
    pub fn is_empty(&self, id: NodeId) -> Option<bool> {
        Some(self.len(id)? == 0)
    }

    fn first_child(&self, id: NodeId) -> Option<NodeId> {
        match self.node(id)?.kind {
            Kind::Array => Some(self.node(id)?.first),
            Kind::Object => Some(self.node(id)?.first),
            _ => None,
        }
    }

    /// The child at `index` in an array.
    pub fn at(&self, id: NodeId, index: usize) -> Option<NodeId> {
        self.array_iter(id)?.nth(index)
    }

    /// Looks up `key` in an object. The first duplicate wins.
    pub fn get(&self, id: NodeId, key: &str) -> Option<NodeId> {
        if !self.is_object(id) {
            return None;
        }
        let mut child = self.node(id)?.first;
        while child != NO_NODE {
            let node = self.node(child)?;
            if let Some(node_key) = node.key {
                if self.key_matches(node_key, key) {
                    return Some(child);
                }
            }
            child = node.next;
        }
        None
    }

    fn key_matches(&self, key: JsonString<'a>, needle: &str) -> bool {
        if let Some(raw) = key.as_borrowed() {
            raw == needle
        } else {
            let mut buffer = [0u8; 256];
            key.decode_into(&mut buffer) == Some(needle)
        }
    }

    /// Iterates over array elements.
    pub fn array_iter(&self, id: NodeId) -> Option<ArrayIter<'_, 'a, 's>> {
        if !self.is_array(id) {
            return None;
        }
        Some(ArrayIter {
            doc: self,
            next: self.node(id)?.first,
        })
    }

    /// Iterates over object members.
    pub fn object_iter(&self, id: NodeId) -> Option<ObjectIter<'_, 'a, 's>> {
        if !self.is_object(id) {
            return None;
        }
        Some(ObjectIter {
            doc: self,
            next: self.node(id)?.first,
        })
    }

    /// Writes this document as compact JSON.
    pub fn encode_to<W: core::fmt::Write>(&self, id: NodeId, out: &mut W) -> core::fmt::Result {
        crate::write_json(self, id, out)
    }

    /// Writes this document as indented JSON.
    pub fn encode_pretty_to<W: core::fmt::Write>(
        &self,
        id: NodeId,
        out: &mut W,
    ) -> core::fmt::Result {
        crate::write_json_pretty(self, id, out)
    }
}

/// Iterator over array elements.
pub struct ArrayIter<'d, 'a, 's> {
    doc: &'d JsonDocument<'a, 's>,
    next: NodeId,
}

impl Iterator for ArrayIter<'_, '_, '_> {
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

/// Iterator over object members.
pub struct ObjectIter<'d, 'a, 's> {
    doc: &'d JsonDocument<'a, 's>,
    next: NodeId,
}

impl<'a> Iterator for ObjectIter<'_, 'a, '_> {
    type Item = (JsonString<'a>, NodeId);

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
