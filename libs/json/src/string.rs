//! Borrowed JSON strings and escape decoding.

/// A JSON string as a span of the source document.
///
/// `raw` contains the bytes between the quotes, including backslash escapes.
/// If [`is_escaped`](Self::is_escaped) is `false`, the span is the decoded
/// string and can be used without copying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JsonString<'a> {
    raw: &'a str,
    escaped: bool,
}

impl<'a> JsonString<'a> {
    /// Creates a string from a raw span.
    ///
    /// `escaped` must be `true` when the span contains backslash escapes.
    pub const fn new(raw: &'a str, escaped: bool) -> Self {
        Self { raw, escaped }
    }

    /// The raw source span, without the surrounding quotes.
    pub const fn raw(self) -> &'a str {
        self.raw
    }

    /// Returns `true` when the span contains backslash escapes.
    pub const fn is_escaped(self) -> bool {
        self.escaped
    }

    /// Returns the decoded string when no escapes are present.
    pub const fn as_borrowed(self) -> Option<&'a str> {
        if self.escaped {
            None
        } else {
            Some(self.raw)
        }
    }

    /// Decodes the string into `out` and returns it as a `str`.
    ///
    /// Even an escape-free string is copied, so the returned slice always
    /// lives as long as `out`. Use [`as_borrowed`](Self::as_borrowed) when a
    /// zero-copy path is desired.
    ///
    /// Returns `None` when `out` is too small or the source contains an
    /// invalid escape. The parser validates escapes before a document can be
    /// built, so the latter only happens for hand-built strings.
    pub fn decode_into<'b>(self, out: &'b mut [u8]) -> Option<&'b str> {
        let mut written = 0usize;
        let bytes = self.raw.as_bytes();
        let mut i = 0usize;

        while i < bytes.len() {
            let byte = bytes[i];
            if byte != b'\\' {
                if written >= out.len() {
                    return None;
                }
                out[written] = byte;
                written += 1;
                i += 1;
                continue;
            }

            i += 1;
            let escape = *bytes.get(i)?;
            i += 1;
            let decoded = match escape {
                b'"' => b'"',
                b'\\' => b'\\',
                b'/' => b'/',
                b'b' => 0x08,
                b'f' => 0x0c,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'u' => {
                    let (character, consumed) = decode_unicode(bytes, i)?;
                    i += consumed;
                    write_char(character, out, &mut written)?;
                    continue;
                }
                _ => return None,
            };
            if written >= out.len() {
                return None;
            }
            out[written] = decoded;
            written += 1;
        }

        core::str::from_utf8(&out[..written]).ok()
    }
}

fn decode_unicode(bytes: &[u8], mut i: usize) -> Option<(char, usize)> {
    let first = decode_hex4(bytes, i)?;
    i += 4;
    let (code, consumed) = if (0xd800..=0xdbff).contains(&first) {
        if bytes.get(i) != Some(&b'\\') || bytes.get(i + 1) != Some(&b'u') {
            return None;
        }
        let second = decode_hex4(bytes, i + 2)?;
        if !(0xdc00..=0xdfff).contains(&second) {
            return None;
        }
        let combined = 0x1_0000 + (((first as u32 - 0xd800) << 10) | (second as u32 - 0xdc00));
        (combined, 10)
    } else if (0xdc00..=0xdfff).contains(&first) {
        return None;
    } else {
        (first as u32, 4)
    };
    Some((char::from_u32(code)?, consumed))
}

fn decode_hex4(bytes: &[u8], i: usize) -> Option<u16> {
    let digits = bytes.get(i..i + 4)?;
    let mut value = 0u16;
    for &digit in digits {
        value = (value << 4) | hex_value(digit)? as u16;
    }
    Some(value)
}

pub(crate) fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn write_char(character: char, out: &mut [u8], written: &mut usize) -> Option<()> {
    let mut encoded = [0u8; 4];
    let text = character.encode_utf8(&mut encoded);
    let end = written.checked_add(text.len())?;
    out.get_mut(*written..end)?.copy_from_slice(text.as_bytes());
    *written = end;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_string_is_borrowed() {
        let string = JsonString::new("hello", false);
        assert_eq!(string.as_borrowed(), Some("hello"));
        assert!(!string.is_escaped());
    }

    #[test]
    fn escapes_decode() {
        let string = JsonString::new(r#"a\nb\t\"c\\d\/"#, true);
        let mut out = [0u8; 32];
        assert_eq!(string.decode_into(&mut out), Some("a\nb\t\"c\\d/"));
    }

    #[test]
    fn unicode_and_surrogate_pairs_decode() {
        let string = JsonString::new(r#"\u00e9 \u20ac \ud83d\ude00"#, true);
        let mut out = [0u8; 32];
        assert_eq!(string.decode_into(&mut out), Some("é € 😀"));
    }

    #[test]
    fn invalid_escapes_and_small_buffers_fail() {
        let string = JsonString::new(r#"\q"#, true);
        let mut out = [0u8; 8];
        assert_eq!(string.decode_into(&mut out), None);

        let string = JsonString::new("hello", false);
        let mut tiny = [0u8; 2];
        assert_eq!(string.decode_into(&mut tiny), None);
    }
}
