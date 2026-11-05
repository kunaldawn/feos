//! Word expansion: parameters, command substitution, arithmetic, splitting
//! and pathname generation.
//!
//! Quoting is tracked per character. Each field carries its text and, when it
//! holds an unquoted metacharacter, a glob pattern in which quoted characters
//! are escaped, so `rm foo\*` can never match `fooa`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_coreutils::glob::{glob_match, has_glob, unescape_glob};
use feos_coreutils::{Kind, Vfs};

use crate::fds::OpenFile;
use crate::lexer::{self, Segment, Word};
use crate::{Flow, Host, Io, Shell};

/// Longest expansion result. Variables have their own budget, but one word can
/// be built from many of them.
const MAX_EXPANSION_BYTES: usize = 8 * 1024 * 1024;

/// Longest text the `#`, `%` and `/` operators will scan. They are quadratic in
/// the worst case, and nothing can interrupt them halfway.
const MAX_PATTERN_SUBJECT: usize = 64 * 1024;

/// Marks a failed expansion. The message has been printed and `last_status` set.
#[derive(Debug)]
pub(crate) struct ExpandFailed;

pub(crate) type Expanded<T> = Result<T, ExpandFailed>;

fn is_glob_meta(ch: char) -> bool {
    matches!(ch, '*' | '?' | '[' | ']' | '\\' | '!' | '^' | '-')
}

#[derive(Default)]
struct Field {
    text: String,
    /// The glob pattern, built only once a metacharacter appears.
    pattern: Option<String>,
}

impl Field {
    fn push_quoted(&mut self, text: &str) {
        self.text.push_str(text);
        if let Some(pattern) = &mut self.pattern {
            push_escaped(pattern, text);
        }
    }

    fn push_plain(&mut self, text: &str) {
        if self.pattern.is_none() && text.contains(['*', '?', '[']) {
            let mut pattern = String::new();
            push_escaped(&mut pattern, &self.text);
            self.pattern = Some(pattern);
        }
        self.text.push_str(text);
        if let Some(pattern) = &mut self.pattern {
            pattern.push_str(text);
        }
    }
}

