//! Fixed-capacity, allocation-free UTF-8 strings.
//!
//! The feos kernel has no allocator, but configuration files, manifests and
//! process names are all text. [`Text`] stores as much of a string as fits in
//! a caller-chosen `N`-byte buffer and never allocates. Truncation happens on
//! a UTF-8 character boundary, so [`Text::as_str`] always succeeds.
//!
//! ```
//! use feos_text::Text;
//!
//! let mut name = Text::<8>::new();
//! name.set("feos desktop");
//! assert_eq!(name.as_str(), "feos des");
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

/// Whether a character does something to whatever shows it, instead of being shown.
pub fn is_control(ch: char) -> bool {
    ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// How many terminal cells a character takes: 0 for a combining mark or other character
/// with no width of its own, 2 for East Asian wide characters and emoji, 1 for the rest.
///
/// This is the usual `wcwidth` by ranges, not a full table: enough that columns line up for
/// what's common, and for a run of wide text that this system has no glyphs for.
pub fn char_width(ch: char) -> usize {
    let code = ch as u32;
    if code < 0x300 {
        return 1;
    }
    let zero = matches!(code,
        0x300..=0x36f | 0x483..=0x489 | 0x591..=0x5bd | 0x5bf | 0x5c1..=0x5c2 | 0x5c4..=0x5c5
        | 0x610..=0x61a | 0x64b..=0x65f | 0x670 | 0x6d6..=0x6dc | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff
        | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2064 | 0x20d0..=0x20f0 | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f | 0xfeff | 0xe0100..=0xe01ef);
    if zero {
        return 0;
    }
    let wide = matches!(code,
        0x1100..=0x115f | 0x2e80..=0x303e | 0x3041..=0x33ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe30..=0xfe6f | 0xff00..=0xff60
        | 0xffe0..=0xffe6 | 0x1f300..=0x1f64f | 0x1f680..=0x1f6ff | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd);
    if wide {
        2
    } else {
        1
    }
}

/// A name, a piece of text from outside such as a file's, ready to be shown.
///
/// Filesystems keep names as they are, and whatever shows one escapes it here, so a name
/// can't move a terminal's cursor, set its colours or break a line of a listing:
///
/// ```
/// assert_eq!(feos_text::Escaped("a\x1b[2Jb\nc").to_string(), "a\\e[2Jb\\nc");
/// assert_eq!(feos_text::Escaped("plain h\u{e9}llo").to_string(), "plain h\u{e9}llo");
/// ```
pub struct Escaped<'a>(pub &'a str);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write as _;
        for ch in self.0.chars() {
            match ch {
                '\n' => out.write_str("\\n")?,
                '\r' => out.write_str("\\r")?,
                '\t' => out.write_str("\\t")?,
                '\u{1b}' => out.write_str("\\e")?,
                ch if !is_control(ch) => out.write_char(ch)?,
                ch if (ch as u32) < 0x100 => write!(out, "\\x{:02x}", ch as u32)?,
                ch => write!(out, "\\u{{{:x}}}", ch as u32)?,
            }
        }
        Ok(())
    }
}

/// [`Escaped`] as text, which only costs anything for a name that needs it.
pub fn display_name(name: &str) -> Cow<'_, str> {
    if !name.chars().any(is_control) {
        return Cow::Borrowed(name);
    }
    use fmt::Write as _;
    let mut shown = String::with_capacity(name.len() + 8);
    let _ = write!(shown, "{}", Escaped(name));
    Cow::Owned(shown)
}

/// A UTF-8 string with a fixed maximum byte length.
///
/// `N` includes no terminator: a `Text<8>` can hold eight bytes. Longer input
/// is truncated on a character boundary by [`Text::set`].
///
/// Equality and hashing look only at the current contents, so bytes left over
/// from a longer earlier value never affect them.
#[derive(Clone, Copy)]
pub struct Text<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Text<N> {
    /// An empty string.
    pub const EMPTY: Self = Self {
        bytes: [0; N],
        len: 0,
    };

    /// Creates an empty string.
    pub const fn new() -> Self {
        Self::EMPTY
    }

    /// Creates a string from `value`, truncating it to fit.
    pub fn from_str(value: &str) -> Self {
        let mut text = Self::EMPTY;
        text.set(value);
        text
    }

