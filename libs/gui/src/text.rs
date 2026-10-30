//! Fixed-capacity UTF-8 text editing and small number formatting helpers.

use core::fmt::{self, Write};

/// A fixed-capacity, UTF-8 text buffer with a cursor and selection.
///
/// `N` is the byte capacity. Insertions that would overflow are rejected and
/// return `false`; the buffer is never left in an invalid UTF-8 state.
///
/// The cursor is a byte offset and always lies on a character boundary.
pub struct TextBuffer<const N: usize = 128> {
    bytes: [u8; N],
    len: usize,
    cursor: usize,
    anchor: Option<usize>,
}

impl<const N: usize> TextBuffer<N> {
    /// Creates an empty buffer.
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
            cursor: 0,
            anchor: None,
        }
    }

    /// Creates a buffer containing `text`, truncated to `N` bytes on a
    /// character boundary.
    pub fn with_text(text: &str) -> Self {
        let mut buffer = Self::new();
        buffer.set(text);
        buffer
    }

    /// The buffer capacity in bytes.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of used bytes.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when the buffer is empty.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The buffer contents.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// The raw bytes (always valid UTF-8).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The cursor's byte offset.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Sets the cursor, snapping to the nearest preceding character boundary.
    pub fn set_cursor(&mut self, cursor: usize) {
        let mut cursor = cursor.min(self.len);
        while cursor > 0 && !self.is_boundary(cursor) {
            cursor -= 1;
        }
        self.cursor = cursor;
    }

    /// Replaces the contents with `text`.
    pub fn set(&mut self, text: &str) {
        let mut end = text.len().min(N);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        self.len = end;
        self.cursor = end;
        self.anchor = None;
    }

    /// Empties the buffer and resets the cursor.
    pub fn clear(&mut self) {
        self.len = 0;
        self.cursor = 0;
        self.anchor = None;
    }

    /// Inserts one character at the cursor. Returns `false` when the
    /// character is a control character or the buffer is full.
    pub fn insert(&mut self, ch: char) -> bool {
        if ch.is_control() {
            return false;
        }
        let mut encoded = [0u8; 4];
        let text = ch.encode_utf8(&mut encoded);
        self.insert_str(text)
    }

    /// Inserts a string at the cursor, replacing the current selection.
    pub fn insert_str(&mut self, text: &str) -> bool {
        self.delete_selection();
        if self.len + text.len() > N {
            return false;
        }

        let cursor = self.cursor;
        self.bytes
            .copy_within(cursor..self.len, cursor + text.len());
        self.bytes[cursor..cursor + text.len()].copy_from_slice(text.as_bytes());
        self.len += text.len();
        self.cursor += text.len();
        true
    }

    /// Deletes the selection, or the character before the cursor.
    pub fn backspace(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }

        let start = self.previous_boundary(self.cursor);
        self.remove_range(start, self.cursor);
        self.cursor = start;
        true
    }

    /// Deletes the selection, or the character after the cursor.
    pub fn delete(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor >= self.len {
            return false;
        }

        let end = self.next_boundary(self.cursor);
        self.remove_range(self.cursor, end);
        true
    }

    /// Moves the cursor one character left.
    pub fn move_left(&mut self, extend_selection: bool) {
        if !extend_selection {
            if let Some((start, _)) = self.selection() {
                self.cursor = start;
                self.anchor = None;
                return;
            }
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }

        if self.cursor > 0 {
            self.cursor = self.previous_boundary(self.cursor);
        }
    }

    /// Moves the cursor one character right.
    pub fn move_right(&mut self, extend_selection: bool) {
        if !extend_selection {
            if let Some((_, end)) = self.selection() {
                self.cursor = end;
                self.anchor = None;
                return;
            }
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }

        if self.cursor < self.len {
            self.cursor = self.next_boundary(self.cursor);
        }
    }

    /// Moves to the start of the buffer.
    pub fn move_home(&mut self, extend_selection: bool) {
        if extend_selection && self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        } else if !extend_selection {
            self.anchor = None;
        }
        self.cursor = 0;
    }

    /// Moves to the end of the buffer.
    pub fn move_end(&mut self, extend_selection: bool) {
        if extend_selection && self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        } else if !extend_selection {
            self.anchor = None;
        }
        self.cursor = self.len;
    }

    /// Selects the whole buffer.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.len;
    }

    /// The selection as `(start, end)` byte offsets, if non-empty.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.anchor.and_then(|anchor| {
            if anchor == self.cursor {
                None
            } else if anchor < self.cursor {
                Some((anchor, self.cursor))
            } else {
                Some((self.cursor, anchor))
            }
        })
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection()
            .map(|(start, end)| &self.as_str()[start..end])
    }

    /// Deletes the selected text. Returns `true` when something was removed.
    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        self.remove_range(start, end);
        self.cursor = start;
        self.anchor = None;
        true
    }

    /// Removes `start..end` and shifts the rest left.
    fn remove_range(&mut self, start: usize, end: usize) {
        if start >= end || end > self.len {
            return;
        }
        self.bytes.copy_within(end..self.len, start);
        self.len -= end - start;
    }

    fn is_boundary(&self, index: usize) -> bool {
        index == 0 || index >= self.len || (self.bytes[index] & 0xc0) != 0x80
    }

    fn previous_boundary(&self, mut index: usize) -> usize {
        if index == 0 {
            return 0;
        }
        index -= 1;
        while index > 0 && (self.bytes[index] & 0xc0) == 0x80 {
            index -= 1;
        }
        index
    }

    fn next_boundary(&self, mut index: usize) -> usize {
        if index >= self.len {
            return self.len;
        }
        index += 1;
        while index < self.len && (self.bytes[index] & 0xc0) == 0x80 {
            index += 1;
        }
        index
    }
}

