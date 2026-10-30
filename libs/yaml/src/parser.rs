//! Indentation-aware YAML parser.

use crate::document::{Kind, YamlNode};
use crate::scalar::{Chomp, ScalarStyle, YamlScalar};
use crate::{NodeId, Result, YamlError, MAX_DEPTH, NO_NODE};

pub(crate) fn parse<'a>(input: &'a str, nodes: &mut [YamlNode<'a>]) -> Result<NodeId> {
    let mut parser = Parser {
        input,
        pos: 0,
        used: 0,
    };

    parser.skip_directives()?;

    let Some(line) = parser.peek_line()? else {
        let node = parser.alloc(nodes)?;
        nodes[node as usize].kind = Kind::Scalar(YamlScalar::plain(""));
        return Ok(node);
    };

    let root = parser.parse_block(nodes, line.indent, 0)?;

    if let Some(line) = parser.peek_line()? {
        let content = strip_comment(line.content).trim_end();
        if content == "..." {
            parser.consume(line);
        } else {
            return Err(YamlError::MultipleDocuments);
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

    /// Absolute input offset of a slice that was taken from [`Parser::input`].
    fn base_of(&self, text: &str) -> usize {
        text.as_ptr() as usize - self.input.as_ptr() as usize
    }

    fn parse_block(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        indent: usize,
        depth: usize,
    ) -> Result<NodeId> {
        if depth > MAX_DEPTH {
            return Err(YamlError::DepthLimit);
        }
        let line = self.peek_line()?.ok_or(YamlError::UnexpectedEnd)?;
        if line.indent != indent {
            return Err(YamlError::InvalidIndentation { offset: line.start });
        }
        let content = strip_comment(line.content).trim_end();

        if is_sequence_item(content) {
            self.parse_sequence(nodes, indent, depth)
        } else if find_mapping_colon(content).is_some() {
            self.parse_mapping(nodes, indent, depth, None)
        } else if is_block_indicator(content) {
            self.consume(line);
            self.parse_block_scalar(nodes, content, indent, depth)
        } else {
            self.consume(line);
            self.parse_inline_value(nodes, content, indent, depth)
        }
    }

    fn parse_sequence(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        indent: usize,
        depth: usize,
    ) -> Result<NodeId> {
        if depth > MAX_DEPTH {
            return Err(YamlError::DepthLimit);
        }
        let sequence = self.alloc(nodes)?;
        let mut first = NO_NODE;
        let mut last = NO_NODE;

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
            let content = strip_comment(line.content).trim_end();
            if !is_sequence_item(content) {
                break;
            }

            let rest = content[1..].trim_start();
            self.consume(line);

            let child = if rest.is_empty() {
                self.parse_nested(nodes, indent, depth + 1)?
            } else if find_mapping_colon(rest).is_some() {
                self.parse_mapping(nodes, indent + 2, depth + 1, Some(rest))?
            } else if is_block_indicator(rest) {
                self.parse_block_scalar(nodes, rest, indent, depth + 1)?
            } else {
                self.parse_inline_value(nodes, rest, indent, depth + 1)?
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
            let content = strip_comment(line.content).trim_end();
            if is_sequence_item(content) {
                break;
            }
            if is_document_marker(content) {
                break;
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
            self.parse_nested(nodes, indent, depth + 1)?
        } else if is_block_indicator(rest) {
            self.parse_block_scalar(nodes, rest, indent, depth + 1)?
        } else {
            self.parse_inline_value(nodes, rest, indent, depth + 1)?
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

    fn parse_nested(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        parent_indent: usize,
        depth: usize,
    ) -> Result<NodeId> {
        if let Some(line) = self.peek_line()? {
            if line.indent > parent_indent {
                return self.parse_block(nodes, line.indent, depth);
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
        parent_indent: usize,
        depth: usize,
    ) -> Result<NodeId> {
        let text = text.trim();
        if text.is_empty() {
            return self.null_node(nodes);
        }
        if let Some(byte) = text.as_bytes().first() {
            match byte {
                b'*' => {
                    return Err(YamlError::UnsupportedAlias {
                        offset: self.pos.saturating_sub(text.len()),
                    })
                }
                b'&' | b'!' => {
                    // Anchors and tags are not modelled. Skip the token and
                    // parse the value that follows it.
                    let rest = text
                        .split_once(char::is_whitespace)
                        .map(|(_, rest)| rest.trim_start())
                        .unwrap_or("");
                    if rest.is_empty() {
                        return self.null_node(nodes);
                    }
                    return self.parse_inline_value(nodes, rest, parent_indent, depth);
                }
                b'[' | b'{' => {
                    let (id, used) = self.parse_flow(nodes, text, depth + 1)?;
                    if !text[used..].trim().is_empty() {
                        return Err(YamlError::UnexpectedToken {
                            offset: self.pos.saturating_sub(text.len()) + used,
                            byte: text.as_bytes()[used],
                        });
                    }
                    return Ok(id);
                }
                b'\'' => {
                    let (scalar, used) = scan_single(text, 0, self.base_of(text))?;
                    self.expect_end(text, used)?;
                    return self.scalar_node(nodes, scalar);
                }
                b'"' => {
                    let (scalar, used) = scan_double(text, 0, self.base_of(text))?;
                    self.expect_end(text, used)?;
                    return self.scalar_node(nodes, scalar);
                }
                b'|' | b'>' => {
                    return self.parse_block_scalar(nodes, text, parent_indent, depth);
                }
                _ => {}
            }
        }
        self.scalar_node(nodes, YamlScalar::plain(text))
    }

    fn expect_end(&self, text: &str, used: usize) -> Result<()> {
        if text[used..].trim().is_empty() {
            Ok(())
        } else {
            Err(YamlError::UnexpectedToken {
                offset: self.pos.saturating_sub(text.len()) + used,
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
            return Err(YamlError::MissingColon { offset: 0 });
        }
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
            b'*' => Err(YamlError::UnsupportedAlias { offset: 0 }),
            b'[' | b'{' => Err(YamlError::MissingColon { offset: 0 }),
            _ => Ok(YamlScalar::plain(text)),
        }
    }

    fn parse_block_scalar(
        &mut self,
        nodes: &mut [YamlNode<'a>],
        indicator: &'a str,
        parent_indent: usize,
        _depth: usize,
    ) -> Result<NodeId> {
        let indicator = indicator.trim();
        let style = match indicator.as_bytes().first() {
            Some(b'|') => ScalarStyle::Literal,
            Some(b'>') => ScalarStyle::Folded,
            _ => return Err(YamlError::MissingColon { offset: 0 }),
        };
        let mut chomp = Chomp::Clip;
        for byte in indicator.bytes().skip(1) {
            match byte {
                b'-' => chomp = Chomp::Strip,
                b'+' => chomp = Chomp::Keep,
                b'0'..=b'9' => {}
                b' ' | b'\t' | b'#' => break,
                other => {
                    return Err(YamlError::UnexpectedToken {
                        offset: self.pos,
                        byte: other,
                    })
                }
            }
        }

        let block_start = self.pos;
        let bytes = self.input.as_bytes();
        let mut scan = self.pos;
        let mut content_indent = None;
        let mut end;
        let mut has_content = false;

        loop {
            if scan >= bytes.len() {
                end = scan;
                break;
            }
            let line_start = scan;
            let mut line_end = scan;
            while line_end < bytes.len() && bytes[line_end] != b'\n' {
                line_end += 1;
            }
            let raw = &self.input[line_start..line_end];
            let mut indent = 0usize;
            while indent < raw.len() && raw.as_bytes()[indent] == b' ' {
                indent += 1;
            }
            if raw.as_bytes().get(indent) == Some(&b'\t') {
                return Err(YamlError::TabIndentation {
                    offset: line_start + indent,
                });
            }
            let blank = raw[indent..].trim().is_empty();
            if !blank {
                if indent <= parent_indent {
                    end = line_start;
                    break;
                }
                content_indent.get_or_insert(indent);
                has_content = true;
            }
            end = if line_end < bytes.len() {
                line_end + 1
            } else {
                line_end
            };
            scan = end;
        }

        self.pos = end;
        if !has_content {
            return self.null_node(nodes);
        }
        let indent = content_indent.unwrap_or(parent_indent + 1);
        let raw = &self.input[block_start..end];
        self.scalar_node(
            nodes,
            YamlScalar::block(raw, style, indent.min(u8::MAX as usize) as u8, chomp),
        )
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
        match byte {
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
                offset: flow.pos,
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
                        offset: flow.pos,
                        byte,
                    })
                }
                None => return Err(YamlError::UnterminatedFlow { offset: flow.pos }),
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
                return Err(YamlError::MissingColon { offset: flow.pos });
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
                        offset: flow.pos,
                        byte,
                    })
                }
                None => return Err(YamlError::UnterminatedFlow { offset: flow.pos }),
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
                    return Err(YamlError::MissingColon { offset: flow.pos });
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

fn strip_comment(content: &str) -> &str {
    let bytes = content.as_bytes();
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if double {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                double = false;
            }
        } else if single {
            if byte == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 1;
                } else {
                    single = false;
                }
            }
        } else {
            match byte {
                b'\'' => single = true,
                b'"' => double = true,
                b'#' if i == 0 || matches!(bytes[i - 1], b' ' | b'\t') => {
                    return content[..i].trim_end();
                }
                _ => {}
            }
        }
        i += 1;
    }
    content.trim_end()
}

fn find_mapping_colon(content: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let mut depth = 0usize;
    for (i, &byte) in bytes.iter().enumerate() {
        if double {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                double = false;
            }
            continue;
        }
        if single {
            if byte == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    continue;
                }
                single = false;
            }
            continue;
        }
        match byte {
            b'\'' => single = true,
            b'"' => double = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                if matches!(bytes.get(i + 1), None | Some(b' ' | b'\t')) {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
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
                i += 1;
                if i >= bytes.len() {
                    return Err(YamlError::UnterminatedQuote {
                        offset: base + start,
                    });
                }
                match bytes[i] {
                    b'x' => i += 2,
                    b'u' => i += 4,
                    b'U' => i += 8,
                    _ => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    Err(YamlError::UnterminatedQuote {
        offset: base + start,
    })
}
