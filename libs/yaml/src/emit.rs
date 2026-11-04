//! YAML block-style serialisation.

use core::fmt::{self, Write};

use crate::document::{Kind, YamlDocument};
use crate::{NodeId, ScalarStyle, NO_NODE};

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
    if sequence_item {
        write_indent(out, indent)?;
        out.write_char('-')?;
    }
    match doc.kind(id) {
        Some(Kind::Scalar(scalar)) => {
            if sequence_item {
                out.write_char(' ')?;
            }
            write_scalar(out, scalar)
        }
        Some(Kind::Sequence) if doc.is_empty(id) == Some(true) => {
            out.write_str(if sequence_item { " []" } else { "[]" })
        }
        Some(Kind::Mapping) if doc.is_empty(id) == Some(true) => {
            out.write_str(if sequence_item { " {}" } else { "{}" })
        }
        Some(Kind::Sequence) => {
            if sequence_item {
                out.write_char('\n')?;
            }
            let child_indent = if sequence_item { indent + 2 } else { indent };
            write_sequence(doc, id, out, child_indent)
        }
        Some(Kind::Mapping) => {
            if sequence_item {
                out.write_char('\n')?;
            }
            let child_indent = if sequence_item { indent + 2 } else { indent };
            write_mapping(doc, id, out, child_indent)
        }
        None => out.write_str("null"),
    }
}

/// Writes items at `indent`, one per line, with no trailing line break.
fn write_sequence<W: Write>(
    doc: &YamlDocument<'_, '_>,
    id: NodeId,
    out: &mut W,
    indent: usize,
) -> fmt::Result {
    let mut child = doc.node(id).map(|node| node.first).unwrap_or(NO_NODE);
    while child != NO_NODE {
        write_node(doc, child, out, indent, true)?;
        child = doc.node(child).map(|node| node.next).unwrap_or(NO_NODE);
        if child != NO_NODE {
            out.write_char('\n')?;
        }
    }
    Ok(())
}

/// Writes entries at `indent`, one per line, with no trailing line break.
fn write_mapping<W: Write>(
    doc: &YamlDocument<'_, '_>,
    id: NodeId,
    out: &mut W,
    indent: usize,
) -> fmt::Result {
    let mut child = doc.node(id).map(|node| node.first).unwrap_or(NO_NODE);
    while child != NO_NODE {
        write_indent(out, indent)?;
        match doc.node(child).and_then(|node| node.key) {
            Some(key) => write_scalar(out, key)?,
            None => return Err(fmt::Error),
        }
        out.write_char(':')?;
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
                write_node(doc, child, out, indent + 2, false)?;
            }
        }
        child = doc.node(child).map(|node| node.next).unwrap_or(NO_NODE);
        if child != NO_NODE {
            out.write_char('\n')?;
        }
    }
    Ok(())
}

fn write_indent<W: Write>(out: &mut W, indent: usize) -> fmt::Result {
    for _ in 0..indent {
        out.write_char(' ')?;
    }
    Ok(())
}

/// Largest decoded scalar the emitter can stage. A longer one fails with
/// [`fmt::Error`]: writing a placeholder would corrupt the document.
const SCALAR_BUFFER: usize = 1024;

fn write_scalar<W: Write>(out: &mut W, scalar: crate::YamlScalar<'_>) -> fmt::Result {
    let plain_style = scalar.style() == ScalarStyle::Plain;
    if plain_style && scalar.raw().is_empty() {
        // An empty plain scalar is null; `''` would turn it into a string.
        return out.write_str("null");
    }
    if plain_style && is_plain_safe(scalar.raw()) {
        return out.write_str(scalar.raw());
    }

    let mut buffer = [0u8; SCALAR_BUFFER];
    let text = scalar.decode_into(&mut buffer).ok_or(fmt::Error)?;

    // A quoted `true` or `12` is a string, so it must stay quoted.
    let keeps_type = plain_style || !looks_typed(text);
    if keeps_type && is_plain_safe(text) {
        out.write_str(text)
    } else if text.chars().any(needs_escape) {
        write_double_quoted(out, text)
    } else {
        write_single_quoted(out, text)
    }
}

fn needs_escape(character: char) -> bool {
    character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
}

/// True when `text` reads back unchanged as a plain scalar.
fn is_plain_safe(text: &str) -> bool {
    if text.is_empty() || text.trim() != text || text.chars().any(needs_escape) {
        return false;
    }
    if text.contains(": ") || text.ends_with(':') || text.contains(" #") {
        return false;
    }
    if text == "---" || text == "..." || text.starts_with("--- ") {
        return false;
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(
            '[' | ']' | '{' | '}' | ',' | '&' | '*' | '!' | '|' | '>' | '\'' | '"' | '%' | '@'
            | '`' | '#',
        ) => false,
        Some('-' | '?' | ':') => chars.next().is_some_and(|next| next != ' '),
        _ => true,
    }
}

/// True when a plain scalar with this text would read as a null, bool or number.
fn looks_typed(text: &str) -> bool {
    text == "~"
        || ["null", "true", "false", "yes", "no", "on", "off"]
            .iter()
            .any(|word| text.eq_ignore_ascii_case(word))
        || text.parse::<f64>().is_ok()
}

fn write_single_quoted<W: Write>(out: &mut W, text: &str) -> fmt::Result {
    out.write_char('\'')?;
    for character in text.chars() {
        if character == '\'' {
            out.write_char('\'')?;
        }
        out.write_char(character)?;
    }
    out.write_char('\'')
}

fn write_double_quoted<W: Write>(out: &mut W, text: &str) -> fmt::Result {
    out.write_char('"')?;
    for character in text.chars() {
        match character {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            control if needs_escape(control) => write!(out, "\\u{:04x}", control as u32)?,
            other => out.write_char(other)?,
        }
    }
    out.write_char('"')
}