impl<const N: usize> Default for TextBuffer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Debug for TextBuffer<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextBuffer")
            .field("text", &self.as_str())
            .field("cursor", &self.cursor)
            .field("selection", &self.selection())
            .finish()
    }
}

/// Optional explicit state for custom text editors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextEditState {
    /// Horizontal scroll offset in pixels.
    pub scroll: i32,
}

impl TextEditState {
    pub const fn new() -> Self {
        Self { scroll: 0 }
    }
}

/// A `core::fmt::Write` sink over a caller-owned byte slice.
pub struct FixedBuf<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl<'a> FixedBuf<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, len: 0 }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// Consumes the sink and returns the formatted text with the backing
    /// slice's lifetime.
    pub fn into_str(self) -> &'a str {
        let length = self.len;
        let bytes = self.bytes;
        core::str::from_utf8(&bytes[..length]).unwrap_or("")
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Write for FixedBuf<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.len + text.len() > self.bytes.len() {
            return Err(fmt::Error);
        }
        self.bytes[self.len..self.len + text.len()].copy_from_slice(text.as_bytes());
        self.len += text.len();
        Ok(())
    }
}

/// Formats a float into `out` with `decimals` digits after the point.
///
/// Returns the formatted prefix when `out` is too small to hold the whole
/// number rather than panicking.
pub fn format_f32(value: f32, decimals: usize, out: &mut [u8]) -> &str {
    let mut buffer = FixedBuf::new(out);
    let _ = write!(&mut buffer, "{:.*}", decimals, value);
    buffer.into_str()
}

/// Formats an unsigned integer into `out`.
///
/// Returns the formatted prefix when `out` is too small rather than panicking.
pub fn format_u32(value: u32, out: &mut [u8]) -> &str {
    let mut buffer = FixedBuf::new(out);
    let _ = write!(&mut buffer, "{}", value);
    buffer.into_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_respect_utf8_boundaries() {
        let mut buffer = TextBuffer::<32>::new();
        assert!(buffer.insert_str("aé"));
        assert_eq!(buffer.as_str(), "aé");
        buffer.move_left(false);
        assert!(buffer.insert('b'));
        assert_eq!(buffer.as_str(), "abé");
        assert!(buffer.backspace());
        assert_eq!(buffer.as_str(), "aé");
        assert!(buffer.backspace());
        assert_eq!(buffer.as_str(), "é");
    }

    #[test]
    fn selection_is_replaced_and_deleted() {
        let mut buffer = TextBuffer::<32>::with_text("hello");
        buffer.select_all();
        assert_eq!(buffer.selected_text(), Some("hello"));
        assert!(buffer.insert_str("world"));
        assert_eq!(buffer.as_str(), "world");

        buffer.move_home(false);
        buffer.move_right(true);
        buffer.move_right(true);
        assert_eq!(buffer.selection(), Some((0, 2)));
        assert!(buffer.delete());
        assert_eq!(buffer.as_str(), "rld");
    }

    #[test]
    fn capacity_is_enforced_without_corruption() {
        let mut buffer = TextBuffer::<4>::new();
        assert!(buffer.insert_str("abcd"));
        assert!(!buffer.insert('e'));
        assert_eq!(buffer.as_str(), "abcd");
        assert!(!buffer.insert_str("é"));
    }

    #[test]
    fn truncation_lands_on_a_boundary() {
        let buffer = TextBuffer::<3>::with_text("aé!");
        assert_eq!(buffer.as_str(), "aé");
        assert_eq!(buffer.len(), 3);
    }

    #[test]
    fn numbers_format() {
        let mut out = [0u8; 16];
        assert_eq!(format_f32(12.345, 1, &mut out), "12.3");
        assert_eq!(format_f32(-0.5, 2, &mut out), "-0.50");
    }
}
