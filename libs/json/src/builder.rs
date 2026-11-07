//! Writing JSON without a document to write it from.
//!
//! [`JsonBuilder`] writes to any [`core::fmt::Write`] as it's called, and allocates
//! nothing: all it remembers is, for each open container, whether it's an object and
//! whether it has anything in it yet, in bits. Calls that wouldn't make valid JSON are
//! errors, so a finished builder has written one valid value.
//!
//! ```
//! use feos_json::JsonBuilder;
//!
//! let mut text = String::new();
//! let mut json = JsonBuilder::new(&mut text);
//! json.begin_object().unwrap();
//! json.field_str("name", "feos").unwrap();
//! json.key("sizes").unwrap();
//! json.begin_array().unwrap();
//! json.uint(1).unwrap();
//! json.int(-2).unwrap();
//! json.end_array().unwrap();
//! json.end_object().unwrap();
//! json.finish().unwrap();
//! assert_eq!(text, r#"{"name":"feos","sizes":[1,-2]}"#);
//! ```

use core::fmt::{self, Write};

use crate::document::JsonDocument;
use crate::encode::{write_json, write_string};
use crate::{NodeId, MAX_DEPTH};

/// How a [`JsonBuilder`] call failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildError {
    /// The output refused the text: a full buffer, usually.
    Output,
    /// This call would have made invalid JSON here: a value in an object with no key,
    /// a key anywhere else, a close that doesn't match, or a second top-level value.
    Misplaced,
    /// More than [`MAX_DEPTH`] containers are open, which the parser wouldn't read.
    TooDeep,
    /// A number JSON has no way of writing, such as infinity.
    NotANumber,
    /// [`JsonBuilder::finish`] with something still open, or nothing written at all.
    Unfinished,
}

impl From<fmt::Error> for BuildError {
    fn from(_: fmt::Error) -> Self {
        Self::Output
    }
}

/// Writes one JSON value, a piece at a time.
pub struct JsonBuilder<'w, W: Write> {
    out: &'w mut W,
    /// Bit `n` set: the container at depth `n` is an object.
    objects: [u64; MAX_DEPTH.div_ceil(64)],
    /// Bit `n` set: the container at depth `n` has an entry already.
    filled: [u64; MAX_DEPTH.div_ceil(64)],
    depth: usize,
    /// In an object, the key has been written and its value hasn't.
    awaiting_value: bool,
    /// The top-level value has started.
    started: bool,
    /// Spaces per level, or none for the compact form.
    indent: Option<usize>,
}

impl<'w, W: Write> JsonBuilder<'w, W> {
    /// A builder for compact output.
    pub fn new(out: &'w mut W) -> Self {
        Self {
            out,
            objects: [0; MAX_DEPTH.div_ceil(64)],
            filled: [0; MAX_DEPTH.div_ceil(64)],
            depth: 0,
            awaiting_value: false,
            started: false,
            indent: None,
        }
    }

    /// A builder that indents by two spaces, as [`crate::write_json_pretty`] does.
    pub fn pretty(out: &'w mut W) -> Self {
        let mut builder = Self::new(out);
        builder.indent = Some(2);
        builder
    }

    fn bit(words: &[u64], index: usize) -> bool {
        words[index / 64] >> (index % 64) & 1 != 0
    }

    fn set_bit(words: &mut [u64], index: usize, value: bool) {
        if value {
            words[index / 64] |= 1 << (index % 64);
        } else {
            words[index / 64] &= !(1 << (index % 64));
        }
    }

    fn in_object(&self) -> bool {
        self.depth > 0 && Self::bit(&self.objects, self.depth - 1)
    }

    fn line_break(&mut self, depth: usize) -> Result<(), BuildError> {
        if let Some(indent) = self.indent {
            self.out.write_char('\n')?;
            for _ in 0..depth * indent {
                self.out.write_char(' ')?;
            }
        }
        Ok(())
    }

    /// Everything before a new entry in a container: a comma, and a new line.
    fn separate(&mut self) -> Result<(), BuildError> {
        let level = self.depth - 1;
        if Self::bit(&self.filled, level) {
            self.out.write_char(',')?;
        }
        Self::set_bit(&mut self.filled, level, true);
        self.line_break(self.depth)
    }

    /// Everything before a value, and the check that one can go here.
    fn before_value(&mut self) -> Result<(), BuildError> {
        if self.depth == 0 {
            if self.started {
                return Err(BuildError::Misplaced);
            }
            self.started = true;
            return Ok(());
        }
        if self.in_object() {
            if !self.awaiting_value {
                return Err(BuildError::Misplaced);
            }
            self.awaiting_value = false;
            return Ok(());
        }
        self.separate()
    }

    /// Starts a key in the object that's open.
    pub fn key(&mut self, key: &str) -> Result<(), BuildError> {
        if !self.in_object() || self.awaiting_value {
            return Err(BuildError::Misplaced);
        }
        self.separate()?;
        write_string(self.out, key)?;
        self.out.write_char(':')?;
        if self.indent.is_some() {
            self.out.write_char(' ')?;
        }
        self.awaiting_value = true;
        Ok(())
    }

