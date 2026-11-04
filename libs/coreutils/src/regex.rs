//! A POSIX regular-expression engine.
//!
//! Patterns compile to a small instruction list that a backtracking matcher
//! runs with an explicit heap stack, so neither a deep pattern nor a long line
//! can overflow a kernel stack. Supported syntax, in both POSIX flavours:
//!
//! - literals, `.`, bracket expressions with ranges and `[:classes:]`
//! - `*`, intervals `{m,n}`, groups, alternation and back-references
//! - `+` and `?` (`\+` and `\?` in basic syntax)
//! - `^`, `$`, `\b`, `\B`, `\<`, `\>`, `\w`, `\W`, `\s`, `\S`
//!
//! Matching is byte-oriented so it works on any file, but `.` and negated
//! classes consume a whole UTF-8 sequence. Results are leftmost-longest, as
//! POSIX requires. A visited-state bitmap keeps ordinary patterns linear in
//! `pattern × text`; patterns with back-references have no such bound and run
//! under a step limit instead.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::glob::posix_class;

/// Which POSIX pattern language to parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Syntax {
    /// Basic regular expressions, as `grep` and `sed` use by default.
    Basic,
    /// Extended regular expressions, as `grep -E` and `sed -E` use.
    Extended,
    /// No operators at all, as `grep -F` uses.
    Fixed,
}

/// Compile-time options.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The pattern language.
    pub syntax: Syntax,
    /// Case-insensitive matching.
    pub ignore_case: bool,
    /// The match must not touch a word character on either side (`grep -w`).
    pub whole_word: bool,
    /// The match must span the whole text (`grep -x`).
    pub whole_line: bool,
}

impl Options {
    /// Options for a plain pattern in `syntax`.
    pub const fn new(syntax: Syntax) -> Self {
        Self {
            syntax,
            ignore_case: false,
            whole_word: false,
            whole_line: false,
        }
    }
}

const MAX_GROUP_DEPTH: usize = 32;
const MAX_PROGRAM: usize = 16 * 1024;
const MAX_REPEAT: u32 = 1024;
const MAX_MEMO_BYTES: usize = 1024 * 1024;
const MAX_STEPS: usize = 8_000_000;
const MAX_STACK: usize = 256 * 1024;

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Byte(u8),
    Any,
    Class(Box<ByteSet>),
    Group(Box<Node>, Option<u32>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
    Bol,
    Eol,
    WordBoundary,
    NotWordBoundary,
    WordStart,
    WordEnd,
    Backref(u32),
}

/// A set of bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ByteSet {
    bits: [u32; 8],
    /// A whole UTF-8 sequence is consumed when the first byte is non-ASCII.
    whole_char: bool,
}

impl ByteSet {
    const fn new() -> Self {
        Self {
            bits: [0; 8],
            whole_char: false,
        }
    }

    fn insert(&mut self, byte: u8) {
        self.bits[usize::from(byte >> 5)] |= 1 << (byte & 31);
    }

    fn insert_range(&mut self, low: u8, high: u8) {
        for byte in low..=high {
            self.insert(byte);
        }
    }

    fn contains(&self, byte: u8) -> bool {
        self.bits[usize::from(byte >> 5)] & (1 << (byte & 31)) != 0
    }

    fn negate(&mut self) {
        for word in &mut self.bits {
            *word = !*word;
        }
        self.whole_char = true;
    }

    fn fold_case(&mut self) {
        for byte in b'a'..=b'z' {
            let upper = byte.to_ascii_uppercase();
            if self.contains(byte) || self.contains(upper) {
                self.insert(byte);
                self.insert(upper);
            }
        }
    }
}

struct Parser<'a> {
    text: &'a [u8],
    index: usize,
    syntax: Syntax,
    ignore_case: bool,
    groups: u32,
    depth: usize,
    has_backref: bool,
}

