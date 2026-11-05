//! An argument vector in a fixed block, as the process table keeps it.

use crate::PROCESS_ARGS_MAX;

/// Most arguments one process can receive.
pub const MAX_ARGC: usize = 32;

/// Arguments stored end to end, each followed by a zero byte. Zero bytes are
/// valid for every field, so the kernel's table of these stays in `.bss`.
#[derive(Clone, Copy)]
pub struct ArgBlock {
    bytes: [u8; PROCESS_ARGS_MAX],
    len: usize,
    count: usize,
}

impl Default for ArgBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl ArgBlock {
    /// An empty block.
    pub const fn new() -> Self {
        Self {
            bytes: [0; PROCESS_ARGS_MAX],
            len: 0,
            count: 0,
        }
    }

    /// Adds one argument. Returns `false`, changing nothing, when it doesn't
    /// fit. A zero byte can't be represented, and ends the argument early.
    pub fn push(&mut self, arg: &str) -> bool {
        let arg = arg.split('\0').next().unwrap_or("");
        if self.count == MAX_ARGC || self.len + arg.len() + 1 > PROCESS_ARGS_MAX {
            return false;
        }
        self.bytes[self.len..self.len + arg.len()].copy_from_slice(arg.as_bytes());
        self.bytes[self.len + arg.len()] = 0;
        self.len += arg.len() + 1;
        self.count += 1;
        true
    }

    /// Adds every argument, or returns `false` at the first that doesn't fit.
    pub fn push_all(&mut self, args: &[&str]) -> bool {
        args.iter().all(|arg| self.push(arg))
    }

    /// Builds a block from a command line's arguments.
    ///
    /// Words split on whitespace. Single and double quotes group, and backslash
    /// escapes the next character outside single quotes. A quote with no partner
    /// is kept as an ordinary character, so `it's` stays a word. `None` when the
    /// result doesn't fit.
    pub fn parse(text: &str) -> Option<Self> {
        let mut block = Self::new();
        let mut rest = text.trim_start();
        while !rest.is_empty() {
            let end = block.parse_word(rest)?;
            rest = rest[end..].trim_start();
        }
        Some(block)
    }

    /// Stores the word at the start of `text`; returns the bytes consumed.
    fn parse_word(&mut self, text: &str) -> Option<usize> {
        if self.count == MAX_ARGC {
            return None;
        }
        let start = self.len;
        let mut quote = None;
        let mut chars = text.char_indices();
        let mut end = text.len();
        while let Some((index, ch)) = chars.next() {
            match (quote, ch) {
                (None, ch) if ch.is_whitespace() => {
                    end = index;
                    break;
                }
                (None, '\'' | '"') if text[index + 1..].contains(ch) => quote = Some(ch),
                (Some(open), ch) if ch == open => quote = None,
                (None | Some('"'), '\\') => {
                    if let Some((_, next)) = chars.next() {
                        self.push_char(next)?;
                    }
                }
                (_, ch) => self.push_char(ch)?,
            }
        }
        debug_assert!(self.len >= start);
        if self.len == PROCESS_ARGS_MAX {
            return None;
        }
        self.bytes[self.len] = 0;
        self.len += 1;
        self.count += 1;
        Some(end)
    }

    fn push_char(&mut self, ch: char) -> Option<()> {
        // Keep room for the terminator.
        if ch == '\0' || self.len + ch.len_utf8() >= PROCESS_ARGS_MAX {
            return None;
        }
        ch.encode_utf8(&mut self.bytes[self.len..]);
        self.len += ch.len_utf8();
        Some(())
    }

    /// Number of arguments.
    pub const fn len(&self) -> usize {
        self.count
    }

    /// True when there are no arguments.
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The arguments, in order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.bytes[..self.len.saturating_sub(1).max(0)]
            .split(|byte| *byte == 0)
            .take(self.count)
            .map(|word| core::str::from_utf8(word).unwrap_or(""))
    }

    /// Fills `slots` with the arguments and returns the part in use.
    pub fn argv<'a, 's>(&'a self, slots: &'s mut [&'a str; MAX_ARGC]) -> &'s [&'a str] {
        let mut count = 0;
        for (slot, word) in slots.iter_mut().zip(self.iter()) {
            *slot = word;
            count += 1;
        }
        &slots[..count]
    }

    /// Writes the arguments joined by spaces into `out` and returns them.
    pub fn joined<'o>(&self, out: &'o mut [u8; PROCESS_ARGS_MAX]) -> &'o str {
        let used = self.len.saturating_sub(1);
        out[..used].copy_from_slice(&self.bytes[..used]);
        for byte in &mut out[..used] {
            if *byte == 0 {
                *byte = b' ';
            }
        }
        core::str::from_utf8(&out[..used]).unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(block: &ArgBlock) -> std::vec::Vec<&str> {
        block.iter().collect()
    }

    #[test]
    fn pushed_arguments_keep_their_spaces_and_empties() {
        let mut block = ArgBlock::new();
        assert!(block.push_all(&["a b", "", "c"]));
        assert_eq!(words(&block), ["a b", "", "c"]);
        assert_eq!(block.len(), 3);
        let mut joined = [0u8; PROCESS_ARGS_MAX];
        assert_eq!(block.joined(&mut joined), "a b  c");
    }

    #[test]
    fn a_trailing_empty_argument_survives() {
        let mut block = ArgBlock::new();
        assert!(block.push_all(&["a", ""]));
        assert_eq!(words(&block), ["a", ""]);
        let mut block = ArgBlock::new();
        assert!(block.push(""));
        assert_eq!(words(&block), [""]);
    }

    #[test]
    fn no_arguments_is_empty() {
        let block = ArgBlock::new();
        assert!(block.is_empty());
        assert!(words(&block).is_empty());
        let mut joined = [0u8; PROCESS_ARGS_MAX];
        assert_eq!(block.joined(&mut joined), "");
    }

    #[test]
    fn parsing_honours_quotes_and_escapes() {
        let block = ArgBlock::parse("  one 'two three' \"four five\" six\\ seven ").unwrap();
        assert_eq!(words(&block), ["one", "two three", "four five", "six seven"]);
        let block = ArgBlock::parse("a''b '' x").unwrap();
        assert_eq!(words(&block), ["ab", "", "x"]);
    }

    #[test]
    fn a_lone_quote_is_an_ordinary_character() {
        let block = ArgBlock::parse("it's here").unwrap();
        assert_eq!(words(&block), ["it's", "here"]);
    }

    #[test]
    fn too_many_or_too_long_is_refused_without_panicking() {
        let long = "x".repeat(PROCESS_ARGS_MAX + 10);
        assert!(ArgBlock::parse(&long).is_none());
        let mut block = ArgBlock::new();
        assert!(!block.push(&long));
        assert!(block.is_empty());
        let many = "x ".repeat(MAX_ARGC + 1);
        assert!(ArgBlock::parse(&many).is_none());
        let exact = "x ".repeat(MAX_ARGC);
        assert_eq!(ArgBlock::parse(&exact).unwrap().len(), MAX_ARGC);
    }

    #[test]
    fn multibyte_text_stays_whole() {
        let block = ArgBlock::parse("é 'ü x'").unwrap();
        assert_eq!(words(&block), ["é", "ü x"]);
    }
}
