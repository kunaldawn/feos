//! `sed`: a stream editor.
//!
//! Scripts are parsed once into a flat command list, and `{ ... }` groups
//! become jumps, so a deeply nested script needs no recursion at run time.
//! Supported commands: `s y d D p P q Q n N g G h H x a i c = l r w b t T : {}`,
//! with addresses `N`, `$`, `/re/`, `first~step`, ranges and `!`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::opts::{bad_option, Opt, Opts};
use super::{error, path_error, read_path, split_lines, usage, write_line, Ctx};
use crate::regex::{Options as RegexOptions, Regex, Syntax};

/// Cycles without reading a line before a script counts as a loop.
const MAX_BRANCHES: usize = 1_000_000;

/// Largest pattern or hold space, which stops `G` in a loop eating the heap.
const MAX_SPACE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
enum Address {
    Line(usize),
    Last,
    Step(usize, usize),
    Regex(Regex),
}

#[derive(Clone)]
struct Replacement {
    regex: Regex,
    parts: Vec<ReplacePart>,
    global: bool,
    print: bool,
    occurrence: usize,
    write_to: Option<String>,
}

#[derive(Clone)]
enum ReplacePart {
    Literal(Vec<u8>),
    Group(usize),
}

#[derive(Clone)]
enum Action {
    Substitute(alloc::boxed::Box<Replacement>),
    Transliterate(alloc::boxed::Box<[u8; 256]>),
    Delete,
    DeleteFirstLine,
    Print,
    PrintFirstLine,
    Quit(i32, bool),
    Next,
    NextAppend,
    GetHold,
    GetHoldAppend,
    Hold,
    HoldAppend,
    Exchange,
    Append(Vec<u8>),
    Insert(Vec<u8>),
    Change(Vec<u8>),
    LineNumber,
    List,
    ReadFile(String),
    WriteFile(String),
    /// Jumps to a command index; `usize::MAX` means the end of the script.
    Branch(usize),
    BranchIfSubstituted(usize),
    BranchIfNotSubstituted(usize),
    /// `{`: skipped by jumping to the matching `}` when the address fails.
    Block(usize),
    Nop,
}

#[derive(Clone)]
struct Command {
    first: Option<Address>,
    second: Option<Address>,
    negate: bool,
    action: Action,
    in_range: bool,
    range_end: usize,
}

struct ScriptParser<'a> {
    text: &'a [u8],
    index: usize,
    syntax: Syntax,
    commands: Vec<Command>,
    blocks: Vec<usize>,
    labels: Vec<(String, usize)>,
    pending: Vec<(usize, String)>,
}

