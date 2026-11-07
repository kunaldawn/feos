//! Indentation-aware YAML parser.

use crate::document::{Kind, YamlNode};
use crate::scalar::{decode_escape, Chomp, ScalarStyle, YamlScalar};
use crate::{NodeId, Result, YamlError, MAX_DEPTH, NO_NODE};

pub(crate) fn parse<'a>(input: &'a str, nodes: &mut [YamlNode<'a>]) -> Result<NodeId> {
    let mut parser = Parser {
        input,
        pos: 0,
        used: 0,
    };

    parser.skip_directives()?;

    let Some(line) = parser.peek_line()? else {
        return parser.null_node(nodes);
    };

    let first = strip_comment(line.content);
    let root = if line.indent == 0 && is_document_marker(first) {
        parser.null_node(nodes)?
    } else if let Some(rest) = first.strip_prefix("--- ").filter(|_| line.indent == 0) {
        // `--- value` puts the root node on the marker line.
        parser.consume(line);
        let rest = rest.trim_start();
        if is_block_indicator(rest) {
            parser.parse_block_scalar(nodes, rest, None)?
        } else {
            parser.parse_inline_value(nodes, rest, None, 0)?
        }
    } else {
        parser.parse_block(nodes, line.indent, None, 0)?
    };

    if let Some(line) = parser.peek_line()? {
        let content = strip_comment(line.content);
        if line.indent == 0 && content == "..." {
            parser.consume(line);
        } else if line.indent == 0 && content == "---" {
            return Err(YamlError::MultipleDocuments);
        } else {
            // Content the root node could not absorb is a layout error, not
            // a second document.
            return Err(YamlError::InvalidIndentation { offset: line.start });
        }
        if parser.peek_line()?.is_some() {
            return Err(YamlError::MultipleDocuments);
        }
    }

    Ok(root)
}

#[derive(Clone, Copy)]
struct Line<'a> {
    start: usize,
    end: usize,
    indent: usize,
    content: &'a str,
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
    used: usize,
}

impl<'a> Parser<'a> {
    fn skip_directives(&mut self) -> Result<()> {
        loop {
            let Some(line) = self.peek_line()? else {
                return Ok(());
            };
            let content = strip_comment(line.content).trim_end();
            if content.starts_with('%') {
                self.consume(line);
                continue;
            }
            if content == "---" {
                self.consume(line);
            }
            return Ok(());
        }
    }

