//! YAML block-style serialisation.

use core::fmt::{self, Write};

use crate::document::{Kind, YamlDocument};
use crate::{NodeId, NO_NODE};

/// Writes a YAML document in block style.
pub fn write_yaml<W: Write>(doc: &YamlDocument<'_, '_>, id: NodeId, out: &mut W) -> fmt::Result {
    write_node(doc, id, out, 0, false)
}

fn write_node<W: Write>(
    doc: &YamlDocument<'_, '_>,
    id: NodeId,
    out: &mut W,
    indent: usize,
    sequence_item: bool,
) -> fmt::Result {
    match doc.kind(id) {
        Some(Kind::Scalar(scalar)) => {
            if sequence_item {
                write_indent(out, indent)?;
                out.write_str("- ")?;
            }
            write_scalar(out, scalar)
        }
        Some(Kind::Sequence) => {
            if sequence_item {
                write_indent(out, indent)?;
                out.write_str("-\n")?;
            }
            let mut child = doc.node(id).map(|node| node.first).unwrap_or(NO_NODE);
            while child != NO_NODE {
                write_node(doc, child, out, indent + 2, true)?;
                out.write_char('\n')?;
                child = doc.node(child).map(|node| node.next).unwrap_or(NO_NODE);
            }
            Ok(())
        }
        Some(Kind::Mapping) => {
            if sequence_item {
                write_indent(out, indent)?;
                out.write_str("-\n")?;
            }
            let mut child = doc.node(id).map(|node| node.first).unwrap_or(NO_NODE);
            while child != NO_NODE {
                write_indent(out, indent + if sequence_item { 2 } else { 0 })?;
                let node = doc.node(child);
                if let Some(key) = node.and_then(|node| node.key) {
                    write_scalar(out, key)?;
                } else {
                    out.write_str("?")?;
                }
                out.write_str(":")?;

                match doc.kind(child) {
                    Some(Kind::Scalar(scalar)) => {
                        out.write_char(' ')?;
                        write_scalar(out, scalar)?;
                    }
                    Some(Kind::Sequence) if doc.is_empty(child) == Some(true) => {
                        out.write_str(" []")?;
                    }
                    Some(Kind::Mapping) if doc.is_empty(child) == Some(true) => {
                        out.write_str(" {}")?;
                    }
                    _ => {
                        out.write_char('\n')?;
                        let child_indent = indent + if sequence_item { 2 } else { 0 } + 2;
                        write_node(doc, child, out, child_indent, false)?;
                    }
                }
                out.write_char('\n')?;
                child = doc.node(child).map(|node| node.next).unwrap_or(NO_NODE);
            }
            Ok(())
        }
        None => out.write_str("null"),
    }
}

fn write_indent<W: Write>(out: &mut W, indent: usize) -> fmt::Result {
    for _ in 0..indent {
        out.write_char(' ')?;
    }
    Ok(())
}

fn write_scalar<W: Write>(out: &mut W, scalar: crate::YamlScalar<'_>) -> fmt::Result {
    let mut buffer = [0u8; 1024];
    let Some(text) = scalar.decode_into(&mut buffer) else {
        out.write_str("\"<scalar too long>\"")?;
        return Ok(());
    };

    let needs_quotes = text.is_empty()
        || text.trim() != text
        || text.contains([':', '#', '\n', '\r', '\t'])
        || text.starts_with([
            '-', '?', '[', ']', '{', '}', ',', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`',
        ]);

    if !needs_quotes {
        out.write_str(text)
    } else if !text.contains('\'') {
        out.write_char('\'')?;
        out.write_str(text)?;
        out.write_char('\'')
    } else {
        out.write_char('"')?;
        for character in text.chars() {
            match character {
                '"' => out.write_str("\\\"")?,
                '\\' => out.write_str("\\\\")?,
                '\n' => out.write_str("\\n")?,
                '\r' => out.write_str("\\r")?,
                '\t' => out.write_str("\\t")?,
                c => out.write_char(c)?,
            }
        }
        out.write_char('"')
    }
}
