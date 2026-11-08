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
            // A line break in the source folds, so the source isn't the value.
            ScalarStyle::Plain if !contains(self.raw.as_bytes(), b'\n') => Some(self.raw),
            ScalarStyle::SingleQuoted
                if !contains(self.raw.as_bytes(), b'\'') && !contains(self.raw.as_bytes(), b'\n') =>
            {
                Some(self.raw)
            }
            _ => None,
        }
    }

    /// Decodes the scalar into `out`.
    ///
    /// Returns `None` if the buffer is too small or the source has an invalid
    /// escape. The parser validates escapes before constructing a document, so
    /// the latter is only possible for hand-built values.
    pub fn decode_into(self, out: &mut [u8]) -> Option<&str> {
        let mut writer = Writer { out, written: 0 };
        match self.style {
            ScalarStyle::Plain => writer.plain(self.raw.as_bytes())?,
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

    /// Folds a line break in a flow scalar: white space around it goes, a single break
    /// is a space, and each blank line after it is a break. `*i` is at the break, and
    /// nothing written before `floor` can be trimmed, as escapes wrote it.
    fn fold_break(&mut self, bytes: &[u8], i: &mut usize, floor: usize) -> Option<()> {
        while self.written > floor && matches!(self.out[self.written - 1], b' ' | b'\t' | b'\r') {
            self.written -= 1;
        }
        let mut breaks = 0usize;
        while let Some(&byte) = bytes.get(*i) {
            match byte {
                b'\n' => breaks += 1,
                b' ' | b'\t' | b'\r' => {}
                _ => break,
            }
            *i += 1;
        }
        if breaks == 1 {
            self.byte(b' ')
        } else {
            self.newlines(breaks - 1)
        }
    }

    fn plain(&mut self, bytes: &[u8]) -> Option<()> {
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\n' {
                self.fold_break(bytes, &mut i, 0)?;
                continue;
            }
            self.byte(bytes[i])?;
            i += 1;
        }
        Some(())
    }

    fn single_quoted(&mut self, bytes: &[u8]) -> Option<()> {
        let mut i = 0;
        let mut floor = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            if byte == b'\n' {
                self.fold_break(bytes, &mut i, floor)?;
                continue;
            }
            if byte == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    self.byte(b'\'')?;
                    i += 2;
                    floor = self.written;
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
        let mut floor = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            if byte == b'\n' {
                self.fold_break(bytes, &mut i, floor)?;
                continue;
            }
            if byte != b'\\' {
                self.byte(byte)?;
                i += 1;
                continue;
            }
            i += 1;
            match decode_escape(bytes, &mut i)? {
                Escape::Byte(value) => self.byte(value)?,
                Escape::Char(value) => self.char(value)?,
                Escape::LineBreak => {}
            }
            // An escaped space or tab, or what's before an escaped break, stays.
            floor = self.written;
        }
        Some(())
    }

    /// Writes a block scalar's body, before chomping.
    ///
    /// Only leading spaces are removed, so a short line can't cut into text or
    /// split a character. Folding follows YAML: runs of ordinary lines join
    /// with a space, blank lines become line breaks, and lines indented beyond
    /// the content keep their breaks.
    // Offsets here come from byte scans that stop only at ASCII bytes or the end, so they fall on character boundaries.
    #[allow(clippy::string_slice)]
    fn block(&mut self, raw: &str, indent: u8, folded: bool) -> Option<()> {
        #[derive(PartialEq)]
        enum Previous {
            Nothing,
            Ordinary,
            MoreIndented,
        }

        let indent = indent as usize;
        let mut previous = Previous::Nothing;
        let mut blanks = 0usize;

        for line in raw.split_inclusive('\n') {
            let line = line.strip_suffix('\n').unwrap_or(line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let spaces = line.bytes().take(indent).take_while(|&b| b == b' ').count();
            let content = &line[spaces..];
            if content.is_empty() {
                blanks += 1;
                continue;
            }

            let more_indented = folded && content.starts_with([' ', '\t']);
            match previous {
                Previous::Nothing => self.newlines(blanks)?,
                Previous::Ordinary if folded && !more_indented => {
                    if blanks == 0 {
                        self.byte(b' ')?;
                    } else {
                        self.newlines(blanks)?;
                    }
                }
                _ => self.newlines(blanks + 1)?,
            }
            self.bytes(content.as_bytes())?;
            previous = if more_indented {
                Previous::MoreIndented
            } else {
                Previous::Ordinary
            };
            blanks = 0;
        }

        // Every line after the last text ends in a break, and so does the text
        // itself when the source does.
        let final_break = previous != Previous::Nothing && raw.ends_with('\n');
        self.newlines(blanks + final_break as usize)
    }

    fn newlines(&mut self, count: usize) -> Option<()> {
        for _ in 0..count {
            self.byte(b'\n')?;
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
                    // Keeps the final break only if the source had one.
                    if trailing > 0 && trailing < written {
                        written -= trailing - 1;
                    } else if trailing == written {
                        written = 0;
                    }
                }
                Chomp::Keep => {}
            }
        }
        let bytes: &'b [u8] = &out[..written];
        core::str::from_utf8(bytes).ok()
    }
}