    fn peek_line(&mut self) -> Result<Option<Line<'a>>> {
        let bytes = self.input.as_bytes();
        loop {
            if self.pos >= bytes.len() {
                return Ok(None);
            }
            let start = self.pos;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            let raw = &self.input[start..end];
            let mut indent = 0usize;
            while indent < raw.len() && raw.as_bytes()[indent] == b' ' {
                indent += 1;
            }
            if raw.as_bytes().get(indent) == Some(&b'\t') {
                return Err(YamlError::TabIndentation {
                    offset: start + indent,
                });
            }
            let content = &raw[indent..];
            let content = content.trim_end_matches([' ', '\t', '\r']);
            if content.is_empty() || content.starts_with('#') {
                self.pos = if end < bytes.len() { end + 1 } else { end };
                continue;
            }
            return Ok(Some(Line {
                start,
                end,
                indent,
                content,
            }));
        }
    }

    fn consume(&mut self, line: Line<'_>) {
        self.pos = if line.end < self.input.len() {
            line.end + 1
        } else {
            line.end
        };
    }

    fn offset(&self, line: Line<'_>, index: usize) -> usize {
        line.start + line.indent + index
    }

    /// Rejects `? key` complex-key syntax, which is not modelled.
    fn reject_complex_key(&self, line: Line<'_>, content: &str) -> Result<()> {
        if content == "?" || content.starts_with("? ") {
            return Err(YamlError::UnexpectedToken {
                offset: self.offset(line, 0),
                byte: b'?',
            });
        }
        Ok(())
    }

    /// Absolute input offset of a slice that was taken from [`Parser::input`].
    fn base_of(&self, text: &str) -> usize {
        text.as_ptr() as usize - self.input.as_ptr() as usize
    }

    /// Parses the block node starting at the next line.
    ///
    /// `parent` is the indentation of the enclosing collection, or `None` for
    /// the document root, where block scalars may start in column zero.
    fn parse_block(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        indent: usize,
        parent: Option<usize>,
        depth: usize,
    ) -> Result<NodeId> {
        if depth > MAX_DEPTH {
            return Err(YamlError::DepthLimit);
        }
        let line = self.peek_line()?.ok_or(YamlError::UnexpectedEnd)?;
        if line.indent != indent {
            return Err(YamlError::InvalidIndentation { offset: line.start });
        }
        let content = strip_comment(line.content);
        self.reject_complex_key(line, content)?;

        if is_sequence_item(content) {
            self.parse_sequence(nodes, indent, depth, None)
        } else if find_mapping_colon(content).is_some() {
            self.parse_mapping(nodes, indent, depth, None)
        } else if is_block_indicator(content) {
            self.consume(line);
            self.parse_block_scalar(nodes, content, parent)
        } else {
            self.consume(line);
            self.parse_inline_value(nodes, content, parent, depth)
        }
    }

    /// Parses a block sequence whose dashes sit at column `indent`.
    ///
    /// `first_item` is an item that already started on the current line, as in
    /// the inner sequence of `- - a`.
    fn parse_sequence(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        indent: usize,
        depth: usize,
        first_item: Option<&'a str>,
    ) -> Result<NodeId> {
        if depth > MAX_DEPTH {
            return Err(YamlError::DepthLimit);
        }
        let sequence = self.alloc(nodes)?;
        let mut first = NO_NODE;
        let mut last = NO_NODE;
        let mut pending = first_item;

        loop {
            let content = if let Some(content) = pending.take() {
                content
            } else {
                let Some(line) = self.peek_line()? else {
                    break;
                };
                if line.indent < indent {
                    break;
                }
                if line.indent > indent {
                    return Err(YamlError::InvalidIndentation { offset: line.start });
                }
                let content = strip_comment(line.content);
                if !is_sequence_item(content) {
                    break;
                }
                self.consume(line);
                content
            };

            let rest = content[1..].trim_start();
            // `rest` is a suffix of `content`, so this is the item's real
            // column: `-   a: 1` continues at column 4, not column 2.
            let column = indent + (content.len() - rest.len());

            let child = if rest.is_empty() {
                self.parse_nested(nodes, indent, depth + 1)?
            } else if is_sequence_item(rest) {
                self.parse_sequence(nodes, column, depth + 1, Some(rest))?
            } else if find_mapping_colon(rest).is_some() {
                self.parse_mapping(nodes, column, depth + 1, Some(rest))?
            } else if is_block_indicator(rest) {
                self.parse_block_scalar(nodes, rest, Some(indent))?
            } else {
                self.parse_inline_value(nodes, rest, Some(indent), depth + 1)?
            };

            if first == NO_NODE {
                first = child;
            } else {
                nodes[last as usize].next = child;
            }
            last = child;
        }

        nodes[sequence as usize].kind = Kind::Sequence;
        nodes[sequence as usize].first = first;
        Ok(sequence)
    }

    fn parse_mapping(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        indent: usize,
        depth: usize,
        first_entry: Option<&'a str>,
    ) -> Result<NodeId> {
        if depth > MAX_DEPTH {
            return Err(YamlError::DepthLimit);
        }
        let mapping = self.alloc(nodes)?;
        let mut first = NO_NODE;
        let mut last = NO_NODE;

        if let Some(entry) = first_entry {
            self.add_mapping_entry(nodes, entry, indent, depth, &mut first, &mut last)?;
        }

        loop {
            let Some(line) = self.peek_line()? else {
                break;
            };
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return Err(YamlError::InvalidIndentation { offset: line.start });
            }
            let content = strip_comment(line.content);
            if is_document_marker(content) {
                break;
            }
            self.reject_complex_key(line, content)?;
            if is_sequence_item(content) {
                // A sequence at this column can only be a key's value, and
                // that case is consumed by `add_mapping_entry`.
                return Err(YamlError::InvalidIndentation { offset: line.start });
            }
            if find_mapping_colon(content).is_none() {
                return Err(YamlError::MissingColon {
                    offset: self.offset(line, 0),
                });
            }
            self.consume(line);
            self.add_mapping_entry(nodes, content, indent, depth, &mut first, &mut last)?;
        }

        nodes[mapping as usize].kind = Kind::Mapping;
        nodes[mapping as usize].first = first;
        Ok(mapping)
    }

    fn add_mapping_entry(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        content: &'a str,
        indent: usize,
        depth: usize,
        first: &mut NodeId,
        last: &mut NodeId,
    ) -> Result<()> {
        let colon = find_mapping_colon(content).ok_or(YamlError::MissingColon { offset: 0 })?;
        let key_text = content[..colon].trim();
        let key = self.parse_key(key_text)?;
        let rest = content[colon + 1..].trim_start();

        let child = if rest.is_empty() {
            self.parse_mapping_value(nodes, indent, depth + 1)?
        } else if is_block_indicator(rest) {
            self.parse_block_scalar(nodes, rest, Some(indent))?
        } else {
            self.parse_inline_value(nodes, rest, Some(indent), depth + 1)?
        };

        nodes[child as usize].key = Some(key);
        if *first == NO_NODE {
            *first = child;
        } else {
            nodes[*last as usize].next = child;
        }
        *last = child;
        Ok(())
    }

    /// A mapping value on the following lines. Unlike a sequence item's value,
    /// it may be a sequence at the key's own column (`tags:\n- a`).
    fn parse_mapping_value(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        indent: usize,
        depth: usize,
    ) -> Result<NodeId> {
        if let Some(line) = self.peek_line()? {
            if line.indent == indent && is_sequence_item(strip_comment(line.content)) {
                return self.parse_sequence(nodes, indent, depth, None);
            }
        }
        self.parse_nested(nodes, indent, depth)
    }

    fn parse_nested(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        parent_indent: usize,
        depth: usize,
    ) -> Result<NodeId> {
        if let Some(line) = self.peek_line()? {
            if line.indent > parent_indent {
                return self.parse_block(nodes, line.indent, Some(parent_indent), depth);
            }
        }
        self.null_node(nodes)
    }

    fn null_node(&mut self, nodes: &mut [YamlNode<'a>]) -> Result<NodeId> {
        let id = self.alloc(nodes)?;
        nodes[id as usize].kind = Kind::Scalar(YamlScalar::plain(""));
        Ok(id)
    }

    fn parse_inline_value(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        text: &'a str,
        parent: Option<usize>,
        depth: usize,
    ) -> Result<NodeId> {
        let text = text.trim();
        if text.is_empty() {
            return self.null_node(nodes);
        }
        if let Some(byte) = text.as_bytes().first() {
            match byte {
                b'*' | b'&' => {
                    return Err(YamlError::UnsupportedAlias {
                        offset: self.base_of(text),
                    })
                }
                b'!' => {
                    return Err(YamlError::UnsupportedTag {
                        offset: self.base_of(text),
                    })
                }
                b'[' | b'{' => {
                    let (id, used) = self.parse_flow(nodes, text, depth + 1)?;
                    self.expect_end(text, used)?;
                    return Ok(id);
                }
                b'\'' | b'"' => {
                    let scalar = self.scan_quoted(text, parent)?;
                    return self.scalar_node(nodes, scalar);
                }
                b'|' | b'>' => {
                    return self.parse_block_scalar(nodes, text, parent);
                }
                _ => {}
            }
        }
        let scalar = self.scan_plain(text, parent)?;
        self.scalar_node(nodes, scalar)
    }

    /// A quoted scalar that starts `text`, which may go on over the following lines. Those
    /// have to be indented further than the collection it's in.
    fn scan_quoted(&mut self, text: &'a str, parent: Option<usize>) -> Result<YamlScalar<'a>> {
        let base = self.base_of(text);
        let double = text.as_bytes()[0] == b'"';
        // The rest of the document, as the closing quote can be anywhere in it.
        let whole = &self.input[base..];
        let (scalar, used) = if double {
            scan_double(whole, 0, base)?
        } else {
            scan_single(whole, 0, base)?
        };
        if used <= text.len() {
            self.expect_end(text, used)?;
            return Ok(scalar);
        }

        // Each line it took has to belong here. The first is the line already consumed.
        let end = base + used;
        let mut start = self.pos;
        while start < end {
            let line_end = self.input[start..].find('\n').map_or(self.input.len(), |at| start + at);
            let raw = &self.input[start..line_end];
            let indent = raw.bytes().take_while(|byte| *byte == b' ').count();
            if !raw.trim().is_empty() {
                if parent.is_some_and(|parent| indent <= parent) {
                    return Err(YamlError::InvalidIndentation { offset: start });
                }
                if parent.is_none() && indent == 0 && is_document_marker(raw.trim_end()) {
                    return Err(YamlError::UnterminatedQuote { offset: base });
                }
            }
            start = if line_end < self.input.len() { line_end + 1 } else { line_end };
        }

        // After the closing quote, only a comment can be on its line.
        let line_end = self.input[end..].find('\n').map_or(self.input.len(), |at| end + at);
        let tail = strip_comment(self.input[end..line_end].trim_start_matches([' ', '\t']));
        let tail = tail.trim_end_matches('\r');
        if !tail.is_empty() {
            return Err(YamlError::UnexpectedToken {
                offset: self.base_of(tail),
                byte: tail.as_bytes()[0],
            });
        }
        self.pos = if line_end < self.input.len() { line_end + 1 } else { line_end };
        Ok(scalar)
    }

    /// A plain scalar that starts with `text`, taking in the more indented lines that
    /// follow it, unless a comment has ended it. One that looks like a mapping entry
    /// isn't taken, and whatever's reading the block rejects it: it's not valid here.
    fn scan_plain(&mut self, text: &'a str, parent: Option<usize>) -> Result<YamlScalar<'a>> {
        let base = self.base_of(text);
        let first_line_end = self.input[base..].find('\n').map_or(self.input.len(), |at| base + at);
        let first_line = self.input[base..first_line_end].trim_end();
        if first_line.len() != text.len() {
            // A comment, which ends the scalar.
            return Ok(YamlScalar::plain(text));
        }
        let mut end = base + text.len();
        loop {
            let saved = self.pos;
            let Some(line) = self.peek_line()? else {
                break;
            };
            let deep_enough = parent.map_or(true, |parent| line.indent > parent);
            let content = strip_comment(line.content);
            if !deep_enough
                || find_mapping_colon(content).is_some()
                || (line.indent == 0 && is_document_marker(content))
            {
                // `peek_line` may have stepped over blank lines, which aren't ours now.
                self.pos = saved;
                break;
            }
            self.consume(line);
            end = self.base_of(content) + content.len();
            if content.len() != line.content.len() {
                break;
            }
        }
        Ok(YamlScalar::plain(&self.input[base..end]))
    }

    fn expect_end(&self, text: &str, used: usize) -> Result<()> {
        if text[used..].trim().is_empty() {
            Ok(())
        } else {
            Err(YamlError::UnexpectedToken {
                offset: self.base_of(text) + used,
                byte: text.as_bytes()[used],
            })
        }
    }

    fn scalar_node(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        scalar: YamlScalar<'a>,
    ) -> Result<NodeId> {
        let id = self.alloc(nodes)?;
        nodes[id as usize].kind = Kind::Scalar(scalar);
        Ok(id)
    }

    fn parse_key(&self, text: &'a str) -> Result<YamlScalar<'a>> {
        let text = text.trim();
        if text.is_empty() {
            return Err(YamlError::MissingColon {
                offset: self.base_of(text),
            });
        }
        let offset = self.base_of(text);
        match text.as_bytes()[0] {
            b'\'' => {
                let (scalar, used) = scan_single(text, 0, self.base_of(text))?;
                self.expect_end(text, used)?;
                Ok(scalar)
            }
            b'"' => {
                let (scalar, used) = scan_double(text, 0, self.base_of(text))?;
                self.expect_end(text, used)?;
                Ok(scalar)
            }
            b'*' | b'&' => Err(YamlError::UnsupportedAlias { offset }),
            b'!' => Err(YamlError::UnsupportedTag { offset }),
            b'[' | b'{' => Err(YamlError::MissingColon { offset }),
            _ => Ok(YamlScalar::plain(text)),
        }
    }

    /// Parses a block scalar whose indicator was on the line just consumed.
    ///
    /// `parent` is the indentation of the enclosing collection. The scalar ends
    /// at the first non-blank line indented less than its content; whatever
    /// that line is, it belongs to the enclosing block.
    fn parse_block_scalar(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        indicator: &'a str,
        parent: Option<usize>,
    ) -> Result<NodeId> {
        let offset = self.base_of(indicator);
        let indicator = indicator.trim();
        let style = match indicator.as_bytes().first() {
            Some(b'|') => ScalarStyle::Literal,
            Some(b'>') => ScalarStyle::Folded,
            _ => return Err(YamlError::MissingColon { offset }),
        };
        let mut chomp = None;
        let mut explicit = None;
        for (index, byte) in indicator.bytes().enumerate().skip(1) {
            let duplicate = match byte {
                b'-' | b'+' => chomp.is_some(),
                b'1'..=b'9' => explicit.is_some(),
                _ => true,
            };
            if duplicate {
                return Err(YamlError::UnexpectedToken {
                    offset: offset + index,
                    byte,
                });
            }
            match byte {
                b'-' => chomp = Some(Chomp::Strip),
                b'+' => chomp = Some(Chomp::Keep),
                digit => explicit = Some((digit - b'0') as usize),
            }
        }
        let chomp = chomp.unwrap_or(Chomp::Clip);

        let minimum = parent.map_or(0, |parent| parent + 1);
        let mut content_indent = explicit.map(|digits| parent.unwrap_or(0) + digits);

        let block_start = self.pos;
        let bytes = self.input.as_bytes();
        let mut scan = self.pos;

        loop {
            if scan >= bytes.len() {
                break;
            }
            let mut line_end = scan;
            while line_end < bytes.len() && bytes[line_end] != b'\n' {
                line_end += 1;
            }
            let raw = &self.input[scan..line_end];
            let indent = raw.bytes().take_while(|&byte| byte == b' ').count();
            if !raw.trim().is_empty() {
                let marker = parent.is_none() && is_document_marker(raw.trim_end());
                if marker || indent < content_indent.unwrap_or(minimum) {
                    break;
                }
                content_indent.get_or_insert(indent);
            }
            scan = if line_end < bytes.len() {
                line_end + 1
            } else {
                line_end
            };
        }

        self.pos = scan;
        let indent = content_indent.unwrap_or(minimum);
        let indent = u8::try_from(indent).map_err(|_| YamlError::InvalidIndentation {
            offset: block_start,
        })?;
        let raw = &self.input[block_start..scan];
        self.scalar_node(nodes, YamlScalar::block(raw, style, indent, chomp))
    }

    fn parse_flow(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        text: &'a str,
        depth: usize,
    ) -> Result<(NodeId, usize)> {
        let mut flow = Flow {
            bytes: text.as_bytes(),
            pos: 0,
        };
        let id = self.flow_value(nodes, &mut flow, text, depth)?;
        Ok((id, flow.pos))
    }

    fn flow_value(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        flow: &mut Flow<'a>,
        text: &'a str,
        depth: usize,
    ) -> Result<NodeId> {
        if depth > MAX_DEPTH {
            return Err(YamlError::DepthLimit);
        }
        flow.skip_space();
        let Some(&byte) = flow.bytes.get(flow.pos) else {
            return Err(YamlError::UnexpectedEnd);
        };
        let base = self.base_of(text);
        match byte {
            b'*' | b'&' => Err(YamlError::UnsupportedAlias {
                offset: base + flow.pos,
            }),
            b'!' => Err(YamlError::UnsupportedTag {
                offset: base + flow.pos,
            }),
            b'[' => self.flow_sequence(nodes, flow, text, depth),
            b'{' => self.flow_mapping(nodes, flow, text, depth),
            b'\'' => {
                let (scalar, used) = scan_single(text, flow.pos, self.base_of(text))?;
                flow.pos = used;
                self.scalar_node(nodes, scalar)
            }
            b'"' => {
                let (scalar, used) = scan_double(text, flow.pos, self.base_of(text))?;
                flow.pos = used;
                self.scalar_node(nodes, scalar)
            }
            b']' | b'}' | b',' => Err(YamlError::UnexpectedToken {
                offset: base + flow.pos,
                byte,
            }),
            _ => {
                let start = flow.pos;
                while let Some(&byte) = flow.bytes.get(flow.pos) {
                    if matches!(byte, b',' | b']' | b'}') {
                        break;
                    }
                    flow.pos += 1;
                }
                let raw = text[start..flow.pos].trim();
                self.scalar_node(nodes, YamlScalar::plain(raw))
            }
        }
    }

    fn flow_sequence(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        flow: &mut Flow<'a>,
        text: &'a str,
        depth: usize,
    ) -> Result<NodeId> {
        let sequence = self.alloc(nodes)?;
        let mut first = NO_NODE;
        let mut last = NO_NODE;
        flow.pos += 1; // '['
        flow.skip_space();
        if flow.bytes.get(flow.pos) == Some(&b']') {
            flow.pos += 1;
            nodes[sequence as usize].kind = Kind::Sequence;
            return Ok(sequence);
        }
        loop {
            let child = self.flow_value(nodes, flow, text, depth + 1)?;
            if first == NO_NODE {
                first = child;
            } else {
                nodes[last as usize].next = child;
            }
            last = child;
            flow.skip_space();
            match flow.bytes.get(flow.pos) {
                Some(b',') => {
                    flow.pos += 1;
                    flow.skip_space();
                    if flow.bytes.get(flow.pos) == Some(&b']') {
                        flow.pos += 1;
                        break;
                    }
                }
                Some(b']') => {
                    flow.pos += 1;
                    break;
                }
                Some(&byte) => {
                    return Err(YamlError::UnexpectedToken {
                        offset: self.base_of(text) + flow.pos,
                        byte,
                    })
                }
                None => {
                    return Err(YamlError::UnterminatedFlow {
                        offset: self.base_of(text) + flow.pos,
                    })
                }
            }
        }
        nodes[sequence as usize].kind = Kind::Sequence;
        nodes[sequence as usize].first = first;
        Ok(sequence)
    }

    fn flow_mapping(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        flow: &mut Flow<'a>,
        text: &'a str,
        depth: usize,
    ) -> Result<NodeId> {
        let mapping = self.alloc(nodes)?;
        let mut first = NO_NODE;
        let mut last = NO_NODE;
        flow.pos += 1; // '{'
        flow.skip_space();
        if flow.bytes.get(flow.pos) == Some(&b'}') {
            flow.pos += 1;
            nodes[mapping as usize].kind = Kind::Mapping;
            return Ok(mapping);
        }
        loop {
            let key = self.flow_key(flow, text)?;
            flow.skip_space();
            if flow.bytes.get(flow.pos) != Some(&b':') {
                return Err(YamlError::MissingColon {
                    offset: self.base_of(text) + flow.pos,
                });
            }
            flow.pos += 1;
            let child = self.flow_value(nodes, flow, text, depth + 1)?;
            nodes[child as usize].key = Some(key);
            if first == NO_NODE {
                first = child;
            } else {
                nodes[last as usize].next = child;
            }
            last = child;
            flow.skip_space();
            match flow.bytes.get(flow.pos) {
                Some(b',') => {
                    flow.pos += 1;
                    flow.skip_space();
                    if flow.bytes.get(flow.pos) == Some(&b'}') {
                        flow.pos += 1;
                        break;
                    }
                }
                Some(b'}') => {
                    flow.pos += 1;
                    break;
                }
                Some(&byte) => {
                    return Err(YamlError::UnexpectedToken {
                        offset: self.base_of(text) + flow.pos,
                        byte,
                    })
                }
                None => {
                    return Err(YamlError::UnterminatedFlow {
                        offset: self.base_of(text) + flow.pos,
                    })
                }
            }
        }
        nodes[mapping as usize].kind = Kind::Mapping;
        nodes[mapping as usize].first = first;
        Ok(mapping)
    }

    fn flow_key(&mut self, flow: &mut Flow<'a>, text: &'a str) -> Result<YamlScalar<'a>> {
        flow.skip_space();
        match flow.bytes.get(flow.pos).copied() {
            Some(b'\'') => {
                let (scalar, used) = scan_single(text, flow.pos, self.base_of(text))?;
                flow.pos = used;
                Ok(scalar)
            }
            Some(b'"') => {
                let (scalar, used) = scan_double(text, flow.pos, self.base_of(text))?;
                flow.pos = used;
                Ok(scalar)
            }
            None => Err(YamlError::UnexpectedEnd),
            Some(b'*' | b'&') => Err(YamlError::UnsupportedAlias {
                offset: self.base_of(text) + flow.pos,
            }),
            Some(b'!') => Err(YamlError::UnsupportedTag {
                offset: self.base_of(text) + flow.pos,
            }),
            _ => {
                let start = flow.pos;
                while let Some(&byte) = flow.bytes.get(flow.pos) {
                    if byte == b':' || matches!(byte, b',' | b'}' | b']') {
                        break;
                    }
                    flow.pos += 1;
                }
                let raw = text[start..flow.pos].trim();
                if raw.is_empty() {
                    return Err(YamlError::MissingColon {
                        offset: self.base_of(text) + flow.pos,
                    });
                }
                Ok(YamlScalar::plain(raw))
            }
        }
    }

    fn alloc(&mut self, nodes: &mut [YamlNode<'a>]) -> Result<NodeId> {
        if self.used >= nodes.len() {
            return Err(YamlError::NodeLimit);
        }
        let id = self.used as NodeId;
        nodes[self.used] = YamlNode::EMPTY;
        self.used += 1;
        Ok(id)
    }
}

struct Flow<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Flow<'_> {
    fn skip_space(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }
}

fn is_sequence_item(content: &str) -> bool {
    content == "-" || content.starts_with("- ") || content.starts_with("-\t")
}

fn is_document_marker(content: &str) -> bool {
    content == "---" || content == "..."
}

fn is_block_indicator(content: &str) -> bool {
    let bytes = content.as_bytes();
    matches!(bytes.first(), Some(b'|' | b'>'))
        && (bytes.len() == 1 || matches!(bytes[1], b'+' | b'-' | b'0'..=b'9' | b' ' | b'\t' | b'#'))
}

/// What one line's structure scan found.
struct LineScan {
    /// First `:` at flow depth zero that separates a key from a value.
    colon: Option<usize>,
    /// Start of a trailing `#` comment.
    comment: Option<usize>,
}

/// Finds a line's mapping colon and comment.
///
/// Quotes only open a quoted scalar at the start of a token. An apostrophe in
/// the middle of a plain scalar (`don't panic`) is ordinary text, as is a
/// bracket (`foo[1]`) outside a flow collection.
fn scan_line(content: &str) -> LineScan {
    let bytes = content.as_bytes();
    let mut scan = LineScan {
        colon: None,
        comment: None,
    };
    let mut token_start = true;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        let next_is_break = matches!(bytes.get(i + 1), None | Some(b' ' | b'\t'));
        match byte {
            b' ' | b'\t' => {}
            b'\'' if token_start => {
                let Some(close) = closing_single(bytes, i + 1) else {
                    return scan;
                };
                i = close;
                token_start = false;
            }
            b'"' if token_start => {
                let Some(close) = closing_double(bytes, i + 1) else {
                    return scan;
                };
                i = close;
                token_start = false;
            }
            b'#' if i == 0 || matches!(bytes[i - 1], b' ' | b'\t') => {
                scan.comment = Some(i);
                return scan;
            }
            b'[' | b'{' if token_start || depth > 0 => {
                depth += 1;
                token_start = true;
            }
            b']' | b'}' if depth > 0 => {
                depth -= 1;
                token_start = false;
            }
            b',' if depth > 0 => token_start = true,
            b':' if next_is_break && depth > 0 => token_start = true,
            b':' if next_is_break && scan.colon.is_none() => {
                scan.colon = Some(i);
                token_start = true;
            }
            b'-' | b'?' if token_start && depth == 0 && next_is_break => {}
            _ => token_start = false,
        }
        i += 1;
    }
    scan
}

fn closing_single(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) != Some(&b'\'') {
                return Some(i);
            }
            i += 1;
        }
        i += 1;
    }
    None
}

