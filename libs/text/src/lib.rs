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

use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

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