type ParseResult<T> = Result<T, &'static str>;

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.index).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.text.get(self.index + offset).copied()
    }

    fn extended(&self) -> bool {
        self.syntax == Syntax::Extended
    }

    fn at_alternation(&self) -> bool {
        if self.extended() {
            self.peek() == Some(b'|')
        } else {
            self.peek() == Some(b'\\') && self.peek_at(1) == Some(b'|')
        }
    }

    fn at_group_close(&self) -> bool {
        if self.extended() {
            self.peek() == Some(b')') && self.depth > 0
        } else {
            self.peek() == Some(b'\\') && self.peek_at(1) == Some(b')')
        }
    }

    fn parse_alternation(&mut self) -> ParseResult<Node> {
        let mut branches = Vec::new();
        loop {
            branches.push(self.parse_concat()?);
            if !self.at_alternation() {
                break;
            }
            self.index += if self.extended() { 1 } else { 2 };
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alt(branches)
        })
    }

    fn parse_concat(&mut self) -> ParseResult<Node> {
        let mut items: Vec<Node> = Vec::new();
        let mut at_start = true;
        while self.peek().is_some() && !self.at_alternation() && !self.at_group_close() {
            let atom = self.parse_atom(at_start)?;
            let atom = self.parse_repeats(atom)?;
            at_start = matches!(atom, Node::Bol);
            items.push(atom);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    fn parse_repeats(&mut self, mut atom: Node) -> ParseResult<Node> {
        // Anchors and assertions repeat as themselves in practice.
        if matches!(atom, Node::Bol) && !self.extended() {
            return Ok(atom);
        }
        loop {
            let (min, max) = match self.peek() {
                Some(b'*') => {
                    self.index += 1;
                    (0, None)
                }
                Some(b'+') if self.extended() => {
                    self.index += 1;
                    (1, None)
                }
                Some(b'?') if self.extended() => {
                    self.index += 1;
                    (0, Some(1))
                }
                Some(b'{') if self.extended() => match self.parse_interval(1)? {
                    Some(bounds) => bounds,
                    None => return Ok(atom),
                },
                Some(b'\\') if !self.extended() => match self.peek_at(1) {
                    Some(b'+') => {
                        self.index += 2;
                        (1, None)
                    }
                    Some(b'?') => {
                        self.index += 2;
                        (0, Some(1))
                    }
                    Some(b'{') => match self.parse_interval(2)? {
                        Some(bounds) => bounds,
                        None => return Ok(atom),
                    },
                    _ => return Ok(atom),
                },
                _ => return Ok(atom),
            };
            atom = Node::Repeat(Box::new(atom), min, max);
        }
    }

    /// Parses `{m}`, `{m,}`, `{,n}` or `{m,n}`. `skip` is the opener's length.
    fn parse_interval(&mut self, skip: usize) -> ParseResult<Option<(u32, Option<u32>)>> {
        let mut index = self.index + skip;
        let number = |index: &mut usize| -> Option<u32> {
            let start = *index;
            let mut value = 0u32;
            while let Some(byte) = self.text.get(*index).filter(|byte| byte.is_ascii_digit()) {
                value = value.saturating_mul(10).saturating_add(u32::from(byte - b'0'));
                *index += 1;
            }
            (*index > start).then_some(value)
        };
        let min = number(&mut index);
        let (max, comma) = if self.text.get(index) == Some(&b',') {
            index += 1;
            (number(&mut index), true)
        } else {
            (min, false)
        };
        let closed = if self.extended() {
            self.text.get(index) == Some(&b'}')
        } else {
            self.text.get(index) == Some(&b'\\') && self.text.get(index + 1) == Some(&b'}')
        };
        if !closed || (min.is_none() && !comma) {
            if self.extended() {
                // A brace that is not an interval is an ordinary character.
                return Ok(None);
            }
            return Err("Unmatched \\{");
        }
        index += if self.extended() { 1 } else { 2 };
        let low = min.unwrap_or(0);
        if low > MAX_REPEAT || max.is_some_and(|high| high > MAX_REPEAT || high < low) {
            return Err("Invalid content of \\{\\}");
        }
        self.index = index;
        Ok(Some((low, if comma { max } else { min })))
    }

    fn parse_atom(&mut self, at_start: bool) -> ParseResult<Node> {
        let Some(byte) = self.peek() else {
            return Ok(Node::Empty);
        };
        match byte {
            b'.' => {
                self.index += 1;
                Ok(Node::Any)
            }
            b'[' => self.parse_class(),
            b'\\' => self.parse_escape(),
            b'^' if self.extended() || at_start => {
                self.index += 1;
                Ok(Node::Bol)
            }
            b'$' if self.extended() || self.dollar_is_anchor() => {
                self.index += 1;
                Ok(Node::Eol)
            }
            b'(' if self.extended() => {
                self.index += 1;
                self.parse_group(1)
            }
            b'*' if at_start => {
                self.index += 1;
                Ok(self.literal_byte(b'*'))
            }
            b'+' | b'?' if self.extended() && at_start => {
                self.index += 1;
                Ok(self.literal_byte(byte))
            }
            _ => self.parse_literal(),
        }
    }

    fn dollar_is_anchor(&self) -> bool {
        match self.peek_at(1) {
            None => true,
            Some(b'\\') => matches!(self.peek_at(2), Some(b')' | b'|')),
            _ => false,
        }
    }

    fn parse_group(&mut self, close: usize) -> ParseResult<Node> {
        if self.depth >= MAX_GROUP_DEPTH {
            return Err("Regular expression too deeply nested");
        }
        self.groups += 1;
        let number = self.groups;
        self.depth += 1;
        let inner = self.parse_alternation()?;
        if !self.at_group_close() {
            return Err(if self.extended() {
                "Unmatched ( or \\("
            } else {
                "Unmatched \\("
            });
        }
        self.depth -= 1;
        self.index += close;
        Ok(Node::Group(Box::new(inner), Some(number)))
    }

    fn parse_escape(&mut self) -> ParseResult<Node> {
        let Some(next) = self.peek_at(1) else {
            return Err("Trailing backslash");
        };
        self.index += 2;
        match next {
            b'(' if !self.extended() => self.parse_group(2),
            b')' if !self.extended() => Err("Unmatched ) or \\)"),
            b'{' if !self.extended() => Err("Invalid preceding regular expression"),
            b'1'..=b'9' => {
                let number = u32::from(next - b'0');
                if number > self.groups {
                    return Err("Invalid back reference");
                }
                self.has_backref = true;
                Ok(Node::Backref(number))
            }
            b'b' => Ok(Node::WordBoundary),
            b'B' => Ok(Node::NotWordBoundary),
            b'<' => Ok(Node::WordStart),
            b'>' => Ok(Node::WordEnd),
            b'w' | b'W' => {
                let mut set = ByteSet::new();
                set.insert_range(b'a', b'z');
                set.insert_range(b'A', b'Z');
                set.insert_range(b'0', b'9');
                set.insert(b'_');
                if next == b'W' {
                    set.negate();
                }
                Ok(Node::Class(Box::new(set)))
            }
            b's' | b'S' => {
                let mut set = ByteSet::new();
                for byte in [b' ', b'\t', b'\n', b'\r', 0x0b, 0x0c] {
                    set.insert(byte);
                }
                if next == b'S' {
                    set.negate();
                }
                Ok(Node::Class(Box::new(set)))
            }
            b'n' => Ok(Node::Byte(b'\n')),
            b't' => Ok(Node::Byte(b'\t')),
            other => Ok(self.literal_byte(other)),
        }
    }

    fn literal_byte(&self, byte: u8) -> Node {
        if self.ignore_case && byte.is_ascii_alphabetic() {
            let mut set = ByteSet::new();
            set.insert(byte.to_ascii_lowercase());
            set.insert(byte.to_ascii_uppercase());
            Node::Class(Box::new(set))
        } else {
            Node::Byte(byte)
        }
    }

    /// Parses one literal character, keeping a UTF-8 sequence together so a
    /// following `*` repeats the whole character.
    fn parse_literal(&mut self) -> ParseResult<Node> {
        let first = self.peek().unwrap_or(0);
        let length = utf8_len(first).min(self.text.len() - self.index);
        if length <= 1 {
            self.index += 1;
            return Ok(self.literal_byte(first));
        }
        let bytes = &self.text[self.index..self.index + length];
        let node = Node::Concat(bytes.iter().map(|byte| Node::Byte(*byte)).collect());
        self.index += length;
        Ok(Node::Group(Box::new(node), None))
    }

    fn parse_class(&mut self) -> ParseResult<Node> {
        let mut index = self.index + 1;
        let mut set = ByteSet::new();
        let mut negate = false;
        if self.text.get(index) == Some(&b'^') {
            negate = true;
            index += 1;
        }
        let mut extra: Vec<Node> = Vec::new();
        let mut first = true;
        loop {
            let Some(&byte) = self.text.get(index) else {
                return Err("Unmatched [, [^, [:, [., or [=");
            };
            if byte == b']' && !first {
                index += 1;
                break;
            }
            first = false;

            if byte == b'[' && self.text.get(index + 1) == Some(&b':') {
                let name_start = index + 2;
                let rest = self.text.get(name_start..).unwrap_or(&[]);
                let Some(length) = rest.windows(2).position(|window| window == b":]") else {
                    return Err("Unmatched [, [^, [:, [., or [=");
                };
                let name = core::str::from_utf8(&rest[..length]).unwrap_or("");
                if posix_class(name, 'a').is_none() {
                    return Err("Invalid character class name");
                }
                for candidate in 0u8..128 {
                    if posix_class(name, candidate as char) == Some(true) {
                        set.insert(candidate);
                    }
                }
                index = name_start + length + 2;
                continue;
            }

            if byte >= 0xc0 {
                // A non-ASCII member cannot live in a byte set.
                let length = utf8_len(byte).min(self.text.len() - index);
                let bytes = &self.text[index..index + length];
                extra.push(Node::Concat(bytes.iter().map(|b| Node::Byte(*b)).collect()));
                index += length;
                continue;
            }

            let high = self.text.get(index + 2).copied();
            if self.text.get(index + 1) == Some(&b'-') && high.is_some_and(|high| high != b']') {
                let high = high.unwrap_or(byte);
                if high < byte {
                    return Err("Invalid range end");
                }
                set.insert_range(byte, high);
                index += 3;
            } else {
                set.insert(byte);
                index += 1;
            }
        }
        self.index = index;

        if self.ignore_case {
            set.fold_case();
        }
        if negate {
            // Non-ASCII members of a negated class are not representable as
            // bytes, so they simply stop being excluded.
            set.negate();
            return Ok(Node::Class(Box::new(set)));
        }
        if extra.is_empty() {
            return Ok(Node::Class(Box::new(set)));
        }
        extra.push(Node::Class(Box::new(set)));
        Ok(Node::Group(Box::new(Node::Alt(extra)), None))
    }
}

fn utf8_len(byte: u8) -> usize {
    match byte {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

#[derive(Clone, Copy, Debug)]
enum Inst {
    Byte(u8),
    Any,
    Set(u32),
    Split(u32, u32),
    Jump(u32),
    Save(u32),
    Bol,
    Eol,
    WordBoundary,
    NotWordBoundary,
    WordStart,
    WordEnd,
    PrevNotWord,
    NextNotWord,
    Backref(u32),
    Match,
}

struct Compiler {
    program: Vec<Inst>,
    sets: Vec<ByteSet>,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> ParseResult<usize> {
        if self.program.len() >= MAX_PROGRAM {
            return Err("Regular expression too big");
        }
        self.program.push(inst);
        Ok(self.program.len() - 1)
    }

    fn emit(&mut self, node: &Node, depth: usize) -> ParseResult<()> {
        if depth > MAX_GROUP_DEPTH * 3 {
            return Err("Regular expression too deeply nested");
        }
        match node {
            Node::Empty => {}
            Node::Byte(byte) => {
                self.push(Inst::Byte(*byte))?;
            }
            Node::Any => {
                self.push(Inst::Any)?;
            }
            Node::Class(set) => {
                let index = match self.sets.iter().position(|known| known == set.as_ref()) {
                    Some(index) => index,
                    None => {
                        self.sets.push(**set);
                        self.sets.len() - 1
                    }
                };
                self.push(Inst::Set(index as u32))?;
            }
            Node::Group(inner, number) => {
                if let Some(number) = number {
                    self.push(Inst::Save(number * 2))?;
                }
                self.emit(inner, depth + 1)?;
                if let Some(number) = number {
                    self.push(Inst::Save(number * 2 + 1))?;
                }
            }
            Node::Concat(items) => {
                for item in items {
                    self.emit(item, depth + 1)?;
                }
            }
            Node::Alt(branches) => {
                let mut exits = Vec::new();
                for (index, branch) in branches.iter().enumerate() {
                    if index + 1 == branches.len() {
                        self.emit(branch, depth + 1)?;
                    } else {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.emit(branch, depth + 1)?;
                        exits.push(self.push(Inst::Jump(0))?);
                        let next = self.program.len() as u32;
                        self.program[split] = Inst::Split(split as u32 + 1, next);
                    }
                }
                let end = self.program.len() as u32;
                for exit in exits {
                    self.program[exit] = Inst::Jump(end);
                }
            }
            Node::Repeat(inner, min, max) => {
                for _ in 0..*min {
                    self.emit(inner, depth + 1)?;
                }
                match max {
                    None => {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.emit(inner, depth + 1)?;
                        self.push(Inst::Jump(split as u32))?;
                        let next = self.program.len() as u32;
                        self.program[split] = Inst::Split(split as u32 + 1, next);
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.push(Inst::Split(0, 0))?);
                            self.emit(inner, depth + 1)?;
                        }
                        let end = self.program.len() as u32;
                        for split in splits {
                            self.program[split] = Inst::Split(split as u32 + 1, end);
                        }
                    }
                }
            }
            Node::Bol => {
                self.push(Inst::Bol)?;
            }
            Node::Eol => {
                self.push(Inst::Eol)?;
            }
            Node::WordBoundary => {
                self.push(Inst::WordBoundary)?;
            }
            Node::NotWordBoundary => {
                self.push(Inst::NotWordBoundary)?;
            }
            Node::WordStart => {
                self.push(Inst::WordStart)?;
            }
            Node::WordEnd => {
                self.push(Inst::WordEnd)?;
            }
            Node::Backref(number) => {
                self.push(Inst::Backref(*number))?;
            }
        }
        Ok(())
    }
}

/// A compiled regular expression.
#[derive(Clone, Debug)]
pub struct Regex {
    program: Vec<Inst>,
    sets: Vec<ByteSet>,
    groups: u32,
    has_backref: bool,
    ignore_case: bool,
    anchored: bool,
    first_byte: Option<u8>,
}

/// Byte offsets of a match and its groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captures {
    slots: Vec<Option<usize>>,
}