fn closing_double(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some(i),
            b'\\' => i += 1,
            _ => {}
        }
        i += 1;
    }
    None
}

fn strip_comment(content: &str) -> &str {
    match scan_line(content).comment {
        Some(comment) => content[..comment].trim_end(),
        None => content.trim_end(),
    }
}

fn find_mapping_colon(content: &str) -> Option<usize> {
    scan_line(content).colon
}

fn scan_single<'a>(text: &'a str, start: usize, base: usize) -> Result<(YamlScalar<'a>, usize)> {
    let bytes = text.as_bytes();
    let mut i = start + 1;
    let content_start = i;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Ok((
                YamlScalar::styled(&text[content_start..i], ScalarStyle::SingleQuoted),
                i + 1,
            ));
        }
        i += 1;
    }
    Err(YamlError::UnterminatedQuote {
        offset: base + start,
    })
}

fn scan_double<'a>(text: &'a str, start: usize, base: usize) -> Result<(YamlScalar<'a>, usize)> {
    let bytes = text.as_bytes();
    let mut i = start + 1;
    let content_start = i;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                return Ok((
                    YamlScalar::styled(&text[content_start..i], ScalarStyle::DoubleQuoted),
                    i + 1,
                ))
            }
            b'\\' => {
                let escape = i;
                i += 1;
                if i >= bytes.len() {
                    return Err(YamlError::UnterminatedQuote {
                        offset: base + start,
                    });
                }
                if decode_escape(bytes, &mut i).is_none() {
                    return Err(YamlError::InvalidEscape {
                        offset: base + escape,
                    });
                }
            }
            _ => i += 1,
        }
    }
    Err(YamlError::UnterminatedQuote {
        offset: base + start,
    })
}