    /// Replaces the contents with `value`, truncating it to fit.
    ///
    /// Returns `true` when the whole value fit.
    pub fn set(&mut self, value: &str) -> bool {
        let bytes = value.as_bytes();
        let whole = bytes.len() <= N;
        let mut len = bytes.len().min(N);
        while len > 0 && !value.is_char_boundary(len) {
            len -= 1;
        }
        self.bytes[..len].copy_from_slice(&bytes[..len]);
        self.len = len;
        whole && len == bytes.len()
    }

    /// Appends `value`, truncating on a character boundary.
    ///
    /// Returns `true` when the whole value fit.
    pub fn push_str(&mut self, value: &str) -> bool {
        let available = N.saturating_sub(self.len);
        let bytes = value.as_bytes();
        let mut len = bytes.len().min(available);
        while len > 0 && !value.is_char_boundary(len) {
            len -= 1;
        }
        self.bytes[self.len..self.len + len].copy_from_slice(&bytes[..len]);
        self.len += len;
        len == bytes.len()
    }

    /// Appends one character.
    pub fn push(&mut self, ch: char) -> bool {
        let mut buffer = [0u8; 4];
        self.push_str(ch.encode_utf8(&mut buffer))
    }

    /// Removes the last character and returns it.
    pub fn pop(&mut self) -> Option<char> {
        if self.len == 0 {
            return None;
        }
        // `Text` only ever stores valid UTF-8, so the prefix is a valid string.
        let text = core::str::from_utf8(&self.bytes[..self.len]).ok()?;
        let ch = text.chars().next_back()?;
        self.len -= ch.len_utf8();
        Some(ch)
    }

    /// Removes all characters.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// The string contents.
    pub fn as_str(&self) -> &str {
        // SAFETY-free invariant: `set`/`push_str` only copy complete UTF-8
        // sequences and truncate at character boundaries.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// Number of bytes in use.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when the string is empty.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The maximum number of bytes the string can hold.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Returns `true` when the whole string equals `other`.
    pub fn eq_str(&self, other: &str) -> bool {
        self.as_str() == other
    }

    /// Returns `true` when the string equals `other`, ignoring ASCII case.
    pub fn eq_ignore_ascii_case(&self, other: &str) -> bool {
        self.as_str().eq_ignore_ascii_case(other)
    }

    /// The string without leading and trailing ASCII whitespace.
    pub fn trim(&self) -> &str {
        self.as_str().trim()
    }

    /// Parses the contents as a boolean (`true`/`yes`/`on`/`false`/...).
    pub fn as_bool(&self) -> Option<bool> {
        let text = self.trim();
        if text.eq_ignore_ascii_case("true")
            || text.eq_ignore_ascii_case("yes")
            || text.eq_ignore_ascii_case("on")
            || text == "1"
        {
            Some(true)
        } else if text.eq_ignore_ascii_case("false")
            || text.eq_ignore_ascii_case("no")
            || text.eq_ignore_ascii_case("off")
            || text == "0"
        {
            Some(false)
        } else {
            None
        }
    }

    /// Parses the contents as an unsigned integer.
    pub fn as_u32(&self) -> Option<u32> {
        self.trim().parse().ok()
    }

    /// Parses the contents as a signed integer.
    pub fn as_i32(&self) -> Option<i32> {
        self.trim().parse().ok()
    }

    /// Copies the string into `out`, returning the written prefix.
    pub fn copy_into<'a>(&self, out: &'a mut [u8]) -> &'a str {
        let len = self.len.min(out.len());
        let mut len = len;
        while len > 0 && !self.as_str().is_char_boundary(len) {
            len -= 1;
        }
        out[..len].copy_from_slice(&self.bytes[..len]);
        core::str::from_utf8(&out[..len]).unwrap_or("")
    }
}