impl ScriptParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.index).copied()
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.index += 1;
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b';')) {
            self.index += 1;
        }
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.index;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.index += 1;
        }
        core::str::from_utf8(&self.text[start..self.index]).ok()?.parse().ok()
    }

    /// Reads up to an unescaped `delimiter`. Escaped delimiters lose their
    /// backslash; other escapes pass through for the regex or replacement.
    fn delimited(&mut self, delimiter: u8) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut in_class = false;
        loop {
            let Some(byte) = self.peek() else {
                return Err(String::from("unterminated address regex or command"));
            };
            self.index += 1;
            if byte == b'\\' {
                let Some(next) = self.peek() else {
                    return Err(String::from("unterminated command"));
                };
                self.index += 1;
                if next == delimiter {
                    out.push(next);
                } else if next == b'n' && delimiter != b'n' {
                    out.push(b'\n');
                } else {
                    out.push(b'\\');
                    out.push(next);
                }
                continue;
            }
            if byte == b'[' {
                in_class = true;
            } else if byte == b']' {
                in_class = false;
            } else if byte == delimiter && !in_class {
                return Ok(out);
            }
            out.push(byte);
        }
    }

    fn regex(&self, pattern: &[u8], ignore_case: bool) -> Result<Regex, String> {
        let text = String::from_utf8_lossy(pattern);
        let options = RegexOptions {
            ignore_case,
            ..RegexOptions::new(self.syntax)
        };
        Regex::with_options(&text, options).map_err(String::from)
    }

    fn address(&mut self) -> Result<Option<Address>, String> {
        match self.peek() {
            Some(b'0'..=b'9') => {
                let first = self.number().ok_or("invalid line number")?;
                if self.peek() == Some(b'~') {
                    self.index += 1;
                    let step = self.number().ok_or("invalid step")?;
                    return Ok(Some(Address::Step(first, step)));
                }
                Ok(Some(Address::Line(first)))
            }
            Some(b'$') => {
                self.index += 1;
                Ok(Some(Address::Last))
            }
            Some(open @ (b'/' | b'\\')) => {
                self.index += 1;
                let delimiter = if open == b'\\' {
                    let delimiter = self.peek().ok_or("unterminated address regex")?;
                    self.index += 1;
                    delimiter
                } else {
                    open
                };
                let pattern = self.delimited(delimiter)?;
                let ignore_case = self.peek() == Some(b'I');
                if ignore_case {
                    self.index += 1;
                }
                Ok(Some(Address::Regex(self.regex(&pattern, ignore_case)?)))
            }
            _ => Ok(None),
        }
    }

    /// Reads a label or file name up to the end of the line. Labels also stop
    /// at `;` and `}`, as GNU sed allows.
    fn word(&mut self, label: bool) -> String {
        self.skip_blanks();
        let start = self.index;
        while let Some(byte) = self.peek() {
            if byte == b'\n' || (label && matches!(byte, b';' | b'}' | b' ')) {
                break;
            }
            self.index += 1;
        }
        String::from_utf8_lossy(&self.text[start..self.index]).into_owned()
    }

    /// Reads the text argument of `a`, `i` or `c`.
    fn text_argument(&mut self) -> Vec<u8> {
        self.skip_blanks();
        if self.peek() == Some(b'\\') {
            self.index += 1;
            self.skip_blanks();
            if self.peek() == Some(b'\n') {
                self.index += 1;
            }
        }
        let mut out = Vec::new();
        while let Some(byte) = self.peek() {
            self.index += 1;
            if byte == b'\\' {
                if let Some(next) = self.peek() {
                    self.index += 1;
                    out.push(next);
                }
            } else if byte == b'\n' {
                break;
            } else {
                out.push(byte);
            }
        }
        out.push(b'\n');
        out
    }

    fn replacement(&self, raw: &[u8]) -> Vec<ReplacePart> {
        let mut parts = Vec::new();
        let mut literal = Vec::new();
        let mut index = 0usize;
        while index < raw.len() {
            let byte = raw[index];
            index += 1;
            let group = match byte {
                b'&' => Some(0),
                b'\\' => match raw.get(index) {
                    Some(digit @ b'0'..=b'9') => {
                        index += 1;
                        Some(usize::from(digit - b'0'))
                    }
                    Some(b'n') => {
                        index += 1;
                        literal.push(b'\n');
                        None
                    }
                    Some(b't') => {
                        index += 1;
                        literal.push(b'\t');
                        None
                    }
                    Some(other) => {
                        index += 1;
                        literal.push(*other);
                        None
                    }
                    None => {
                        literal.push(b'\\');
                        None
                    }
                },
                other => {
                    literal.push(other);
                    None
                }
            };
            if let Some(group) = group {
                if !literal.is_empty() {
                    parts.push(ReplacePart::Literal(core::mem::take(&mut literal)));
                }
                parts.push(ReplacePart::Group(group));
            }
        }
        if !literal.is_empty() {
            parts.push(ReplacePart::Literal(literal));
        }
        parts
    }

    fn substitute(&mut self) -> Result<Action, String> {
        let delimiter = self.peek().ok_or("unterminated `s' command")?;
        if matches!(delimiter, b'\n' | b'\\') {
            return Err(String::from("invalid delimiter for `s'"));
        }
        self.index += 1;
        let pattern = self.delimited(delimiter)?;
        let raw = {
            // The replacement keeps `\n` and `\&` for later, so it is read
            // here without the regex rules for classes.
            let mut out = Vec::new();
            loop {
                let Some(byte) = self.peek() else {
                    return Err(String::from("unterminated `s' command"));
                };
                self.index += 1;
                if byte == b'\\' {
                    let Some(next) = self.peek() else {
                        return Err(String::from("unterminated `s' command"));
                    };
                    self.index += 1;
                    if next == delimiter {
                        out.push(next);
                    } else {
                        out.push(b'\\');
                        out.push(next);
                    }
                    continue;
                }
                if byte == delimiter {
                    break;
                }
                out.push(byte);
            }
            out
        };

        let mut global = false;
        let mut print = false;
        let mut ignore_case = false;
        let mut occurrence = 1usize;
        let mut write_to = None;
        loop {
            match self.peek() {
                Some(b'g') => global = true,
                Some(b'p') => print = true,
                Some(b'i' | b'I') => ignore_case = true,
                Some(b'0'..=b'9') => {
                    occurrence = self.number().ok_or("invalid occurrence")?;
                    if occurrence == 0 {
                        return Err(String::from("number option to `s' may not be zero"));
                    }
                    continue;
                }
                Some(b'w') => {
                    self.index += 1;
                    write_to = Some(self.word(false));
                    break;
                }
                _ => break,
            }
            self.index += 1;
        }

        let regex = self.regex(&pattern, ignore_case)?;
        let parts = self.replacement(&raw);
        for part in &parts {
            if let ReplacePart::Group(group) = part {
                if *group > regex.groups() {
                    return Err(String::from("invalid reference on `s' command's RHS"));
                }
            }
        }
        Ok(Action::Substitute(alloc::boxed::Box::new(Replacement {
            regex,
            parts,
            global,
            print,
            occurrence,
            write_to,
        })))
    }

    fn transliterate(&mut self) -> Result<Action, String> {
        let delimiter = self.peek().ok_or("unterminated `y' command")?;
        self.index += 1;
        let unescape = |raw: Vec<u8>| -> Vec<u8> {
            let mut out = Vec::new();
            let mut bytes = raw.into_iter();
            while let Some(byte) = bytes.next() {
                if byte == b'\\' {
                    match bytes.next() {
                        Some(b'n') => out.push(b'\n'),
                        Some(other) => out.push(other),
                        None => out.push(b'\\'),
                    }
                } else {
                    out.push(byte);
                }
            }
            out
        };
        let from = unescape(self.delimited(delimiter)?);
        let to = unescape(self.delimited(delimiter)?);
        if from.len() != to.len() {
            return Err(String::from("strings for `y' command are different lengths"));
        }
        let mut table = alloc::boxed::Box::new([0u8; 256]);
        for (index, slot) in table.iter_mut().enumerate() {
            *slot = index as u8;
        }
        for (a, b) in from.iter().zip(&to) {
            table[usize::from(*a)] = *b;
        }
        Ok(Action::Transliterate(table))
    }

    fn parse(&mut self) -> Result<(), String> {
        loop {
            self.skip_separators();
            let Some(byte) = self.peek() else {
                break;
            };
            if byte == b'#' {
                while self.peek().is_some_and(|byte| byte != b'\n') {
                    self.index += 1;
                }
                continue;
            }

            let first = self.address()?;
            let mut second = None;
            if first.is_some() {
                self.skip_blanks();
                if self.peek() == Some(b',') {
                    self.index += 1;
                    self.skip_blanks();
                    second = Some(self.address()?.ok_or("unexpected `,'")?);
                }
            }
            self.skip_blanks();
            let mut negate = false;
            while self.peek() == Some(b'!') {
                negate = true;
                self.index += 1;
                self.skip_blanks();
            }

            let Some(name) = self.peek() else {
                return Err(String::from("missing command"));
            };
            self.index += 1;
            let at = self.commands.len();
            let action = match name {
                b'{' => {
                    self.blocks.push(at);
                    Action::Block(usize::MAX)
                }
                b'}' => {
                    let Some(open) = self.blocks.pop() else {
                        return Err(String::from("unexpected `}'"));
                    };
                    if let Some(command) = self.commands.get_mut(open) {
                        command.action = Action::Block(at);
                    }
                    Action::Nop
                }
                b's' => self.substitute()?,
                b'y' => self.transliterate()?,
                b'd' => Action::Delete,
                b'D' => Action::DeleteFirstLine,
                b'p' => Action::Print,
                b'P' => Action::PrintFirstLine,
                b'q' | b'Q' => {
                    self.skip_blanks();
                    let status = self.number().unwrap_or(0) as i32;
                    Action::Quit(status, name == b'q')
                }
                b'n' => Action::Next,
                b'N' => Action::NextAppend,
                b'g' => Action::GetHold,
                b'G' => Action::GetHoldAppend,
                b'h' => Action::Hold,
                b'H' => Action::HoldAppend,
                b'x' => Action::Exchange,
                b'a' => Action::Append(self.text_argument()),
                b'i' => Action::Insert(self.text_argument()),
                b'c' => Action::Change(self.text_argument()),
                b'=' => Action::LineNumber,
                b'l' => Action::List,
                b'r' => Action::ReadFile(self.word(false)),
                b'w' => Action::WriteFile(self.word(false)),
                b':' => {
                    let label = self.word(true);
                    if label.is_empty() {
                        return Err(String::from("\":\" lacks a label"));
                    }
                    self.labels.push((label, at));
                    Action::Nop
                }
                b'b' | b't' | b'T' => {
                    let label = self.word(true);
                    if !label.is_empty() {
                        self.pending.push((at, label));
                    }
                    match name {
                        b'b' => Action::Branch(usize::MAX),
                        b't' => Action::BranchIfSubstituted(usize::MAX),
                        _ => Action::BranchIfNotSubstituted(usize::MAX),
                    }
                }
                other => {
                    return Err(alloc::format!("unknown command: `{}'", other as char));
                }
            };
            self.commands.push(Command {
                first,
                second,
                negate,
                action,
                in_range: false,
                range_end: 0,
            });

            self.skip_blanks();
            match self.peek() {
                None | Some(b'\n' | b';' | b'}' | b'#') => {}
                Some(_) if name == b'}' || name == b'{' => {}
                Some(other) => {
                    return Err(alloc::format!(
                        "extra characters after command: `{}'",
                        other as char
                    ));
                }
            }
        }

        if !self.blocks.is_empty() {
            return Err(String::from("unmatched `{'"));
        }
        for (at, label) in core::mem::take(&mut self.pending) {
            let Some((_, target)) = self.labels.iter().find(|(name, _)| *name == label) else {
                return Err(alloc::format!("can't find label for jump to `{label}'"));
            };
            let target = *target;
            if let Some(command) = self.commands.get_mut(at) {
                match &mut command.action {
                    Action::Branch(slot)
                    | Action::BranchIfSubstituted(slot)
                    | Action::BranchIfNotSubstituted(slot) => *slot = target,
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CycleEnd {
    Normal,
    /// `d`: no auto-print.
    Deleted,
    /// `D`: restart without reading a new line.
    Restart,
    /// `q` or `Q` with a status and whether to auto-print.
    Quit(i32, bool),
}

struct Runner<'a> {
    commands: Vec<Command>,
    quiet: bool,
    lines: Vec<&'a [u8]>,
    next_line: usize,
    line_number: usize,
    output: Vec<u8>,
    writes: Vec<(String, Vec<u8>)>,
    hold: Vec<u8>,
    append_queue: Vec<Vec<u8>>,
    overflow: bool,
}

impl Runner<'_> {
    fn is_last(&self) -> bool {
        self.next_line >= self.lines.len()
    }

    fn address_matches(&self, address: &Address, space: &[u8]) -> bool {
        match address {
            Address::Line(line) => self.line_number == *line,
            Address::Last => self.is_last(),
            Address::Step(first, step) => {
                if *step == 0 {
                    self.line_number == *first
                } else {
                    self.line_number >= *first && (self.line_number - first).is_multiple_of(*step)
                }
            }
            Address::Regex(regex) => regex.is_match(space),
        }
    }

    fn selected(&mut self, index: usize, space: &[u8]) -> bool {
        let Some(command) = self.commands.get(index) else {
            return false;
        };
        let Some(first) = &command.first else {
            return !command.negate;
        };
        let result = match &command.second {
            None => self.address_matches(first, space),
            Some(second) => {
                let second = second.clone();
                let first = first.clone();
                let in_range = command.in_range;
                let range_end = command.range_end;
                if in_range {
                    let done = match &second {
                        Address::Line(line) => {
                            self.line_number >= *line || range_end > 0 && self.line_number >= range_end
                        }
                        other => self.address_matches(other, space),
                    };
                    if done {
                        if let Some(command) = self.commands.get_mut(index) {
                            command.in_range = false;
                        }
                    }
                    true
                } else if self.address_matches(&first, space) {
                    // A range whose end is already past matches one line.
                    let one_line = matches!(second, Address::Line(line) if line <= self.line_number);
                    if !one_line {
                        if let Some(command) = self.commands.get_mut(index) {
                            command.in_range = true;
                        }
                    }
                    true
                } else {
                    false
                }
            }
        };
        let negate = self.commands.get(index).is_some_and(|command| command.negate);
        result != negate
    }

    fn write_file(&mut self, name: &str, data: &[u8]) {
        if name == "/dev/stdout" {
            self.output.extend_from_slice(data);
            self.output.push(b'\n');
            return;
        }
        if let Some((_, buffer)) = self.writes.iter_mut().find(|(known, _)| known == name) {
            buffer.extend_from_slice(data);
            buffer.push(b'\n');
        }
    }

    fn flush_appends(&mut self) {
        for text in core::mem::take(&mut self.append_queue) {
            self.output.extend_from_slice(&text);
        }
    }

    fn take_line(&mut self) -> Option<&[u8]> {
        let line = *self.lines.get(self.next_line)?;
        self.next_line += 1;
        self.line_number += 1;
        Some(line.strip_suffix(b"\n").unwrap_or(line))
    }

    /// Runs the script over one pattern space.
    fn cycle(&mut self, space: &mut Vec<u8>, ctx: &mut Ctx<'_>) -> CycleEnd {
        let mut index = 0usize;
        let mut branches = 0usize;
        let mut substituted = false;

        while index < self.commands.len() {
            let selected = self.selected(index, space);
            let action = self.commands[index].action.clone();
            if !selected {
                index = match action {
                    Action::Block(end) => end,
                    _ => index + 1,
                };
                continue;
            }
            index += 1;

            match action {
                Action::Nop | Action::Block(_) => {}
                Action::Substitute(replacement) => {
                    if let Some(result) = substitute(&replacement, space) {
                        *space = result;
                        substituted = true;
                        if replacement.print {
                            self.output.extend_from_slice(space);
                            self.output.push(b'\n');
                        }
                        if let Some(name) = &replacement.write_to {
                            let data = space.clone();
                            self.write_file(name, &data);
                        }
                    }
                }
                Action::Transliterate(table) => {
                    for byte in space.iter_mut() {
                        *byte = table[usize::from(*byte)];
                    }
                }
                Action::Delete => return CycleEnd::Deleted,
                Action::DeleteFirstLine => {
                    return match space.iter().position(|byte| *byte == b'\n') {
                        Some(at) => {
                            space.drain(..=at);
                            CycleEnd::Restart
                        }
                        None => CycleEnd::Deleted,
                    };
                }
                Action::Print => {
                    self.output.extend_from_slice(space);
                    self.output.push(b'\n');
                }
                Action::PrintFirstLine => {
                    let end = space.iter().position(|byte| *byte == b'\n').unwrap_or(space.len());
                    self.output.extend_from_slice(&space[..end]);
                    self.output.push(b'\n');
                }
                Action::Quit(status, print) => return CycleEnd::Quit(status, print),
                Action::Next => {
                    if self.is_last() {
                        return CycleEnd::Quit(0, true);
                    }
                    if !self.quiet {
                        self.output.extend_from_slice(space);
                        self.output.push(b'\n');
                    }
                    self.flush_appends();
                    let line = self.take_line().map(<[u8]>::to_vec).unwrap_or_default();
                    *space = line;
                }
                Action::NextAppend => {
                    if self.is_last() {
                        // GNU sed prints the pattern space before leaving.
                        return CycleEnd::Quit(0, true);
                    }
                    self.flush_appends();
                    let line = self.take_line().map(<[u8]>::to_vec).unwrap_or_default();
                    space.push(b'\n');
                    space.extend_from_slice(&line);
                }
                Action::GetHold => space.clone_from(&self.hold),
                Action::GetHoldAppend => {
                    space.push(b'\n');
                    space.extend_from_slice(&self.hold);
                }
                Action::Hold => self.hold.clone_from(space),
                Action::HoldAppend => {
                    self.hold.push(b'\n');
                    self.hold.extend_from_slice(space);
                }
                Action::Exchange => core::mem::swap(space, &mut self.hold),
                Action::Append(text) => self.append_queue.push(text),
                Action::Insert(text) => self.output.extend_from_slice(&text),
                Action::Change(text) => {
                    let command = &self.commands[index - 1];
                    if command.second.is_none() || !command.in_range || command.negate {
                        self.output.extend_from_slice(&text);
                    }
                    return CycleEnd::Deleted;
                }
                Action::LineNumber => {
                    let text = alloc::format!("{}\n", self.line_number);
                    self.output.extend_from_slice(text.as_bytes());
                }
                Action::List => {
                    let mut text = String::new();
                    for byte in space.iter() {
                        match byte {
                            b'\\' => text.push_str("\\\\"),
                            b'\t' => text.push_str("\\t"),
                            b'\n' => text.push_str("\\n"),
                            32..=126 => text.push(*byte as char),
                            other => text.push_str(&alloc::format!("\\{other:03o}")),
                        }
                    }
                    text.push_str("$\n");
                    self.output.extend_from_slice(text.as_bytes());
                }
                Action::ReadFile(path) => {
                    if let Ok(data) = ctx.fs.read_file(&path) {
                        self.append_queue.push(data);
                    }
                }
                Action::WriteFile(name) => {
                    let data = space.clone();
                    self.write_file(&name, &data);
                }
                Action::Branch(target) => {
                    branches += 1;
                    index = target;
                }
                Action::BranchIfSubstituted(target) => {
                    if substituted {
                        substituted = false;
                        branches += 1;
                        index = target;
                    }
                }
                Action::BranchIfNotSubstituted(target) => {
                    if substituted {
                        substituted = false;
                    } else {
                        branches += 1;
                        index = target;
                    }
                }
            }

            if space.len().max(self.hold.len()) > MAX_SPACE_BYTES
                || self.output.len() > crate::io::MAX_STREAM_BYTES
            {
                self.overflow = true;
                return CycleEnd::Quit(1, false);
            }
            if branches > MAX_BRANCHES || (branches % 1024 == 1023 && ctx.cancelled()) {
                self.overflow = true;
                return CycleEnd::Quit(1, false);
            }
        }
        CycleEnd::Normal
    }
}

fn substitute(replacement: &Replacement, text: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut position = 0usize;
    let mut copied = 0usize;
    let mut count = 0usize;
    let mut changed = false;
    while position <= text.len() {
        let Some(caps) = replacement.regex.find_at(text, position) else {
            break;
        };
        let (start, end) = caps.span();
        count += 1;
        if count >= replacement.occurrence {
            output.extend_from_slice(&text[copied..start]);
            for part in &replacement.parts {
                match part {
                    ReplacePart::Literal(bytes) => output.extend_from_slice(bytes),
                    ReplacePart::Group(group) => {
                        if let Some((from, to)) = caps.group(*group) {
                            output.extend_from_slice(&text[from..to]);
                        }
                    }
                }
            }
            copied = end;
            changed = true;
            if !replacement.global {
                break;
            }
        }
        if end > start {
            position = end;
        } else {
            // An empty match: step over one byte so the scan moves on.
            position = end + 1;
        }
    }
    if !changed {
        return None;
    }
    if let Some(rest) = text.get(copied..) {
        output.extend_from_slice(rest);
    }
    Some(output)
}

pub(super) fn sed(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut quiet = false;
    let mut in_place = false;
    let mut syntax = Syntax::Basic;
    let mut script = String::new();
    let mut have_script = false;
    let mut opts = Opts::new(args, "nEris:e:f:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('n') => quiet = true,
            Opt::Flag('E' | 'r') => syntax = Syntax::Extended,
            Opt::Flag('i') => in_place = true,
            Opt::Flag('s') => {}
            Opt::Value('e', value) => {
                script.push_str(value);
                script.push('\n');
                have_script = true;
            }
            Opt::Value('f', path) => match ctx.fs.read_file(path) {
                Ok(data) => {
                    script.push_str(&String::from_utf8_lossy(&data));
                    script.push('\n');
                    have_script = true;
                }
                Err(err) => {
                    path_error(ctx.stderr, "sed", path, err);
                    return 1;
                }
            },
            Opt::Long("in-place", _) => in_place = true,
            other => return bad_option(ctx, "sed", other),
        }
    }
    let mut files = core::mem::take(&mut opts.operands);
    if !have_script {
        if files.is_empty() {
            return usage(ctx.stderr, "sed", "sed [-nEi] [-e SCRIPT] [-f FILE] [SCRIPT] [file...]");
        }
        script = files.remove(0);
    }

    let mut parser = ScriptParser {
        text: script.as_bytes(),
        index: 0,
        syntax,
        commands: Vec::new(),
        blocks: Vec::new(),
        labels: Vec::new(),
        pending: Vec::new(),
    };
    if let Err(message) = parser.parse() {
        error(ctx.stderr, "sed", &alloc::format!("-e expression: {message}"));
        return 1;
    }
    let commands = parser.commands;
    if in_place && files.is_empty() {
        error(ctx.stderr, "sed", "no input files");
        return 1;
    }

    if in_place {
        let mut status = 0;
        for file in &files {
            let data = match read_path(ctx, file) {
                Ok(data) => data,
                Err(err) => {
                    path_error(ctx.stderr, "sed", file, err);
                    status = 1;
                    continue;
                }
            };
            let (output, code) = run_script(&commands, quiet, &data, ctx);
            if let Err(err) = ctx.fs.write_file(file, &output, false) {
                path_error(ctx.stderr, "sed", file, err);
                status = 1;
            } else if code != 0 {
                status = code;
            }
        }
        return status;
    }

    let mut data = Vec::new();
    if files.is_empty() {
        files.push(String::from("-"));
    }
    let mut status = 0;
    for file in &files {
        match read_path(ctx, file) {
            Ok(chunk) => {
                let needs_newline = !chunk.is_empty() && !chunk.ends_with(b"\n");
                data.extend_from_slice(&chunk);
                if needs_newline && file != files.last().map_or("", String::as_str) {
                    data.push(b'\n');
                }
            }
            Err(err) => {
                path_error(ctx.stderr, "sed", file, err);
                status = 2;
            }
        }
    }
    let (output, code) = run_script(&commands, quiet, &data, ctx);
    ctx.stdout.write(&output);
    if code != 0 {
        code
    } else {
        status
    }
}

fn run_script(commands: &[Command], quiet: bool, data: &[u8], ctx: &mut Ctx<'_>) -> (Vec<u8>, i32) {
    let mut writes: Vec<(String, Vec<u8>)> = Vec::new();
    for command in commands {
        let name = match &command.action {
            Action::WriteFile(name) => Some(name),
            Action::Substitute(replacement) => replacement.write_to.as_ref(),
            _ => None,
        };
        if let Some(name) = name {
            if name != "/dev/stdout" && !writes.iter().any(|(known, _)| known == name) {
                writes.push((name.to_string(), Vec::new()));
            }
        }
    }

    let mut runner = Runner {
        commands: commands.to_vec(),
        quiet,
        lines: split_lines(data),
        next_line: 0,
        line_number: 0,
        output: Vec::new(),
        writes,
        hold: Vec::new(),
        append_queue: Vec::new(),
        overflow: false,
    };
    let ends_with_newline = data.ends_with(b"\n");

    let mut status = 0;
    let mut space = Vec::new();
    let mut restart = false;
    let mut restarts = 0usize;
    loop {
        if !restart {
            let Some(line) = runner.take_line().map(<[u8]>::to_vec) else {
                break;
            };
            space = line;
            restarts = 0;
        } else {
            restarts += 1;
            if restarts > MAX_BRANCHES {
                runner.overflow = true;
                break;
            }
        }
        restart = false;

        let end = runner.cycle(&mut space, ctx);
        let (print, quit) = match end {
            CycleEnd::Normal => (!runner.quiet, None),
            CycleEnd::Deleted => (false, None),
            CycleEnd::Restart => {
                restart = true;
                (false, None)
            }
            CycleEnd::Quit(code, print) => (print && !runner.quiet, Some(code)),
        };
        if print {
            runner.output.extend_from_slice(&space);
            runner.output.push(b'\n');
        }
        runner.flush_appends();
        if let Some(code) = quit {
            status = code;
            break;
        }
    }

    if runner.overflow {
        error(ctx.stderr, "sed", "script exceeded its memory or loop limit");
        status = 1;
    }
    for (name, data) in core::mem::take(&mut runner.writes) {
        if let Err(err) = ctx.fs.write_file(&name, &data, false) {
            path_error(ctx.stderr, "sed", &name, err);
            status = 4;
        }
    }
    let mut output = core::mem::take(&mut runner.output);
    // A missing final newline stays missing, as with GNU sed.
    if !ends_with_newline && !data.is_empty() && runner.is_last() && output.ends_with(b"\n") {
        output.pop();
    }
    let _ = write_line;
    (output, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;
    use crate::fs::{OverlayFs, Vfs};
    use crate::io::{BytesInput, BytesOutput};
    use crate::system::NoSystem;

    fn run_with(fs: &mut OverlayFs, args: &[&str], input: &str) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let mut env = Env::feos();
        let mut stdin = BytesInput::new(input.as_bytes().to_vec());
        let mut stdout = BytesOutput::new();
        let mut stderr = BytesOutput::new();
        let mut ctx = Ctx {
            fs,
            env: &mut env,
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
            system: &NoSystem,
            depth: 0,
        };
        let status = sed(&args, &mut ctx);
        (
            status,
            String::from_utf8_lossy(stdout.bytes()).into_owned(),
            String::from_utf8_lossy(stderr.bytes()).into_owned(),
        )
    }

    fn run(args: &[&str], input: &str) -> String {
        let mut fs = OverlayFs::memory();
        let (status, out, err) = run_with(&mut fs, args, input);
        assert_eq!(status, 0, "sed {args:?} failed: {err}");
        out
    }

    #[test]
    fn substitutes_with_flags_and_groups() {
        assert_eq!(run(&["s/a/b/"], "aa\n"), "ba\n");
        assert_eq!(run(&["s/a/b/g"], "aa\n"), "bb\n");
        assert_eq!(run(&["s/a/b/2"], "aaa\n"), "aba\n");
        assert_eq!(run(&["s/a/b/2g"], "aaa\n"), "abb\n");
        assert_eq!(run(&["s/A/x/I"], "a\n"), "x\n");
        assert_eq!(run(&[r"s/\(a\)\(b\)/\2\1/"], "ab\n"), "ba\n");
        assert_eq!(run(&["-E", "s/(a)(b)/[&:\\2]/"], "ab\n"), "[ab:b]\n");
        assert_eq!(run(&["s/x*/-/g"], "abc\n"), "-a-b-c-\n");
        assert_eq!(run(&["s,/,|,g"], "a/b\n"), "a|b\n");
        assert_eq!(run(&[r"s/a/x\ny/"], "a\n"), "x\ny\n");
        assert_eq!(run(&["-n", "s/a/b/p"], "a\nc\n"), "b\n");
    }

    #[test]
    fn addresses_select_lines() {
        let input = "1\n2\n3\n4\n5\n";
        assert_eq!(run(&["-n", "2p"], input), "2\n");
        assert_eq!(run(&["-n", "$p"], input), "5\n");
        assert_eq!(run(&["-n", "2,3p"], input), "2\n3\n");
        assert_eq!(run(&["-n", "/2/,/4/p"], input), "2\n3\n4\n");
        assert_eq!(run(&["-n", "1~2p"], input), "1\n3\n5\n");
        assert_eq!(run(&["2!d"], input), "2\n");
        assert_eq!(run(&["2,4d"], input), "1\n5\n");
        assert_eq!(run(&["-n", "4,2p"], input), "4\n");
    }

    #[test]
    fn hold_space_and_multiline_commands_work() {
        // The classic tac and join-pairs one-liners.
        assert_eq!(run(&["-n", "1!G;h;$p"], "a\nb\nc\n"), "c\nb\na\n");
        assert_eq!(run(&["N;s/\\n/+/"], "a\nb\nc\nd\n"), "a+b\nc+d\n");
        assert_eq!(run(&["$!N;P;D"], "a\nb\nc\n"), "a\nb\nc\n");
        assert_eq!(run(&["x"], "a\nb\n"), "\na\n");
    }

    #[test]
    fn text_and_flow_commands_work() {
        assert_eq!(run(&["1i\\\nfirst"], "a\n"), "first\na\n");
        assert_eq!(run(&["1a\\\nlast"], "a\nb\n"), "a\nlast\nb\n");
        assert_eq!(run(&["1c\\\nnew"], "a\nb\n"), "new\nb\n");
        assert_eq!(run(&["2q"], "a\nb\nc\n"), "a\nb\n");
        assert_eq!(run(&["="], "a\n"), "1\na\n");
        assert_eq!(run(&["y/abc/xyz/"], "cab\n"), "zxy\n");
        assert_eq!(run(&[":a;s/^.\\{1,3\\}$/ &/;ta"], "ab\n"), "  ab\n");
        assert_eq!(run(&["/a/{s/a/b/;s/b/c/}"], "a\nx\n"), "c\nx\n");
        assert_eq!(run(&["-n", "/a/{/b/p}"], "a\nab\n"), "ab\n");
        assert_eq!(run(&["n;d"], "a\nb\nc\n"), "a\nc\n");
    }

    #[test]
    fn a_missing_final_newline_is_preserved() {
        assert_eq!(run(&["s/a/b/"], "a"), "b");
    }

    #[test]
    fn edits_files_in_place() {
        let mut fs = OverlayFs::memory();
        fs.write_file("/f", b"old\n", false).unwrap();
        let (status, out, _) = run_with(&mut fs, &["-i", "s/old/new/", "f"], "");
        assert_eq!((status, out.as_str()), (0, ""));
        assert_eq!(fs.read_file("/f").unwrap(), b"new\n");
    }

    #[test]
    fn write_commands_create_files() {
        let mut fs = OverlayFs::memory();
        run_with(&mut fs, &["-n", "/a/w out"], "a\nb\na\n");
        assert_eq!(fs.read_file("/out").unwrap(), b"a\na\n");
    }

    #[test]
    fn bad_scripts_report_errors_without_panicking() {
        let mut fs = OverlayFs::memory();
        for script in [
            "s/a", "s/a/b", "y/ab/c/", "{", "}", "b nowhere", "s/\\(/x/", "k", "1,", "s/a/\\1/",
            "y", "a", "s", ":", "/a",
        ] {
            let (status, _, _) = run_with(&mut fs, &[script], "x\n");
            if script != "a" {
                assert_ne!(status, 0, "{script}");
            }
        }
    }

    #[test]
    fn endless_loops_are_stopped() {
        let mut fs = OverlayFs::memory();
        let (status, _, err) = run_with(&mut fs, &[":a;ba"], "x\n");
        assert_eq!(status, 1);
        assert!(err.contains("limit"), "{err}");
        let (status, _, _) = run_with(&mut fs, &[":a;G;G;ba"], "xxxxxxxx\n");
        assert_eq!(status, 1);
    }
}