    fn open(&mut self, object: bool) -> Result<(), BuildError> {
        if self.depth == MAX_DEPTH {
            return Err(BuildError::TooDeep);
        }
        self.before_value()?;
        self.out.write_char(if object { '{' } else { '[' })?;
        Self::set_bit(&mut self.objects, self.depth, object);
        Self::set_bit(&mut self.filled, self.depth, false);
        self.depth += 1;
        Ok(())
    }

    fn close(&mut self, object: bool) -> Result<(), BuildError> {
        if self.depth == 0 || self.in_object() != object || self.awaiting_value {
            return Err(BuildError::Misplaced);
        }
        let filled = Self::bit(&self.filled, self.depth - 1);
        self.depth -= 1;
        if filled {
            self.line_break(self.depth)?;
        }
        self.out.write_char(if object { '}' } else { ']' })?;
        Ok(())
    }

    /// Opens an object.
    pub fn begin_object(&mut self) -> Result<(), BuildError> {
        self.open(true)
    }

    /// Closes the open object.
    pub fn end_object(&mut self) -> Result<(), BuildError> {
        self.close(true)
    }

    /// Opens an array.
    pub fn begin_array(&mut self) -> Result<(), BuildError> {
        self.open(false)
    }

    /// Closes the open array.
    pub fn end_array(&mut self) -> Result<(), BuildError> {
        self.close(false)
    }

    /// Writes `null`.
    pub fn null(&mut self) -> Result<(), BuildError> {
        self.before_value()?;
        self.out.write_str("null")?;
        Ok(())
    }

    /// Writes `true` or `false`.
    pub fn boolean(&mut self, value: bool) -> Result<(), BuildError> {
        self.before_value()?;
        self.out.write_str(if value { "true" } else { "false" })?;
        Ok(())
    }

    /// Writes a string, escaped as JSON needs.
    pub fn string(&mut self, value: &str) -> Result<(), BuildError> {
        self.before_value()?;
        write_string(self.out, value)?;
        Ok(())
    }

    /// Writes a signed integer.
    pub fn int(&mut self, value: i64) -> Result<(), BuildError> {
        self.before_value()?;
        write!(self.out, "{value}")?;
        Ok(())
    }

    /// Writes an unsigned integer.
    pub fn uint(&mut self, value: u64) -> Result<(), BuildError> {
        self.before_value()?;
        write!(self.out, "{value}")?;
        Ok(())
    }

    /// Writes a floating point number, the shortest form that reads back the same.
    /// Infinities and NaN have no JSON form, and are refused.
    pub fn float(&mut self, value: f64) -> Result<(), BuildError> {
        if !value.is_finite() {
            return Err(BuildError::NotANumber);
        }
        self.before_value()?;
        write!(self.out, "{value}")?;
        Ok(())
    }

    /// Writes a value from a parsed document, compactly.
    pub fn value_from(&mut self, document: &JsonDocument<'_, '_>, id: NodeId) -> Result<(), BuildError> {
        self.before_value()?;
        write_json(document, id, &mut *self.out)?;
        Ok(())
    }

    /// `key` and then a string.
    pub fn field_str(&mut self, key: &str, value: &str) -> Result<(), BuildError> {
        self.key(key)?;
        self.string(value)
    }

    /// `key` and then an unsigned integer.
    pub fn field_uint(&mut self, key: &str, value: u64) -> Result<(), BuildError> {
        self.key(key)?;
        self.uint(value)
    }

    /// `key` and then `true` or `false`.
    pub fn field_bool(&mut self, key: &str, value: bool) -> Result<(), BuildError> {
        self.key(key)?;
        self.boolean(value)
    }

    /// Ends the value. An error means what was written so far isn't complete JSON.
    pub fn finish(self) -> Result<(), BuildError> {
        if self.depth != 0 || !self.started {
            return Err(BuildError::Unfinished);
        }
        Ok(())
    }
}

/// An output over a caller's buffer, for writing with no allocator. A write that doesn't
/// fit is an error, so nothing is ever silently cut.
pub struct SliceOutput<'b> {
    buffer: &'b mut [u8],
    written: usize,
}

impl<'b> SliceOutput<'b> {
    /// An empty output over `buffer`.
    pub fn new(buffer: &'b mut [u8]) -> Self {
        Self { buffer, written: 0 }
    }

    /// What's been written.
    pub fn as_str(&self) -> &str {
        // Only `write_str` adds to it, and that takes whole strings.
        core::str::from_utf8(&self.buffer[..self.written]).unwrap_or("")
    }
}

impl Write for SliceOutput<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.written.checked_add(text.len()).ok_or(fmt::Error)?;
        self.buffer
            .get_mut(self.written..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.written = end;
        Ok(())
    }
}
