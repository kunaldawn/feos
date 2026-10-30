//! JSON serialisation.

use core::fmt::{self, Write};

use crate::document::JsonDocument;
use crate::{Kind, NodeId, NO_NODE};

/// Writes a JSON value from `doc` to `out` in compact form.
pub fn write_json<W: Write>(doc: &JsonDocument<'_, '_>, id: NodeId, out: &mut W) -> fmt::Result {
    write_value(doc, id, out, None, 0)
}

/// Writes a JSON value from `doc` to `out` with two-space indentation.
pub fn write_json_pretty<W: Write>(
    doc: &JsonDocument<'_, '_>,
    id: NodeId,
    out: &mut W,
) -> fmt::Result {
    write_value(doc, id, out, Some(2), 0)
}

fn write_value<W: Write>(
    doc: &JsonDocument<'_, '_>,
    id: NodeId,
    out: &mut W,
    indent: Option<usize>,
    depth: usize,
) -> fmt::Result {
    match doc.kind(id) {
        Some(Kind::Null) => out.write_str("null"),
        Some(Kind::Bool(true)) => out.write_str("true"),
        Some(Kind::Bool(false)) => out.write_str("false"),
        Some(Kind::Number(text)) => out.write_str(text),
        Some(Kind::String(string)) => {
            out.write_char('"')?;
            out.write_str(string.raw())?;
            out.write_char('"')
        }
        Some(Kind::Array) => {
            out.write_char('[')?;
            let mut child = doc.node(id).map(|node| node.first).unwrap_or(NO_NODE);
            let mut first = true;
            while child != NO_NODE {
                if !first {
                    out.write_char(',')?;
                }
                first = false;
                newline(out, indent, depth + 1)?;
                write_value(doc, child, out, indent, depth + 1)?;
                child = doc.node(child).map(|node| node.next).unwrap_or(NO_NODE);
            }
            if !first {
                newline(out, indent, depth)?;
            }
            out.write_char(']')
        }
        Some(Kind::Object) => {
            out.write_char('{')?;
            let mut child = doc.node(id).map(|node| node.first).unwrap_or(NO_NODE);
            let mut first = true;
            while child != NO_NODE {
                if !first {
                    out.write_char(',')?;
                }
                first = false;
                newline(out, indent, depth + 1)?;
                if let Some(key) = doc.node(child).and_then(|node| node.key) {
                    out.write_char('"')?;
                    out.write_str(key.raw())?;
                    out.write_char('"')?;
                    out.write_char(':')?;
                    if indent.is_some() {
                        out.write_char(' ')?;
                    }
                }
                write_value(doc, child, out, indent, depth + 1)?;
                child = doc.node(child).map(|node| node.next).unwrap_or(NO_NODE);
            }
            if !first {
                newline(out, indent, depth)?;
            }
            out.write_char('}')
        }
        None => out.write_str("null"),
    }
}

fn newline<W: Write>(out: &mut W, indent: Option<usize>, depth: usize) -> fmt::Result {
    let Some(step) = indent else {
        return Ok(());
    };
    out.write_char('\n')?;
    for _ in 0..step * depth {
        out.write_char(' ')?;
    }
    Ok(())
}