/// One decoded double-quoted escape.
pub(crate) enum Escape {
    Byte(u8),
    Char(char),
    /// An escaped line break, which produces nothing.
    LineBreak,
}

/// Decodes the escape whose character (after the backslash) is at `*i`.
///
/// Returns `None` for an escape YAML does not define, which is what the parser
/// reports as `InvalidEscape`.
pub(crate) fn decode_escape(bytes: &[u8], i: &mut usize) -> Option<Escape> {
    let escape = *bytes.get(*i)?;
    *i += 1;
    Some(match escape {
        b'0' => Escape::Byte(0),
        b'a' => Escape::Byte(0x07),
        b'b' => Escape::Byte(0x08),
        b't' | b'\t' => Escape::Byte(b'\t'),
        b'n' => Escape::Byte(b'\n'),
        b'v' => Escape::Byte(0x0b),
        b'f' => Escape::Byte(0x0c),
        b'r' => Escape::Byte(b'\r'),
        b'e' => Escape::Byte(0x1b),
        b' ' => Escape::Byte(b' '),
        b'"' => Escape::Byte(b'"'),
        b'/' => Escape::Byte(b'/'),
        b'\\' => Escape::Byte(b'\\'),
        b'N' => Escape::Char('\u{85}'),
        b'_' => Escape::Char('\u{a0}'),
        b'L' => Escape::Char('\u{2028}'),
        b'P' => Escape::Char('\u{2029}'),
        b'x' => Escape::Char(char::from_u32(read_hex(bytes, i, 2)?)?),
        b'u' => Escape::Char(char::from_u32(read_hex16(bytes, i)?)?),
        b'U' => Escape::Char(char::from_u32(read_hex32(bytes, i)?)?),
        b'\n' => {
            // Escaped line break: skip following indentation.
            while matches!(bytes.get(*i), Some(b' ' | b'\t')) {
                *i += 1;
            }
            Escape::LineBreak
        }
        _ => return None,
    })
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

    #[test]
    fn folded_block_keeps_breaks_around_more_indented_lines() {
        let scalar = YamlScalar::block(
            "  one\n  two\n    code\n  three\n",
            ScalarStyle::Folded,
            2,
            Chomp::Clip,
        );
        assert_eq!(decode(scalar), "one two\n  code\nthree\n");
    }

    #[test]
    fn short_lines_never_lose_text_or_split_characters() {
        // Hand-built spans can hold lines shorter than the indent.
        let scalar =
            YamlScalar::block("    four\n  \u{e9}\n", ScalarStyle::Literal, 4, Chomp::Clip);
        assert_eq!(decode(scalar), "four\n\u{e9}\n");
    }

    #[test]
    fn chomping_rules() {
        let block = |raw, chomp| YamlScalar::block(raw, ScalarStyle::Literal, 2, chomp);
        assert_eq!(decode(block("  a\n\n\n", Chomp::Clip)), "a\n");
        assert_eq!(decode(block("  a\n\n\n", Chomp::Strip)), "a");
        assert_eq!(decode(block("  a\n\n\n", Chomp::Keep)), "a\n\n\n");
        assert_eq!(decode(block("  a", Chomp::Clip)), "a");
        assert_eq!(decode(block("\n", Chomp::Clip)), "");
        assert_eq!(decode(block("  a\r\n  b\r\n", Chomp::Clip)), "a\nb\n");
    }
}
