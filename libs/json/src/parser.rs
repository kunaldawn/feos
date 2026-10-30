//! Recursive-descent JSON parser.

use crate::document::JsonNode;
use crate::string::hex_value;
use crate::{JsonError, JsonString, Kind, NodeId, MAX_DEPTH, NO_NODE};

pub(crate) fn parse<'a>(input: &'a str, nodes: &mut [JsonNode<'a>]) -> crate::Result<NodeId> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
        used: 0,
    };
    parser.skip_whitespace();
    let root = parser.parse_value(nodes, 0)?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return Err(JsonError::TrailingData { offset: parser.pos });
    }
    Ok(root)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    used: usize,
}

impl<'a> Parser<'a> {
    fn parse_value(&mut self, nodes: &mut [JsonNode<'a>], depth: usize) -> crate::Result<NodeId> {
        if depth > MAX_DEPTH {
            return Err(JsonError::DepthLimit);
        }

        self.skip_whitespace();
        let byte = *self.bytes.get(self.pos).ok_or(JsonError::UnexpectedEnd)?;
        let id = self.alloc(nodes)?;

        let kind = match byte {
            b'n' => {
                self.expect_literal(b"null")?;
                Kind::Null
            }
            b't' => {
                self.expect_literal(b"true")?;
                Kind::Bool(true)
            }
            b'f' => {
                self.expect_literal(b"false")?;
                Kind::Bool(false)
            }
            b'"' => Kind::String(self.parse_string()?),
            b'-' | b'0'..=b'9' => Kind::Number(self.parse_number()?),
            b'[' => return self.parse_array(nodes, depth, id),
            b'{' => return self.parse_object(nodes, depth, id),
            other => {
                return Err(JsonError::UnexpectedByte {
                    offset: self.pos,
                    byte: other,
                })
            }
        };

        nodes[id as usize].kind = kind;
        Ok(id)
    }

    fn parse_array(
        &mut self,
        nodes: &mut [JsonNode<'a>],
        depth: usize,
        id: NodeId,
    ) -> crate::Result<NodeId> {
        self.pos += 1; // '['
        self.skip_whitespace();

        let mut first = NO_NODE;
        let mut last = NO_NODE;

        if self.peek() == Some(b']') {
            self.pos += 1;
            nodes[id as usize].first = NO_NODE;
            nodes[id as usize].kind = Kind::Array;
            return Ok(id);
        }

        loop {
            let child = self.parse_value(nodes, depth + 1)?;
            if first == NO_NODE {
                first = child;
            } else {
                nodes[last as usize].next = child;
            }
            last = child;

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_whitespace();
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                Some(byte) => {
                    return Err(JsonError::UnexpectedByte {
                        offset: self.pos,
                        byte,
                    })
                }
                None => return Err(JsonError::UnexpectedEnd),
            }
        }

        nodes[id as usize].kind = Kind::Array;
        nodes[id as usize].first = first;
        Ok(id)
    }

    fn parse_object(
        &mut self,
        nodes: &mut [JsonNode<'a>],
        depth: usize,
        id: NodeId,
    ) -> crate::Result<NodeId> {
        self.pos += 1; // '{'
        self.skip_whitespace();

        let mut first = NO_NODE;
        let mut last = NO_NODE;

        if self.peek() == Some(b'}') {
            self.pos += 1;
            nodes[id as usize].first = NO_NODE;
            nodes[id as usize].kind = Kind::Object;
            return Ok(id);
        }

        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return match self.peek() {
                    Some(byte) => Err(JsonError::UnexpectedByte {
                        offset: self.pos,
                        byte,
                    }),
                    None => Err(JsonError::UnexpectedEnd),
                };
            }
            let key = self.parse_string()?;

            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return match self.peek() {
                    Some(byte) => Err(JsonError::UnexpectedByte {
                        offset: self.pos,
                        byte,
                    }),
                    None => Err(JsonError::UnexpectedEnd),
                };
            }
            self.pos += 1;

            let child = self.parse_value(nodes, depth + 1)?;
            nodes[child as usize].key = Some(key);

            if first == NO_NODE {
                first = child;
            } else {
                nodes[last as usize].next = child;
            }
            last = child;

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(byte) => {
                    return Err(JsonError::UnexpectedByte {
                        offset: self.pos,
                        byte,
                    })
                }
                None => return Err(JsonError::UnexpectedEnd),
            }
        }

        nodes[id as usize].kind = Kind::Object;
        nodes[id as usize].first = first;
        Ok(id)
    }

    fn parse_string(&mut self) -> crate::Result<JsonString<'a>> {
        self.pos += 1; // opening quote
        let start = self.pos;
        let mut escaped = false;

        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err(JsonError::UnexpectedEnd);
            };
            match byte {
                b'"' => {
                    let raw = core::str::from_utf8(&self.bytes[start..self.pos])
                        .map_err(|_| JsonError::InvalidEscape { offset: start })?;
                    self.pos += 1;
                    return Ok(JsonString::new(raw, escaped));
                }
                b'\\' => {
                    escaped = true;
                    let escape_offset = self.pos;
                    self.pos += 1;
                    let Some(&escape) = self.bytes.get(self.pos) else {
                        return Err(JsonError::UnexpectedEnd);
                    };
                    match escape {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                            self.pos += 1;
                        }
                        b'u' => {
                            self.pos += 1;
                            self.parse_unicode_escape(escape_offset)?;
                        }
                        _ => {
                            return Err(JsonError::InvalidEscape {
                                offset: escape_offset,
                            })
                        }
                    }
                }
                byte if byte < 0x20 => {
                    return Err(JsonError::ControlCharacter { offset: self.pos })
                }
                _ => self.pos += 1,
            }
        }
    }

    fn parse_unicode_escape(&mut self, escape_offset: usize) -> crate::Result<()> {
        let first = self.read_hex4(escape_offset)?;
        if (0xd800..=0xdbff).contains(&first) {
            if self.bytes.get(self.pos) != Some(&b'\\')
                || self.bytes.get(self.pos + 1) != Some(&b'u')
            {
                return Err(JsonError::InvalidUnicode {
                    offset: escape_offset,
                });
            }
            self.pos += 2;
            let second = self.read_hex4(escape_offset)?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(JsonError::InvalidUnicode {
                    offset: escape_offset,
                });
            }
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(JsonError::InvalidUnicode {
                offset: escape_offset,
            });
        }
        Ok(())
    }

    fn read_hex4(&mut self, escape_offset: usize) -> crate::Result<u16> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or(JsonError::UnexpectedEnd)?;
        let mut value = 0u16;
        for &digit in digits {
            let Some(nibble) = hex_value(digit) else {
                return Err(JsonError::InvalidEscape {
                    offset: escape_offset,
                });
            };
            value = (value << 4) | nibble as u16;
        }
        self.pos += 4;
        Ok(value)
    }

    fn parse_number(&mut self) -> crate::Result<&'a str> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }

        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(JsonError::InvalidNumber { offset: start }),
        }

        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError::InvalidNumber { offset: start });
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError::InvalidNumber { offset: start });
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        core::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| JsonError::InvalidNumber { offset: start })
    }

    fn expect_literal(&mut self, literal: &[u8]) -> crate::Result<()> {
        let end = self.pos + literal.len();
        if self.bytes.get(self.pos..end) == Some(literal) {
            self.pos = end;
            Ok(())
        } else {
            let offset = self
                .bytes
                .iter()
                .zip(literal)
                .position(|(actual, expected)| actual != expected)
                .map(|index| self.pos + index)
                .unwrap_or(self.pos);
            let byte = self.bytes.get(offset).copied().unwrap_or(0);
            Err(JsonError::UnexpectedByte { offset, byte })
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn alloc(&mut self, nodes: &mut [JsonNode<'a>]) -> crate::Result<NodeId> {
        if self.used >= nodes.len() {
            return Err(JsonError::NodeLimit);
        }
        let id = self.used as NodeId;
        nodes[self.used] = JsonNode::EMPTY;
        self.used += 1;
        Ok(id)
    }
}
