//! YAML scalars and scalar decoding.

/// How a scalar was written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScalarStyle {
    /// An unquoted scalar.
    Plain,
    /// A single-quoted scalar (`''` escapes a quote).
    SingleQuoted,
    /// A double-quoted scalar with backslash escapes.
    DoubleQuoted,
    /// A literal block scalar (`|`).
    Literal,
    /// A folded block scalar (`>`).
    Folded,
}

/// Trailing-newline handling for block scalars.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chomp {
    /// Keep exactly one trailing newline (the default).
    Clip,
    /// Remove trailing newlines (`-`).
    Strip,
    /// Keep all trailing newlines (`+`).
    Keep,
}

/// A scalar value borrowed from the source document.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct YamlScalar<'a> {
    raw: &'a str,
    style: ScalarStyle,
    indent: u8,
    chomp: Chomp,
}

impl<'a> YamlScalar<'a> {
    pub const fn plain(raw: &'a str) -> Self {
        Self {
            raw,
            style: ScalarStyle::Plain,
            indent: 0,
            chomp: Chomp::Clip,
        }
    }

    pub const fn styled(raw: &'a str, style: ScalarStyle) -> Self {
        Self {
            raw,
            style,
            indent: 0,
            chomp: Chomp::Clip,
        }
    }

    pub const fn block(raw: &'a str, style: ScalarStyle, indent: u8, chomp: Chomp) -> Self {
        Self {
            raw,
            style,
            indent,
            chomp,
        }
    }

    /// The raw source span. For block scalars this includes line breaks and
    /// indentation; use [`decode_into`](Self::decode_into) for the value.
    pub const fn raw(self) -> &'a str {
        self.raw
    }

    /// The scalar style.
    pub const fn style(self) -> ScalarStyle {
        self.style
    }

    /// Returns the value when it needs no decoding.
    pub const fn as_borrowed(self) -> Option<&'a str> {
        match self.style {
            ScalarStyle::Plain => Some(self.raw),
            ScalarStyle::SingleQuoted if !contains(self.raw.as_bytes(), b'\'') => Some(self.raw),
            _ => None,
        }
    }

    /// Decodes the scalar into `out`.
    ///
    /// Returns `None` if the buffer is too small or the source has an invalid
    /// escape. The parser validates escapes before constructing a document, so
    /// the latter is only possible for hand-built values.
    pub fn decode_into<'b>(self, out: &'b mut [u8]) -> Option<&'b str> {
        let mut writer = Writer { out, written: 0 };
        match self.style {
            ScalarStyle::Plain => writer.bytes(self.raw.as_bytes())?,
            ScalarStyle::SingleQuoted => writer.single_quoted(self.raw.as_bytes())?,
            ScalarStyle::DoubleQuoted => writer.double_quoted(self.raw.as_bytes())?,
            ScalarStyle::Literal => writer.block(self.raw, self.indent, false)?,
            ScalarStyle::Folded => writer.block(self.raw, self.indent, true)?,
        }
        writer.finish(self)
    }
}

const fn contains(bytes: &[u8], needle: u8) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == needle {
            return true;
        }
        i += 1;
    }
    false
}

struct Writer<'b> {
    out: &'b mut [u8],
    written: usize,
}

impl<'b> Writer<'b> {
    fn byte(&mut self, byte: u8) -> Option<()> {
        *self.out.get_mut(self.written)? = byte;
        self.written += 1;
        Some(())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.written.checked_add(bytes.len())?;
        self.out.get_mut(self.written..end)?.copy_from_slice(bytes);
        self.written = end;
        Some(())
    }

    fn char(&mut self, character: char) -> Option<()> {
        let mut buffer = [0u8; 4];
        self.bytes(character.encode_utf8(&mut buffer).as_bytes())
    }