impl<const N: usize> Default for Text<N> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<const N: usize> Deref for Text<N> {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> fmt::Debug for Text<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> fmt::Display for Text<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> PartialEq for Text<N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const N: usize> Eq for Text<N> {}

impl<const N: usize> Hash for Text<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

/// Writing reports [`fmt::Error`] when the value did not fit. The prefix that
/// fit on a character boundary is kept, so callers that only want a best-effort
/// label can ignore the result.
impl<const N: usize> fmt::Write for Text<N> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.push_str(value) {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

impl<const N: usize> PartialEq<str> for Text<N> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<const N: usize> PartialEq<&str> for Text<N> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_are_what_a_terminal_expects() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('\u{e9}'), 1);
        assert_eq!(char_width('\u{3b1}'), 1);
        assert_eq!(char_width('\u{416}'), 1);
        assert_eq!(char_width('\u{2500}'), 1);
        assert_eq!(char_width('\u{301}'), 0);
        assert_eq!(char_width('\u{200b}'), 0);
        assert_eq!(char_width('\u{4e2d}'), 2);
        assert_eq!(char_width('\u{ac00}'), 2);
        assert_eq!(char_width('\u{1f600}'), 2);
    }

    #[test]
    fn names_are_escaped_only_where_it_matters() {
        assert!(matches!(display_name("fine.txt"), Cow::Borrowed("fine.txt")));
        assert!(matches!(display_name("Gr\u{fc}\u{df}e \u{3b1}"), Cow::Borrowed(_)));
        assert_eq!(display_name("a\x07b\x7f\u{85}"), "a\\x07b\\x7f\\x85");
        assert_eq!(display_name("\u{202e}evil"), "\\u{202e}evil");
        assert_eq!(display_name("x\ty\rz\n"), "x\\ty\\rz\\n");
    }
    use core::fmt::Write;

    #[test]
    fn truncates_on_char_boundaries() {
        let mut text = Text::<4>::new();
        assert!(!text.set("héllo"));
        assert_eq!(text.as_str(), "hél");
        assert!(text.set("abcd"));
        assert_eq!(text.as_str(), "abcd");
    }

    #[test]
    fn push_and_format() {
        let mut text = Text::<16>::new();
        assert!(text.push_str("feos"));
        assert!(write!(text, " {}", 42).is_ok());
        assert_eq!(text.as_str(), "feos 42");
        assert_eq!(text.len(), 7);
    }

    #[test]
    fn parse_helpers() {
        let mut text = Text::<8>::new();
        text.set("yes");
        assert_eq!(text.as_bool(), Some(true));
        text.set(" 42 ");
        assert_eq!(text.as_u32(), Some(42));
        assert_eq!(text.trim(), "42");
    }

    #[test]
    fn pop_removes_whole_characters() {
        let mut text = Text::<8>::new();
        text.set("hé");
        assert_eq!(text.pop(), Some('é'));
        assert_eq!(text.as_str(), "h");
        assert_eq!(text.pop(), Some('h'));
        assert_eq!(text.pop(), None);
        assert!(text.is_empty());
    }

    #[test]
    fn equality_ignores_bytes_left_by_a_longer_value() {
        let mut text = Text::<8>::new();
        text.set("abcd");
        text.set("ab");
        assert!(text == Text::<8>::from_str("ab"));
    }

    #[test]
    fn hash_ignores_bytes_left_by_a_longer_value() {
        use std::collections::hash_map::DefaultHasher;

        fn hash_of(text: &Text<8>) -> u64 {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            hasher.finish()
        }

        let mut text = Text::<8>::new();
        text.set("abcd");
        text.clear();
        text.push_str("ab");
        assert_eq!(hash_of(&text), hash_of(&Text::from_str("ab")));
    }

    #[test]
    fn formatting_reports_truncation_and_stops() {
        let mut text = Text::<4>::new();
        assert!(write!(text, "{}{}", "abcdef", "x").is_err());
        assert_eq!(text.as_str(), "abcd");
        // Nothing after the failed piece may be appended.
        let mut text = Text::<4>::new();
        assert!(write!(text, "{}{}", "éé€", "x").is_err());
        assert_eq!(text.as_str(), "éé");
    }

    #[test]
    fn empty_and_copy() {
        let text = Text::<8>::EMPTY;
        assert!(text.is_empty());
        assert_eq!(text.capacity(), 8);
        let mut out = [0u8; 2];
        let mut text = Text::<8>::new();
        text.set("hello");
        assert_eq!(text.copy_into(&mut out), "he");
    }
}