fn push_escaped(pattern: &mut String, text: &str) {
    for ch in text.chars() {
        if is_glob_meta(ch) {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
}

/// Where expanded text goes.
enum Sink {
    /// Whole words, split on `IFS`.
    Fields {
        fields: Vec<Field>,
        current: Field,
        active: bool,
        ifs: String,
    },
    /// One string, for assignments and here-documents.
    Joined(String),
    /// One glob pattern, for `case` and `${x#pattern}`.
    Pattern(String),
}

impl Sink {
    fn quoted(&mut self, text: &str) {
        match self {
            Self::Fields {
                current, active, ..
            } => {
                current.push_quoted(text);
                *active = true;
            }
            Self::Joined(out) => out.push_str(text),
            Self::Pattern(out) => push_escaped(out, text),
        }
    }

    /// Unquoted source text: no splitting, but metacharacters count.
    fn plain(&mut self, text: &str) {
        match self {
            Self::Fields {
                current, active, ..
            } => {
                if !text.is_empty() {
                    current.push_plain(text);
                    *active = true;
                }
            }
            Self::Joined(out) | Self::Pattern(out) => out.push_str(text),
        }
    }

    /// The result of an unquoted expansion, which `IFS` splits.
    fn split(&mut self, text: &str) {
        let Self::Fields {
            fields,
            current,
            active,
            ifs,
        } = self
        else {
            return self.plain(text);
        };

        let is_white = |ch: char| matches!(ch, ' ' | '\t' | '\n');
        let mut start: Option<usize> = None;
        let mut chars = text.char_indices().peekable();
        while let Some((index, ch)) = chars.next() {
            if !ifs.contains(ch) {
                if start.is_none() {
                    start = Some(index);
                }
                continue;
            }
            if let Some(from) = start.take() {
                current.push_plain(&text[from..index]);
                *active = true;
            }
            // A delimiter ends the field. A non-white delimiter also makes an
            // empty field when nothing came before it.
            let white = is_white(ch);
            if *active || !white {
                fields.push(core::mem::take(current));
                *active = false;
            }
            if white {
                // Adjacent white space, and one non-white delimiter with its
                // surrounding white space, count as a single separator.
                while let Some((_, next)) = chars.peek() {
                    if ifs.contains(*next) && is_white(*next) {
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Some((_, next)) = chars.peek() {
                    if ifs.contains(*next) && !is_white(*next) {
                        chars.next();
                        while let Some((_, after)) = chars.peek() {
                            if ifs.contains(*after) && is_white(*after) {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
        }
        if let Some(from) = start {
            current.push_plain(&text[from..]);
            *active = true;
        }
    }

    /// Ends the current field, for the gaps in `"$@"`.
    fn break_field(&mut self) {
        if let Self::Fields {
            fields,
            current,
            active,
            ..
        } = self
        {
            if *active {
                fields.push(core::mem::take(current));
                *active = false;
            }
        } else {
            self.plain(" ");
        }
    }

    fn length(&self) -> usize {
        match self {
            Self::Fields {
                current, fields, ..
            } => current.text.len() + fields.iter().map(|field| field.text.len()).sum::<usize>(),
            Self::Joined(out) | Self::Pattern(out) => out.len(),
        }
    }
}

/// What a `${...}` expression asks for, once its name is known.
enum Op<'a> {
    None,
    Default {
        colon: bool,
        word: &'a str,
    },
    Assign {
        colon: bool,
        word: &'a str,
    },
    Alternate {
        colon: bool,
        word: &'a str,
    },
    Error {
        colon: bool,
        word: &'a str,
    },
    RemovePrefix {
        greedy: bool,
        pattern: &'a str,
    },
    RemoveSuffix {
        greedy: bool,
        pattern: &'a str,
    },
    Replace {
        all: bool,
        anchor: u8,
        pattern: &'a str,
        with: &'a str,
    },
    Substring {
        offset: &'a str,
        length: Option<&'a str>,
    },
    ChangeCase {
        upper: bool,
        all: bool,
        pattern: &'a str,
    },
}

/// Finds the `/` that separates the pattern from the replacement.
fn find_unquoted(text: &str, target: char) -> Option<usize> {
    let mut escaped = false;
    let mut single = false;
    let mut double = false;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !single => escaped = true,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            _ if ch == target && !single && !double => return Some(index),
            _ => {}
        }
    }
    None
}

/// Splits `${x:offset:length}`, watching for `:` inside `?:`.
fn split_substring(text: &str) -> (&str, Option<&str>) {
    let mut questions = 0usize;
    let mut depth = 0usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '?' => questions += 1,
            ':' if depth == 0 => {
                if questions == 0 {
                    return (&text[..index], Some(&text[index + 1..]));
                }
                questions -= 1;
            }
            _ => {}
        }
    }
    (text, None)
}

fn parse_op(rest: &str) -> Option<Op<'_>> {
    if rest.is_empty() {
        return Some(Op::None);
    }
    let mut chars = rest.chars();
    let first = chars.next()?;
    let second = chars.next();
    let tail = |skip: usize| &rest[skip..];
    Some(match (first, second) {
        (':', Some('-')) => Op::Default {
            colon: true,
            word: tail(2),
        },
        (':', Some('=')) => Op::Assign {
            colon: true,
            word: tail(2),
        },
        (':', Some('+')) => Op::Alternate {
            colon: true,
            word: tail(2),
        },
        (':', Some('?')) => Op::Error {
            colon: true,
            word: tail(2),
        },
        (':', _) => {
            let (offset, length) = split_substring(tail(1));
            Op::Substring { offset, length }
        }
        ('-', _) => Op::Default {
            colon: false,
            word: tail(1),
        },
        ('=', _) => Op::Assign {
            colon: false,
            word: tail(1),
        },
        ('+', _) => Op::Alternate {
            colon: false,
            word: tail(1),
        },
        ('?', _) => Op::Error {
            colon: false,
            word: tail(1),
        },
        ('#', Some('#')) => Op::RemovePrefix {
            greedy: true,
            pattern: tail(2),
        },
        ('#', _) => Op::RemovePrefix {
            greedy: false,
            pattern: tail(1),
        },
        ('%', Some('%')) => Op::RemoveSuffix {
            greedy: true,
            pattern: tail(2),
        },
        ('%', _) => Op::RemoveSuffix {
            greedy: false,
            pattern: tail(1),
        },
        ('/', _) => {
            let (all, anchor, body) = match second {
                Some('/') => (true, 0, tail(2)),
                Some('#') => (false, b'#', tail(2)),
                Some('%') => (false, b'%', tail(2)),
                _ => (false, 0, tail(1)),
            };
            let (pattern, with) = match find_unquoted(body, '/') {
                Some(at) => (&body[..at], &body[at + 1..]),
                None => (body, ""),
            };
            Op::Replace {
                all,
                anchor,
                pattern,
                with,
            }
        }
        ('^', Some('^')) => Op::ChangeCase {
            upper: true,
            all: true,
            pattern: tail(2),
        },
        ('^', _) => Op::ChangeCase {
            upper: true,
            all: false,
            pattern: tail(1),
        },
        (',', Some(',')) => Op::ChangeCase {
            upper: false,
            all: true,
            pattern: tail(2),
        },
        (',', _) => Op::ChangeCase {
            upper: false,
            all: false,
            pattern: tail(1),
        },
        _ => return None,
    })
}

/// Splits the parameter name off the front of a `${...}` body.
fn split_name(text: &str) -> Option<(&str, &str)> {
    let first = text.chars().next()?;
    if first == '_' || first.is_ascii_alphabetic() {
        let end = text
            .find(|ch: char| ch != '_' && !ch.is_ascii_alphanumeric())
            .unwrap_or(text.len());
        return Some(text.split_at(end));
    }
    if first.is_ascii_digit() {
        let end = text
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(text.len());
        return Some(text.split_at(end));
    }
    if matches!(first, '@' | '*' | '#' | '?' | '$' | '!' | '-') {
        return Some(text.split_at(1));
    }
    None
}

impl Shell<'_> {
    /// Prints an expansion error. The command fails, and the shell goes on.
    pub(crate) fn expand_error(&mut self, io: &mut Io<'_>, message: &str) -> ExpandFailed {
        self.write_err(io, &format!("sh: {message}\n"));
        self.last_status = 1;
        ExpandFailed
    }

    /// Prints an error that POSIX says ends a script: unset under `set -u`, and
    /// `${x:?}`. At a prompt it only ends the current line.
    fn expand_fatal(&mut self, io: &mut Io<'_>, message: &str) -> ExpandFailed {
        let failed = self.expand_error(io, message);
        self.flow = if self.interactive {
            Flow::Abort(1)
        } else {
            // The status Bash leaves behind in this case.
            Flow::Exit(127)
        };
        failed
    }

    /// Expands one word into zero or more command arguments.
    pub(crate) fn expand_word_into(
        &mut self,
        word: &Word,
        result: &mut Vec<String>,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<()> {
        // A word with nothing to expand is by far the most common case.
        if let Some(text) = word.plain() {
            if !self.opts.noglob && has_glob(text) {
                let mut sink = self.field_sink();
                sink.plain(text);
                return self.finish_fields(sink, result);
            }
            if !text.is_empty() {
                result.push(text.to_string());
            }
            return Ok(());
        }

        let mut sink = self.field_sink();
        self.expand_into(&word.segments, &mut sink, io, host)?;
        self.finish_fields(sink, result)
    }

    /// Expands one word into a new list of fields.
    pub(crate) fn expand_word(
        &mut self,
        word: &Word,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<Vec<String>> {
        let mut result = Vec::new();
        self.expand_word_into(word, &mut result, io, host)?;
        Ok(result)
    }

    fn field_sink(&self) -> Sink {
        Sink::Fields {
            fields: Vec::new(),
            current: Field::default(),
            active: false,
            ifs: self.env.get("IFS").unwrap_or(" \t\n").to_string(),
        }
    }

    fn finish_fields(&mut self, sink: Sink, result: &mut Vec<String>) -> Expanded<()> {
        let Sink::Fields {
            mut fields,
            current,
            active,
            ..
        } = sink
        else {
            return Ok(());
        };
        if active {
            fields.push(current);
        }
        for field in fields {
            match field.pattern {
                Some(pattern) if !self.opts.noglob && has_glob(&pattern) => {
                    let matches = self.glob_path(&pattern);
                    if matches.is_empty() {
                        result.push(field.text);
                    } else {
                        result.extend(matches);
                    }
                }
                _ => result.push(field.text),
            }
        }
        Ok(())
    }

    /// Expands a word to one string with no splitting or globbing, for
    /// assignments, redirection bodies and `case` subjects.
    pub(crate) fn expand_assign(
        &mut self,
        word: &Word,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<String> {
        let mut sink = Sink::Joined(String::new());
        self.expand_into(&word.segments, &mut sink, io, host)?;
        match sink {
            Sink::Joined(text) => Ok(text),
            _ => Ok(String::new()),
        }
    }

    /// Expands a word to a glob pattern with quoted parts escaped.
    pub(crate) fn expand_pattern(
        &mut self,
        word: &Word,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<String> {
        let mut sink = Sink::Pattern(String::new());
        self.expand_into(&word.segments, &mut sink, io, host)?;
        match sink {
            Sink::Pattern(text) => Ok(text),
            _ => Ok(String::new()),
        }
    }

    fn expand_into(
        &mut self,
        segments: &[Segment],
        sink: &mut Sink,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<()> {
        for segment in segments {
            match segment {
                Segment::Literal(text, true) => sink.quoted(text),
                Segment::Literal(text, false) => sink.plain(text),
                Segment::Tilde => {
                    let home = self.env.get("HOME").unwrap_or("/").to_string();
                    sink.quoted(&home);
                }
                Segment::Var(name, quoted) => match self.var_value(name) {
                    Some(value) => emit(sink, &value, *quoted),
                    None => {
                        if self.opts.nounset {
                            return Err(self.expand_fatal(io, &format!("{name}: unbound variable")));
                        }
                    }
                },
                Segment::Special(ch, quoted) => self.expand_special(*ch, *quoted, sink, io)?,
                Segment::Param(text, quoted) => self.expand_param(text, *quoted, sink, io, host)?,
                Segment::CommandSub(script, quoted) => {
                    let output = self.capture(script, io, host)?;
                    emit(sink, &output, *quoted);
                }
                Segment::Arith(expression, quoted) => {
                    let value = self.eval_arith_text(expression, io, host)?;
                    emit(sink, &value.to_string(), *quoted);
                }
            }
            if sink.length() > MAX_EXPANSION_BYTES {
                return Err(self.expand_error(io, "expansion is too large"));
            }
        }
        Ok(())
    }

    /// Looks up a variable, including the dynamic ones.
    pub(crate) fn var_value(&mut self, name: &str) -> Option<String> {
        if let Some(value) = self.env.get(name) {
            return Some(value.to_string());
        }
        match name {
            "RANDOM" => {
                // xorshift: there is no entropy source, and scripts only need
                // numbers that differ.
                self.random ^= self.random << 13;
                self.random ^= self.random >> 7;
                self.random ^= self.random << 17;
                Some(((self.random >> 16) & 0x7fff).to_string())
            }
            "SECONDS" => Some((self.system.uptime_ms() / 1000).to_string()),
            _ => None,
        }
    }

    fn special_value(&self, ch: char) -> Option<String> {
        match ch {
            '?' => Some(self.last_status.to_string()),
            '$' => Some(self.system.pid().to_string()),
            '#' => Some(self.params.len().to_string()),
            '!' => self.last_background.map(|pid| pid.to_string()),
            '-' => Some(self.option_flags()),
            '0' => Some(self.arg0.clone()),
            '1'..='9' => self.params.get(ch as usize - '1' as usize).cloned(),
            _ => None,
        }
    }

    fn expand_special(
        &mut self,
        ch: char,
        quoted: bool,
        sink: &mut Sink,
        io: &mut Io<'_>,
    ) -> Expanded<()> {
        if ch == '@' || ch == '*' {
            self.emit_params(ch, quoted, None, sink);
            return Ok(());
        }
        match self.special_value(ch) {
            Some(value) => emit(sink, &value, quoted),
            None => {
                if self.opts.nounset && ch != '!' {
                    return Err(self.expand_fatal(io, &format!("{ch}: unbound variable")));
                }
            }
        }
        Ok(())
    }

    /// Expands `$@` or `$*`, optionally over a slice of the parameters.
    fn emit_params(
        &mut self,
        ch: char,
        quoted: bool,
        range: Option<(usize, usize)>,
        sink: &mut Sink,
    ) {
        let params = core::mem::take(&mut self.params);
        let (from, to) = range.unwrap_or((0, params.len()));
        let selected = params.get(from..to.min(params.len())).unwrap_or(&[]);
        if ch == '*' && quoted {
            let separator = self
                .env
                .get("IFS")
                .map_or(Some(' '), |ifs| ifs.chars().next());
            let mut joined = String::new();
            for (index, param) in selected.iter().enumerate() {
                if index > 0 {
                    joined.extend(separator);
                }
                joined.push_str(param);
            }
            sink.quoted(&joined);
        } else {
            for (index, param) in selected.iter().enumerate() {
                if index > 0 {
                    sink.break_field();
                }
                if quoted {
                    sink.quoted(param);
                } else {
                    sink.split(param);
                }
            }
        }
        self.params = params;
    }

    fn param_lookup(&mut self, name: &str) -> Option<String> {
        if let Some(first) = name.chars().next() {
            if first.is_ascii_digit() && name != "0" {
                let index: usize = name.parse().ok()?;
                return index
                    .checked_sub(1)
                    .and_then(|at| self.params.get(at))
                    .cloned();
            }
            if !first.is_ascii_alphabetic() && first != '_' {
                if first == '@' || first == '*' {
                    return if self.params.is_empty() {
                        None
                    } else {
                        Some(self.params.join(" "))
                    };
                }
                return self.special_value(first);
            }
        }
        self.var_value(name)
    }

    fn expand_param(
        &mut self,
        text: &str,
        quoted: bool,
        sink: &mut Sink,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<()> {
        if !self.enter_nested(io) {
            return Err(ExpandFailed);
        }
        let result = self.expand_param_inner(text, quoted, sink, io, host);
        self.leave_nested();
        result
    }

    fn expand_param_inner(
        &mut self,
        text: &str,
        quoted: bool,
        sink: &mut Sink,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<()> {
        let bad = |shell: &mut Self, io: &mut Io<'_>| -> ExpandFailed {
            shell.expand_error(io, &format!("${{{text}}}: bad substitution"))
        };

        // `${#name}` is a length, but `${#}` and `${#-x}` are about `$#`.
        if let Some(rest) = text.strip_prefix('#') {
            if let Some((name, "")) = split_name(rest) {
                if name == "@" || name == "*" {
                    emit(sink, &self.params.len().to_string(), quoted);
                    return Ok(());
                }
                let value = self.param_lookup(name);
                if value.is_none() && self.opts.nounset {
                    return Err(self.expand_fatal(io, &format!("{name}: unbound variable")));
                }
                let length = value.map_or(0, |value| value.chars().count());
                emit(sink, &length.to_string(), quoted);
                return Ok(());
            }
        }

        // `${!name}` reads the variable whose name is in `name`.
        let (name, rest, indirect) = match text.strip_prefix('!') {
            Some(after) if !after.is_empty() => match split_name(after) {
                Some((name, rest)) => (name, rest, true),
                None => return Err(bad(self, io)),
            },
            _ => match split_name(text) {
                Some((name, rest)) => (name, rest, false),
                None => return Err(bad(self, io)),
            },
        };
        let Some(op) = parse_op(rest) else {
            return Err(bad(self, io));
        };

        if !indirect && (name == "@" || name == "*") {
            let which = name.chars().next().unwrap_or('@');
            match op {
                Op::None => {
                    self.emit_params(which, quoted, None, sink);
                    return Ok(());
                }
                Op::Substring { offset, length } => {
                    let count = self.params.len() as i64;
                    let start = self.eval_arith_text(offset, io, host)?;
                    // `${@:1}` is the first parameter.
                    let from = if start < 0 { count + start } else { start - 1 }.clamp(0, count);
                    let to = match length {
                        Some(length) => {
                            let length = self.eval_arith_text(length, io, host)?;
                            if length < 0 {
                                return Err(self.expand_error(io, "substring expression < 0"));
                            }
                            from.saturating_add(length).min(count)
                        }
                        None => count,
                    };
                    self.emit_params(which, quoted, Some((from as usize, to as usize)), sink);
                    return Ok(());
                }
                _ => {}
            }
        }

        let mut value = self.param_lookup(name);
        if indirect {
            value = match value {
                Some(target) => {
                    if lexer::valid_name(&target) || target.chars().all(|ch| ch.is_ascii_digit()) {
                        self.param_lookup(&target)
                    } else {
                        return Err(bad(self, io));
                    }
                }
                None => None,
            };
        }
        let unset_or_empty = |value: &Option<String>, colon: bool| match value {
            None => true,
            Some(text) => colon && text.is_empty(),
        };

        match op {
            Op::None => match value {
                Some(value) => emit(sink, &value, quoted),
                None => {
                    if self.opts.nounset && name != "@" && name != "*" {
                        return Err(self.expand_fatal(io, &format!("{name}: unbound variable")));
                    }
                }
            },
            Op::Default { colon, word } => {
                if unset_or_empty(&value, colon) {
                    self.expand_operand(word, quoted, sink, io, host)?;
                } else if let Some(value) = value {
                    emit(sink, &value, quoted);
                }
            }
            Op::Assign { colon, word } => {
                if unset_or_empty(&value, colon) {
                    if !lexer::valid_name(name) {
                        return Err(
                            self.expand_error(io, &format!("${name}: cannot assign in this way"))
                        );
                    }
                    let assigned = self.operand_text(word, quoted, io, host)?;
                    if let Err(error) = self.env.try_set(name, &assigned) {
                        return Err(self.expand_error(io, &format!("{name}: {}", error.text())));
                    }
                    emit(sink, &assigned, quoted);
                } else if let Some(value) = value {
                    emit(sink, &value, quoted);
                }
            }
            Op::Alternate { colon, word } => {
                if !unset_or_empty(&value, colon) {
                    self.expand_operand(word, quoted, sink, io, host)?;
                }
            }
            Op::Error { colon, word } => {
                if unset_or_empty(&value, colon) {
                    let message = if word.is_empty() {
                        String::from("parameter null or not set")
                    } else {
                        self.operand_text(word, quoted, io, host)?
                    };
                    return Err(self.expand_fatal(io, &format!("{name}: {message}")));
                }
                if let Some(value) = value {
                    emit(sink, &value, quoted);
                }
            }
            Op::RemovePrefix { greedy, pattern } => {
                if let Some(value) = self.need_value(name, value, io)? {
                    let pattern = self.operand_pattern(pattern, io, host)?;
                    self.check_subject(&value, io)?;
                    emit(sink, remove_prefix(&value, &pattern, greedy), quoted);
                }
            }
            Op::RemoveSuffix { greedy, pattern } => {
                if let Some(value) = self.need_value(name, value, io)? {
                    let pattern = self.operand_pattern(pattern, io, host)?;
                    self.check_subject(&value, io)?;
                    emit(sink, remove_suffix(&value, &pattern, greedy), quoted);
                }
            }
            Op::Replace {
                all,
                anchor,
                pattern,
                with,
            } => {
                if let Some(value) = self.need_value(name, value, io)? {
                    let pattern = self.operand_pattern(pattern, io, host)?;
                    let with = self.operand_text(with, quoted, io, host)?;
                    self.check_subject(&value, io)?;
                    emit(sink, &replace(&value, &pattern, &with, all, anchor), quoted);
                }
            }
            Op::Substring { offset, length } => {
                if let Some(value) = self.need_value(name, value, io)? {
                    let offset = self.eval_arith_text(offset, io, host)?;
                    let length = match length {
                        Some(length) => Some(self.eval_arith_text(length, io, host)?),
                        None => None,
                    };
                    let Some(slice) = substring(&value, offset, length) else {
                        return Err(self.expand_error(io, "substring expression < 0"));
                    };
                    emit(sink, &slice, quoted);
                }
            }
            Op::ChangeCase {
                upper,
                all,
                pattern,
            } => {
                if let Some(value) = self.need_value(name, value, io)? {
                    let pattern = if pattern.is_empty() {
                        String::from("?")
                    } else {
                        self.operand_pattern(pattern, io, host)?
                    };
                    emit(sink, &change_case(&value, &pattern, upper, all), quoted);
                }
            }
        }
        Ok(())
    }

    fn need_value(
        &mut self,
        name: &str,
        value: Option<String>,
        io: &mut Io<'_>,
    ) -> Expanded<Option<String>> {
        if value.is_none() && self.opts.nounset {
            return Err(self.expand_fatal(io, &format!("{name}: unbound variable")));
        }
        Ok(value)
    }

    fn check_subject(&mut self, value: &str, io: &mut Io<'_>) -> Expanded<()> {
        if value.len() > MAX_PATTERN_SUBJECT {
            return Err(self.expand_error(io, "value is too long for a pattern operation"));
        }
        Ok(())
    }

    fn lex_operand_word(&mut self, text: &str, quoted: bool, io: &mut Io<'_>) -> Expanded<Word> {
        lexer::lex_operand(text, quoted).map_err(|error| self.expand_error(io, error.message))
    }

    fn expand_operand(
        &mut self,
        text: &str,
        quoted: bool,
        sink: &mut Sink,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<()> {
        let word = self.lex_operand_word(text, quoted, io)?;
        // An unquoted operand's own expansions split, and `"$x"` in it does not.
        self.expand_into(&word.segments, sink, io, host)
    }

    fn operand_text(
        &mut self,
        text: &str,
        quoted: bool,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<String> {
        let word = self.lex_operand_word(text, quoted, io)?;
        self.expand_assign(&word, io, host)
    }

    fn operand_pattern(
        &mut self,
        text: &str,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<String> {
        // A pattern's metacharacters count even inside double quotes.
        let word = self.lex_operand_word(text, false, io)?;
        self.expand_pattern(&word, io, host)
    }

    /// Runs a command substitution in a copy of the shell's state.
    pub(crate) fn capture(
        &mut self,
        script: &str,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<String> {
        if !self.enter_nested(io) {
            return Err(ExpandFailed);
        }
        let saved = self.fds.save();
        let capture = self.fds.open(OpenFile::Capture {
            data: Vec::new(),
            overflow: false,
        });
        self.fds.hold(capture);
        let mut leftovers = Vec::new();
        self.fds.bind(1, capture, &mut leftovers);

        let snapshot = self.snapshot();
        // As in Bash, `set -e` does not reach into a command substitution; the
        // failure shows up in the status of the command that used it.
        self.opts.errexit = false;
        let status = self.run_text_inner(script, io, host);
        let flow = core::mem::replace(&mut self.flow, Flow::None);
        self.restore_snapshot(snapshot);

        self.finish_redirects(saved, io);
        let (data, overflow) = self.fds.take_capture(capture);
        self.flush_leftovers(leftovers, io);
        self.leave_nested();
        self.last_status = status;
        self.subst_status = Some(status);

        // An interrupt or a nesting failure must keep unwinding.
        if matches!(flow, Flow::Interrupted | Flow::Abort(_)) {
            self.flow = flow;
            return Err(ExpandFailed);
        }
        if overflow || data.len() > MAX_EXPANSION_BYTES {
            return Err(self.expand_error(io, "command substitution output is too large"));
        }

        let mut text = match String::from_utf8(data) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        };
        text.retain(|ch| ch != '\0');
        let kept = text.trim_end_matches('\n').len();
        text.truncate(kept);
        Ok(text)
    }

    /// Generates pathnames for a glob pattern with escapes.
    pub(crate) fn glob_path(&mut self, pattern: &str) -> Vec<String> {
        let absolute = pattern.starts_with('/');
        let dirs_only = pattern.ends_with('/');
        let mut paths: Vec<String> = alloc::vec![if absolute {
            String::from("/")
        } else {
            String::new()
        }];

        let components: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
        let last = components.len().saturating_sub(1);

        for (index, component) in components.iter().enumerate() {
            let is_last = index == last;
            if has_glob(component) {
                let explicit_dot = component.starts_with('.') || component.starts_with("\\.");
                let mut next = Vec::new();
                for path in &paths {
                    let lookup = if path.is_empty() { "." } else { path.as_str() };
                    let Ok(entries) = self.fs.read_dir(lookup) else {
                        continue;
                    };
                    for entry in entries {
                        if entry.name.starts_with('.') && !explicit_dot {
                            continue;
                        }
                        if glob_match(component, &entry.name) {
                            next.push(join_display(path, &entry.name));
                        }
                    }
                }
                paths = next;
            } else {
                // `.`, `..` and plain names are appended as written, so `d/../*`
                // reports what the user asked for.
                let literal = unescape_glob(component);
                for path in paths.iter_mut() {
                    *path = join_display(path, &literal);
                }
            }

            paths.retain(|path| {
                let lookup = if path.is_empty() { "." } else { path.as_str() };
                match self.fs.meta(lookup) {
                    Ok(meta) => (is_last && !dirs_only) || meta.kind == Kind::Dir,
                    Err(_) => false,
                }
            });
        }

        paths.sort();
        paths.dedup();
        if dirs_only {
            for path in paths.iter_mut() {
                if !path.ends_with('/') {
                    path.push('/');
                }
            }
        }
        paths
    }

    /// Expands `$x` and `$(cmd)` inside an expression, then evaluates it.
    pub(crate) fn eval_arith_text(
        &mut self,
        expression: &str,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Expanded<i64> {
        let text = if expression.contains(['$', '`', '"', '\\', '\'']) {
            // The whole expression behaves as if double-quoted.
            let word = self.lex_operand_word(expression, true, io)?;
            self.expand_assign(&word, io, host)?
        } else {
            String::from(expression)
        };
        match crate::arith::evaluate(&text, &mut self.env) {
            Ok(value) => Ok(value),
            Err(message) => Err(self.expand_error(io, &format!("{expression}: {message}"))),
        }
    }
}

fn emit(sink: &mut Sink, text: &str, quoted: bool) {
    if quoted {
        sink.quoted(text);
    } else {
        sink.split(text);
    }
}

fn join_display(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_string()
    } else if directory == "/" {
        format!("/{name}")
    } else {
        format!("{directory}/{name}")
    }
}

/// Offsets where a character starts, plus the end of the text.
fn boundaries(text: &str) -> impl DoubleEndedIterator<Item = usize> + '_ {
    text.char_indices()
        .map(|(index, _)| index)
        .chain(core::iter::once(text.len()))
}

fn remove_prefix<'a>(value: &'a str, pattern: &str, greedy: bool) -> &'a str {
    let matched = |end: &usize| glob_match(pattern, &value[..*end]);
    let found = if greedy {
        boundaries(value).rev().find(matched)
    } else {
        boundaries(value).find(matched)
    };
    found.map_or(value, |end| &value[end..])
}

fn remove_suffix<'a>(value: &'a str, pattern: &str, greedy: bool) -> &'a str {
    let matched = |start: &usize| glob_match(pattern, &value[*start..]);
    let found = if greedy {
        boundaries(value).find(matched)
    } else {
        boundaries(value).rev().find(matched)
    };
    found.map_or(value, |start| &value[..start])
}

/// Finds the longest match of `pattern` that starts at `start`.
fn longest_match(value: &str, pattern: &str, start: usize) -> Option<usize> {
    boundaries(&value[start..])
        .rev()
        .map(|end| start + end)
        .find(|end| glob_match(pattern, &value[start..*end]))
}

fn replace(value: &str, pattern: &str, with: &str, all: bool, anchor: u8) -> String {
    if pattern.is_empty() {
        return value.to_string();
    }
    match anchor {
        b'#' => {
            return match longest_match(value, pattern, 0) {
                Some(end) => format!("{with}{}", &value[end..]),
                None => value.to_string(),
            };
        }
        b'%' => {
            return match boundaries(value).find(|start| glob_match(pattern, &value[*start..])) {
                Some(start) => format!("{}{with}", &value[..start]),
                None => value.to_string(),
            };
        }
        _ => {}
    }

    let mut result = String::new();
    let mut start = 0usize;
    let mut copied = 0usize;
    while start <= value.len() {
        match longest_match(value, pattern, start) {
            Some(end) => {
                result.push_str(&value[copied..start]);
                result.push_str(with);
                copied = end;
                if !all {
                    break;
                }
                start = if end > start {
                    end
                } else {
                    next_boundary(value, start)
                };
            }
            None => start = next_boundary(value, start),
        }
    }
    if let Some(rest) = value.get(copied..) {
        result.push_str(rest);
    }
    result
}

fn next_boundary(text: &str, at: usize) -> usize {
    text.get(at..)
        .and_then(|rest| rest.chars().next())
        .map_or(text.len() + 1, |ch| at + ch.len_utf8())
}

/// `${x:offset:length}` in characters. A negative length means an end offset.
fn substring(value: &str, offset: i64, length: Option<i64>) -> Option<String> {
    let count = value.chars().count() as i64;
    let start = if offset < 0 {
        (count + offset).max(0)
    } else {
        offset.min(count)
    };
    let end = match length {
        None => count,
        Some(length) if length >= 0 => start.saturating_add(length).min(count),
        Some(length) => {
            let end = count + length;
            if end < start {
                return None;
            }
            end
        }
    };
    Some(
        value
            .chars()
            .skip(start as usize)
            .take((end - start) as usize)
            .collect(),
    )
}

fn change_case(value: &str, pattern: &str, upper: bool, all: bool) -> String {
    let mut result = String::with_capacity(value.len());
    let mut buffer = [0u8; 4];
    for (index, ch) in value.chars().enumerate() {
        let selected = (all || index == 0) && glob_match(pattern, ch.encode_utf8(&mut buffer));
        if !selected {
            result.push(ch);
        } else if upper {
            result.push(ch.to_ascii_uppercase());
        } else {
            result.push(ch.to_ascii_lowercase());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{run, NoHost};
    use feos_coreutils::NoSystem;

    #[test]
    fn quoted_and_unquoted_expansion() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut host = NoHost;
        run(&mut shell, &mut host, "name='a b'");
        let (_, out, _) = run(&mut shell, &mut host, "printf '<%s>\\n' $name");
        assert_eq!(out, "<a>\n<b>\n");
        let (_, out, _) = run(&mut shell, &mut host, "printf '<%s>\\n' \"$name\"");
        assert_eq!(out, "<a b>\n");
    }

    #[test]
    fn pattern_operators_respect_character_boundaries() {
        assert_eq!(remove_prefix("éàb", "?", false), "àb");
        assert_eq!(remove_prefix("a/b/c", "*/", false), "b/c");
        assert_eq!(remove_prefix("a/b/c", "*/", true), "c");
        assert_eq!(remove_suffix("a.b.c", ".*", false), "a.b");
        assert_eq!(remove_suffix("a.b.c", ".*", true), "a");
        assert_eq!(remove_suffix("éà", "?", false), "é");
        assert_eq!(replace("aXbXc", "X", "-", false, 0), "a-bXc");
        assert_eq!(replace("aXbXc", "X", "-", true, 0), "a-b-c");
        assert_eq!(replace("abc", "a", "z", false, b'#'), "zbc");
        assert_eq!(replace("abc", "c", "z", false, b'%'), "abz");
        assert_eq!(replace("abc", "z", "y", true, 0), "abc");
        assert_eq!(replace("éaé", "é", "e", true, 0), "eae");
    }

    #[test]
    fn substrings_count_characters() {
        assert_eq!(substring("héllo", 1, Some(2)).as_deref(), Some("él"));
        assert_eq!(substring("hello", -3, None).as_deref(), Some("llo"));
        assert_eq!(substring("hello", 1, Some(-1)).as_deref(), Some("ell"));
        assert_eq!(substring("hello", 99, None).as_deref(), Some(""));
        assert_eq!(substring("hello", 3, Some(-3)), None);
    }

    #[test]
    fn case_operators_honour_patterns() {
        assert_eq!(change_case("hello", "?", true, false), "Hello");
        assert_eq!(change_case("hello", "?", true, true), "HELLO");
        assert_eq!(change_case("HELLO", "?", false, true), "hello");
        assert_eq!(change_case("hello", "[l]", true, true), "heLLo");
    }
}