    fn single_quoted(&mut self, bytes: &[u8]) -> Option<()> {
        let mut i = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            if byte == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    self.byte(b'\'')?;
                    i += 2;
                    continue;
                }
                return None;
            }
            self.byte(byte)?;
            i += 1;
        }
        Some(())
    }

    fn double_quoted(&mut self, bytes: &[u8]) -> Option<()> {
        let mut i = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            if byte != b'\\' {
                self.byte(byte)?;
                i += 1;
                continue;
            }
            i += 1;
            let escape = *bytes.get(i)?;
            i += 1;
            match escape {
                b'0' => self.byte(0)?,
                b'a' => self.byte(0x07)?,
                b'b' => self.byte(0x08)?,
                b't' => self.byte(b'\t')?,
                b'n' => self.byte(b'\n')?,
                b'v' => self.byte(0x0b)?,
                b'f' => self.byte(0x0c)?,
                b'r' => self.byte(b'\r')?,
                b'e' => self.byte(0x1b)?,
                b' ' => self.byte(b' ')?,
                b'"' => self.byte(b'"')?,
                b'/' => self.byte(b'/')?,
                b'\\' => self.byte(b'\\')?,
                b'N' => self.char('\u{85}')?,
                b'_' => self.char('\u{a0}')?,
                b'L' => self.char('\u{2028}')?,
                b'P' => self.char('\u{2029}')?,
                b'x' => {
                    let value = read_hex(bytes, &mut i, 2)?;
                    self.char(char::from_u32(value)?)?;
                }
                b'u' => {
                    let value = read_hex16(bytes, &mut i)?;
                    self.char(char::from_u32(value)?)?;
                }
                b'U' => {
                    let value = read_hex32(bytes, &mut i)?;
                    self.char(char::from_u32(value)?)?;
                }
                b'\n' => {
                    // Escaped line break: skip following indentation.
                    while matches!(bytes.get(i), Some(b' ' | b'\t')) {
                        i += 1;
                    }
                }
                _ => return None,
            }
        }
        Some(())
    }

    fn block(&mut self, raw: &str, indent: u8, folded: bool) -> Option<()> {
        let ends_newline = raw.ends_with('\n');
        let body = if ends_newline {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        let indent = indent as usize;
        let mut previous_blank = true;
        let mut first = true;

        for line in body.split('\n') {
            let content = if line.len() >= indent {
                &line[indent..]
            } else {
                ""
            };
            let blank = content.trim().is_empty();
            if !first {
                if folded {
                    if blank {
                        self.byte(b'\n')?;
                    } else if !previous_blank {
                        self.byte(b' ')?;
                    }
                } else {
                    self.byte(b'\n')?;
                }
            }
            self.bytes(content.as_bytes())?;
            previous_blank = blank;
            first = false;
        }

        match self_finish_block(self, ends_newline, folded) {
            Some(()) => {}
            None => return None,
        }
        Some(())
    }

    fn finish(self, scalar: YamlScalar<'_>) -> Option<&'b str> {
        let Writer { out, mut written } = self;
        if matches!(scalar.style, ScalarStyle::Literal | ScalarStyle::Folded) {
            let trailing = trailing_newlines(&out[..written]);
            match scalar.chomp {
                Chomp::Strip => written -= trailing,
                Chomp::Clip => {
                    written -= trailing;
                    if written > 0 {
                        *out.get_mut(written)? = b'\n';
                        written += 1;
                    }
                }
                Chomp::Keep => {}
            }
        }
        let bytes: &'b [u8] = &out[..written];
        core::str::from_utf8(bytes).ok()
    }
}

// Kept separate to make the block writer's chomp rules explicit.
fn self_finish_block(writer: &mut Writer<'_>, ends_newline: bool, _folded: bool) -> Option<()> {
    if ends_newline {
        writer.byte(b'\n')?;
    }
    Some(())
}

fn trailing_newlines(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'\n')
        .count()
}

fn read_hex(bytes: &[u8], i: &mut usize, count: usize) -> Option<u32> {
    let digits = bytes.get(*i..*i + count)?;
    let mut value = 0u32;
    for &digit in digits {
        let nibble = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            b'A'..=b'F' => digit - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | nibble as u32;
    }
    *i += count;
    Some(value)
}

fn read_hex16(bytes: &[u8], i: &mut usize) -> Option<u32> {
    let high = read_hex(bytes, i, 4)?;
    if (0xd800..=0xdbff).contains(&high) {
        if bytes.get(*i) != Some(&b'\\') || bytes.get(*i + 1) != Some(&b'u') {
            return None;
        }
        *i += 2;
        let low = read_hex(bytes, i, 4)?;
        if !(0xdc00..=0xdfff).contains(&low) {
            return None;
        }
        Some(0x1_0000 + (((high - 0xd800) << 10) | (low - 0xdc00)))
    } else if (0xdc00..=0xdfff).contains(&high) {
        None
    } else {
        Some(high)
    }
}

fn read_hex32(bytes: &[u8], i: &mut usize) -> Option<u32> {
    read_hex(bytes, i, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(scalar: YamlScalar<'_>) -> String {
        let mut out = [0u8; 256];
        scalar.decode_into(&mut out).unwrap().to_string()
    }

    #[test]
    fn quoted_scalars_decode() {
        assert_eq!(decode(YamlScalar::plain("hello")), "hello");
        assert_eq!(
            decode(YamlScalar::styled("it''s", ScalarStyle::SingleQuoted)),
            "it's"
        );
        assert_eq!(
            decode(YamlScalar::styled(
                r#"a\nb\u20ac"#,
                ScalarStyle::DoubleQuoted
            )),
            "a\nb€"
        );
    }

    #[test]
    fn literal_block_preserves_lines() {
        let scalar = YamlScalar::block("  one\n  two\n", ScalarStyle::Literal, 2, Chomp::Clip);
        assert_eq!(decode(scalar), "one\ntwo\n");
        let scalar = YamlScalar::block("  one\n  two\n", ScalarStyle::Literal, 2, Chomp::Strip);
        assert_eq!(decode(scalar), "one\ntwo");
    }

    #[test]
    fn folded_block_folds_lines() {
        let scalar = YamlScalar::block(
            "  one\n  two\n\n  three\n",
            ScalarStyle::Folded,
            2,
            Chomp::Clip,
        );
        assert_eq!(decode(scalar), "one two\nthree\n");
    }
}