impl Captures {
    /// The offsets of the whole match.
    pub fn span(&self) -> (usize, usize) {
        (
            self.slots.first().copied().flatten().unwrap_or(0),
            self.slots.get(1).copied().flatten().unwrap_or(0),
        )
    }

    /// The offsets of group `index`, when it took part in the match.
    pub fn group(&self, index: usize) -> Option<(usize, usize)> {
        let start = self.slots.get(index * 2).copied().flatten()?;
        let end = self.slots.get(index * 2 + 1).copied().flatten()?;
        Some((start, end))
    }
}

enum Frame {
    Branch(u32, usize),
    Restore(u32, Option<usize>),
}

fn is_word(byte: Option<&u8>) -> bool {
    byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

impl Regex {
    /// Compiles `pattern` in `syntax`.
    pub fn new(pattern: &str, syntax: Syntax) -> Result<Self, &'static str> {
        Self::with_options(pattern, Options::new(syntax))
    }

    /// Compiles `pattern` with explicit options.
    pub fn with_options(pattern: &str, options: Options) -> Result<Self, &'static str> {
        let mut parser = Parser {
            text: pattern.as_bytes(),
            index: 0,
            syntax: options.syntax,
            ignore_case: options.ignore_case,
            groups: 0,
            depth: 0,
            has_backref: false,
        };

        let node = if options.syntax == Syntax::Fixed {
            Node::Concat(
                pattern
                    .bytes()
                    .map(|byte| parser.literal_byte(byte))
                    .collect(),
            )
        } else {
            let node = parser.parse_alternation()?;
            if parser.index < parser.text.len() {
                return Err("Unmatched ) or \\)");
            }
            node
        };

        let mut compiler = Compiler {
            program: Vec::new(),
            sets: Vec::new(),
        };
        compiler.push(Inst::Save(0))?;
        if options.whole_line {
            compiler.push(Inst::Bol)?;
        }
        if options.whole_word {
            compiler.push(Inst::PrevNotWord)?;
        }
        compiler.emit(&node, 0)?;
        if options.whole_word {
            compiler.push(Inst::NextNotWord)?;
        }
        if options.whole_line {
            compiler.push(Inst::Eol)?;
        }
        compiler.push(Inst::Save(1))?;
        compiler.push(Inst::Match)?;

        let anchored = matches!(compiler.program.get(1), Some(Inst::Bol));
        let first_byte = match compiler.program.get(1) {
            Some(Inst::Byte(byte)) => Some(*byte),
            _ => None,
        };
        Ok(Self {
            program: compiler.program,
            sets: compiler.sets,
            groups: parser.groups,
            has_backref: parser.has_backref,
            ignore_case: options.ignore_case,
            anchored,
            first_byte,
        })
    }

    /// The number of capture groups.
    pub fn groups(&self) -> usize {
        self.groups as usize
    }

    /// Returns `true` when the pattern matches anywhere in `text`.
    pub fn is_match(&self, text: &[u8]) -> bool {
        self.search(text, 0, true).is_some()
    }

    /// Finds the leftmost-longest match starting at or after `from`.
    pub fn find_at(&self, text: &[u8], from: usize) -> Option<Captures> {
        self.search(text, from, false)
    }

    fn search(&self, text: &[u8], from: usize, any: bool) -> Option<Captures> {
        if from > text.len() {
            return None;
        }

        // The memo stays valid across start offsets: a state that failed once
        // fails from every start.
        let bits = self.program.len().saturating_mul(text.len() + 1);
        let use_memo = !self.has_backref && bits / 8 <= MAX_MEMO_BYTES;
        let mut memo = Vec::new();
        if use_memo && memo.try_reserve_exact(bits / 32 + 1).is_ok() {
            memo.resize(bits / 32 + 1, 0u32);
        }
        let mut stack = Vec::new();
        let mut caps = alloc::vec![None; (self.groups as usize + 1) * 2];
        let mut steps = 0usize;

        for start in from..=text.len() {
            if self.anchored && start > 0 {
                return None;
            }
            if let Some(first) = self.first_byte {
                if text.get(start) != Some(&first) {
                    continue;
                }
            }
            caps.iter_mut().for_each(|slot| *slot = None);
            if let Some(found) = self.run(text, start, any, &mut memo, &mut stack, &mut caps, &mut steps)
            {
                return Some(Captures { slots: found });
            }
            if steps > MAX_STEPS {
                return None;
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        text: &[u8],
        start: usize,
        any: bool,
        memo: &mut [u32],
        stack: &mut Vec<Frame>,
        caps: &mut [Option<usize>],
        steps: &mut usize,
    ) -> Option<Vec<Option<usize>>> {
        let mut best: Option<Vec<Option<usize>>> = None;
        let mut best_end = 0usize;
        stack.clear();
        stack.push(Frame::Branch(0, start));
        let width = text.len() + 1;

        while let Some(frame) = stack.pop() {
            let (mut pc, mut sp) = match frame {
                Frame::Branch(pc, sp) => (pc as usize, sp),
                Frame::Restore(slot, value) => {
                    if let Some(entry) = caps.get_mut(slot as usize) {
                        *entry = value;
                    }
                    continue;
                }
            };

            loop {
                *steps += 1;
                if *steps > MAX_STEPS || stack.len() > MAX_STACK {
                    return best;
                }
                let Some(inst) = self.program.get(pc) else {
                    break;
                };
                match *inst {
                    Inst::Byte(byte) => {
                        if text.get(sp) != Some(&byte) {
                            break;
                        }
                        sp += 1;
                        pc += 1;
                    }
                    Inst::Any => {
                        let Some(&byte) = text.get(sp) else {
                            break;
                        };
                        sp = (sp + utf8_len(byte)).min(text.len());
                        pc += 1;
                    }
                    Inst::Set(index) => {
                        let Some(&byte) = text.get(sp) else {
                            break;
                        };
                        let Some(set) = self.sets.get(index as usize) else {
                            break;
                        };
                        if !set.contains(byte) {
                            break;
                        }
                        sp = if set.whole_char {
                            (sp + utf8_len(byte)).min(text.len())
                        } else {
                            sp + 1
                        };
                        pc += 1;
                    }
                    Inst::Split(first, second) => {
                        if !memo.is_empty() {
                            let bit = pc * width + sp;
                            let Some(word) = memo.get_mut(bit / 32) else {
                                break;
                            };
                            let mask = 1u32 << (bit % 32);
                            if *word & mask != 0 {
                                break;
                            }
                            *word |= mask;
                        }
                        stack.push(Frame::Branch(second, sp));
                        pc = first as usize;
                    }
                    Inst::Jump(target) => pc = target as usize,
                    Inst::Save(slot) => {
                        if let Some(entry) = caps.get_mut(slot as usize) {
                            stack.push(Frame::Restore(slot, *entry));
                            *entry = Some(sp);
                        }
                        pc += 1;
                    }
                    Inst::Bol => {
                        if sp != 0 {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Eol => {
                        if sp != text.len() {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::WordBoundary | Inst::NotWordBoundary => {
                        let before = sp > 0 && is_word(text.get(sp - 1));
                        let after = is_word(text.get(sp));
                        let boundary = before != after;
                        if boundary != matches!(*inst, Inst::WordBoundary) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::WordStart => {
                        if (sp > 0 && is_word(text.get(sp - 1))) || !is_word(text.get(sp)) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::WordEnd => {
                        if !(sp > 0 && is_word(text.get(sp - 1))) || is_word(text.get(sp)) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::PrevNotWord => {
                        if sp > 0 && is_word(text.get(sp - 1)) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::NextNotWord => {
                        if is_word(text.get(sp)) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Backref(number) => {
                        let start_slot = caps.get(number as usize * 2).copied().flatten();
                        let end_slot = caps.get(number as usize * 2 + 1).copied().flatten();
                        let (Some(from), Some(to)) = (start_slot, end_slot) else {
                            break;
                        };
                        let Some(group) = text.get(from..to) else {
                            break;
                        };
                        let Some(here) = text.get(sp..sp + group.len()) else {
                            break;
                        };
                        let same = if self.ignore_case {
                            group.eq_ignore_ascii_case(here)
                        } else {
                            group == here
                        };
                        if !same {
                            break;
                        }
                        sp += group.len();
                        pc += 1;
                    }
                    Inst::Match => {
                        if best.is_none() || sp > best_end {
                            best_end = sp;
                            best = Some(caps.to_vec());
                        }
                        if any || sp == text.len() {
                            return best;
                        }
                        break;
                    }
                }
            }
        }
        best
    }
}

/// Returns `true` if the extended-syntax `pattern` matches anywhere in `text`.
/// A pattern that does not compile matches nothing.
pub fn is_match(pattern: &str, text: &str) -> bool {
    Regex::new(pattern, Syntax::Extended)
        .map(|regex| regex.is_match(text.as_bytes()))
        .unwrap_or(false)
}

/// Escapes `text` so it matches literally in extended syntax.
pub fn escape(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
        ) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(pattern: &str, text: &str) -> bool {
        Regex::new(pattern, Syntax::Basic).unwrap().is_match(text.as_bytes())
    }

    fn span(pattern: &str, text: &str) -> Option<(usize, usize)> {
        Regex::new(pattern, Syntax::Extended)
            .unwrap()
            .find_at(text.as_bytes(), 0)
            .map(|caps| caps.span())
    }

    #[test]
    fn matches_literals_and_dot() {
        assert!(is_match("abc", "xxabcxx"));
        assert!(!is_match("abc", "ab"));
        assert!(is_match("a.c", "abc"));
        assert!(is_match("a.c", "a-c"));
    }

    #[test]
    fn matches_repetitions() {
        assert!(is_match("ab*c", "ac"));
        assert!(is_match("ab*c", "abbbbc"));
        assert!(is_match("ab+c", "abc"));
        assert!(!is_match("ab+c", "ac"));
        assert!(is_match("ab?c", "ac"));
        assert!(is_match("ab?c", "abc"));
        assert!(!is_match("ab?c", "abbc"));
    }

    #[test]
    fn matches_classes_and_anchors() {
        assert!(is_match("[abc]at", "cat"));
        assert!(!is_match("[^abc]at", "cat"));
        assert!(is_match("[!abc]at", "!at"));
        assert!(is_match("[a-z]at", "cat"));
        assert!(is_match("^cat$", "cat"));
        assert!(!is_match("^cat$", "scat"));
        assert!(!is_match("^cat$", "cats"));
        assert!(is_match("^$", ""));
    }

    #[test]
    fn escapes_metacharacters() {
        assert!(is_match(r"a\.c", "a.c"));
        assert!(!is_match(r"a\.c", "abc"));
        assert!(is_match(r"\*", "x*y"));
    }

    #[test]
    fn matches_alternation_and_groups() {
        assert!(is_match("a|b", "xb"));
        assert!(!is_match("a|b", "xc"));
        assert!(is_match("(foo|bar)baz", "barbaz"));
        assert!(is_match("^(ab)+$", "ababab"));
        assert!(!is_match("^(ab)+$", "aba"));
    }

    #[test]
    fn matches_intervals() {
        assert!(is_match("^a{2,3}$", "aaa"));
        assert!(!is_match("^a{2,3}$", "aaaa"));
        assert!(is_match("^a{2}$", "aa"));
        assert!(is_match("^a{2,}$", "aaaaa"));
        assert!(is_match("a{x", "a{x"));
    }

    #[test]
    fn matches_posix_classes() {
        assert!(is_match("^[[:digit:]]+$", "123"));
        assert!(!is_match("^[[:digit:]]+$", "12a"));
        assert!(is_match("[[:upper:]][[:lower:]]", "xAb"));
        assert!(is_match("^[^[:space:]]+$", "abc"));
        assert_eq!(
            Regex::new("[[:nope:]]", Syntax::Extended).unwrap_err(),
            "Invalid character class name"
        );
    }

    #[test]
    fn basic_syntax_has_escaped_operators() {
        assert!(basic(r"a\|b", "b"));
        assert!(basic("a+", "a+"));
        assert!(!basic("a+", "aa"));
        assert!(basic(r"a\+", "aa"));
        assert!(basic(r"\(ab\)\{2\}", "abab"));
        assert!(basic("*a", "*a"));
        assert!(basic("a^b$c", "a^b$c"));
        assert!(basic("^a$", "a"));
        assert!(basic(r"\(a\)\1", "aa"));
        assert!(!basic(r"\(a\)\1", "ab"));
    }

    #[test]
    fn matches_are_leftmost_longest() {
        assert_eq!(span("a|ab|abc", "xabcd"), Some((1, 4)));
        assert_eq!(span("a*", "baa"), Some((0, 0)));
        assert_eq!(span("(a|ab)(c|bcd)", "abcd"), Some((0, 4)));
    }

    #[test]
    fn captures_report_group_offsets() {
        let regex = Regex::new("(a+)(b)?", Syntax::Extended).unwrap();
        let caps = regex.find_at(b"xaac", 0).unwrap();
        assert_eq!(caps.span(), (1, 3));
        assert_eq!(caps.group(1), Some((1, 3)));
        assert_eq!(caps.group(2), None);
    }

    #[test]
    fn options_constrain_matches() {
        let word = Options {
            whole_word: true,
            ..Options::new(Syntax::Basic)
        };
        let regex = Regex::with_options("x", word).unwrap();
        assert!(regex.is_match(b"a x b"));
        assert!(!regex.is_match(b"axb"));
        assert!(regex.is_match("\u{e9} x".as_bytes()));

        let line = Options {
            whole_line: true,
            ..Options::new(Syntax::Extended)
        };
        let regex = Regex::with_options("a|ab", line).unwrap();
        assert!(regex.is_match(b"ab"));
        assert!(!regex.is_match(b"abc"));

        let fold = Options {
            ignore_case: true,
            ..Options::new(Syntax::Extended)
        };
        let regex = Regex::with_options("h[a-c]L+o", fold).unwrap();
        assert!(regex.is_match(b"HBllO"));

        let fixed = Regex::new("a.*", Syntax::Fixed).unwrap();
        assert!(fixed.is_match(b"xa.*y"));
        assert!(!fixed.is_match(b"abc"));
    }

    #[test]
    fn dot_and_negated_classes_consume_whole_characters() {
        let regex = Regex::new("^.$", Syntax::Extended).unwrap();
        assert!(regex.is_match("\u{e9}".as_bytes()));
        let regex = Regex::new("^[^a]$", Syntax::Extended).unwrap();
        assert!(regex.is_match("\u{e9}".as_bytes()));
        let regex = Regex::new("^[\u{e9}x]+$", Syntax::Extended).unwrap();
        assert!(regex.is_match("\u{e9}x\u{e9}".as_bytes()));
    }

    #[test]
    fn malformed_patterns_report_errors() {
        assert!(Regex::new("(a", Syntax::Extended).is_err());
        assert!(Regex::new("[a", Syntax::Extended).is_err());
        assert!(Regex::new(r"\(a", Syntax::Basic).is_err());
        assert!(Regex::new(r"a\", Syntax::Basic).is_err());
        assert!(Regex::new("a{3,2}", Syntax::Extended).is_err());
        assert!(Regex::new(r"\2", Syntax::Basic).is_err());
    }

    #[test]
    fn hostile_patterns_are_bounded() {
        // Deep groups are an error, not a stack overflow.
        let deep = "(".repeat(5000) + &")".repeat(5000);
        assert!(Regex::new(&deep, Syntax::Extended).is_err());
        // Nested counted repeats are refused by the program limit.
        assert!(Regex::new("(a{1000}){1000}", Syntax::Extended).is_err());
        // Exponential backtracking becomes linear with the memo.
        let regex = Regex::new("^(a*)*b$", Syntax::Extended).unwrap();
        assert!(!regex.is_match("a".repeat(2000).as_bytes()));
        let regex = Regex::new("^(a|a)*$", Syntax::Extended).unwrap();
        assert!(regex.is_match("a".repeat(2000).as_bytes()));
        // Back-references run under the step limit.
        let regex = Regex::new(r"^(a*)*\1b$", Syntax::Extended).unwrap();
        assert!(!regex.is_match("a".repeat(64).as_bytes()));
    }

    #[test]
    fn empty_repeats_terminate() {
        assert!(is_match("(a*)*", "b"));
        assert!(is_match("(|a)+b", "ab"));
        assert!(is_match("()*x", "x"));
    }
}
