//! Shell tokenizer.
//!
//! The lexer understands the parts of Bash syntax the executor supports:
//! quoting, escapes, parameter and command substitution, here-documents,
//! redirections, pipelines and list operators. It is deliberately `char`
//! oriented and keeps the original quoting information per word fragment so
//! expansion can perform word splitting and pathname generation correctly.
//!
//! [`Lexer`] works one logical line at a time, so the parser can run a script
//! command by command and a here-document body is always attached to its
//! operator by the time the parser sees it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A fragment of a word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Literal text; the flag says whether it was quoted.
    Literal(String, bool),
    /// `$NAME`.
    Var(String, bool),
    /// `$?`, `$$`, `$#`, `$@`, `$*`, `$!`, `$-`, `$0` … `$9`.
    Special(char, bool),
    /// `${...}` with its inner text.
    Param(String, bool),
    /// `$(command)` or a backquoted command.
    CommandSub(String, bool),
    /// `$((expression))`.
    Arith(String, bool),
    /// A leading `~`.
    Tilde,
}

/// A shell word, split into expandable segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// Fragments in source order.
    pub segments: Vec<Segment>,
    /// True when the word contains no quotes, escapes or expansions.
    pub simple: bool,
}

/// `NAME=value` or `NAME+=value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    /// The variable.
    pub name: String,
    /// True for `+=`.
    pub append: bool,
    /// The right-hand side.
    pub value: Word,
}

impl Word {
    /// Returns the literal text when the word is completely plain.
    pub fn plain(&self) -> Option<&str> {
        if !self.simple {
            return None;
        }
        match self.segments.as_slice() {
            [Segment::Literal(text, false)] => Some(text),
            _ => None,
        }
    }

    /// A word that is exactly `text`, with no quoting.
    pub fn literal(text: &str) -> Self {
        Self {
            segments: alloc::vec![Segment::Literal(text.to_string(), false)],
            simple: true,
        }
    }

    /// A display form used by error messages.
    pub fn raw(&self) -> String {
        let mut text = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(value, _) | Segment::Var(value, _) | Segment::Param(value, _) => {
                    text.push_str(value)
                }
                Segment::Special(ch, _) => {
                    text.push('$');
                    text.push(*ch);
                }
                Segment::CommandSub(script, _) => {
                    text.push_str("$(");
                    text.push_str(script);
                    text.push(')');
                }
                Segment::Arith(expr, _) => {
                    text.push_str("$((");
                    text.push_str(expr);
                    text.push_str("))");
                }
                Segment::Tilde => text.push('~'),
            }
        }
        text
    }

    /// Splits `NAME=value` or `NAME+=value` when `NAME` is valid.
    pub fn split_assignment(&self) -> Option<Assignment> {
        let (first, rest) = self.segments.split_first()?;
        let Segment::Literal(text, false) = first else {
            return None;
        };
        let index = text.find('=')?;
        let (name, append) = match text[..index].strip_suffix('+') {
            Some(name) => (name, true),
            None => (&text[..index], false),
        };
        if !valid_name(name) {
            return None;
        }
        let value = &text[index + 1..];
        let mut segments = Vec::new();
        // `x=~/bin` expands the tilde, as a word start would.
        let remainder = match value.strip_prefix('~') {
            Some(after) if after.is_empty() && rest.is_empty() || after.starts_with('/') => {
                segments.push(Segment::Tilde);
                after
            }
            _ => value,
        };
        if !remainder.is_empty() {
            segments.push(Segment::Literal(remainder.to_string(), false));
        }
        segments.extend_from_slice(rest);
        Some(Assignment {
            name: name.to_string(),
            append,
            value: Word {
                segments,
                simple: false,
            },
        })
    }
}

/// True when `name` can be a variable or function name.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// A redirection operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirOp {
    /// `<file` (`fd` is normally 0).
    Read(u8),
    /// `>file`.
    Write(u8),
    /// `>|file`: write even under `set -C`.
    Clobber(u8),
    /// `>>file`.
    Append(u8),
    /// `<>file`.
    ReadWrite(u8),
    /// `&>file`: stdout and stderr.
    WriteBoth,
    /// `&>>file`.
    AppendBoth,
    /// `n>&m` or `n<&m`.
    Dup(u8, u8),
    /// `n>&word` where the word is only known after expansion.
    DupWord(u8),
    /// `n>&-` or `n<&-`.
    Close(u8),
    /// `<<DELIM` or `<<-DELIM`; the target word is the body.
    HereDoc(u8),
    /// `<<<word`.
    HereString(u8),
}

/// One lexical token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// A word.
    Word(Word),
    /// `;`
    Seq,
    /// `;;`
    CaseEnd,
    /// `;&`
    CaseFall,
    /// `;;&`
    CaseContinue,
    /// Newline.
    Newline,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `|`
    Pipe,
    /// `|&`
    PipeBoth,
    /// `&`
    Amp,
    /// `<`, `>`, `>>`, `2>` …
    Redir(RedirOp),
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `(( expression ))` in command position.
    Arith(String),
    /// `(( init; test; step ))` after `for`.
    ArithFor(String),
}

/// A lexical error.
#[derive(Clone, Debug)]
pub struct LexError {
    /// Human-readable reason.
    pub message: &'static str,
    /// True when reading more input could complete the construct.
    pub incomplete: bool,
}

impl LexError {
    fn incomplete(message: &'static str) -> Self {
        Self {
            message,
            incomplete: true,
        }
    }

    fn error(message: &'static str) -> Self {
        Self {
            message,
            incomplete: false,
        }
    }
}

/// A here-document whose body has not been read yet.
struct PendingHereDoc {
    /// Where the body word goes in the token list being built.
    token: usize,
    delimiter: String,
    quoted: bool,
    strip_tabs: bool,
}

/// A resumable tokenizer over a whole script.
pub struct Lexer {
    chars: Vec<char>,
    index: usize,
    command_start: bool,
    after_for: bool,
    in_cond: bool,
    next_is_regex: bool,
}

impl Lexer {
    /// Creates a lexer over `input`.
    pub fn new(input: &str) -> Self {
        Self {
            chars: input.chars().collect(),
            index: 0,
            command_start: true,
            after_for: false,
            in_cond: false,
            next_is_regex: false,
        }
    }

    /// True once every character has been consumed.
    pub fn at_end(&self) -> bool {
        self.index >= self.chars.len()
    }

    fn note_word(&mut self, word: &Word) {
        self.after_for = false;
        self.command_start = match word.plain() {
            Some("for") => {
                self.after_for = true;
                false
            }
            Some("if" | "then" | "elif" | "else" | "while" | "until" | "do" | "{" | "!" | "}"
                | "fi" | "done" | "esac") => true,
            _ => false,
        };
    }

    /// Lexes up to and including the next unquoted newline.
    pub fn next_line(&mut self, tokens: &mut Vec<Token>) -> Result<(), LexError> {
        let mut pending: Vec<PendingHereDoc> = Vec::new();

        while self.index < self.chars.len() {
            let chars = &self.chars;
            let index = self.index;
            let ch = chars[index];
            let next = chars.get(index + 1).copied();

            if self.in_cond && !matches!(ch, ' ' | '\t' | '\r' | '\n') {
                self.lex_cond_token(tokens)?;
                continue;
            }

            match ch {
                ' ' | '\t' | '\r' => self.index += 1,
                '\\' if next == Some('\n') => self.index += 2,
                '\n' => {
                    self.index += 1;
                    if !self.in_cond {
                        tokens.push(Token::Newline);
                        self.command_start = true;
                        if !pending.is_empty() {
                            self.read_heredocs(tokens, &mut pending)?;
                        }
                        return Ok(());
                    }
                }
                '#' => {
                    while self.index < self.chars.len() && self.chars[self.index] != '\n' {
                        self.index += 1;
                    }
                }
                ';' => {
                    let token = match (next, chars.get(index + 2)) {
                        (Some(';'), Some('&')) => {
                            self.index += 3;
                            Token::CaseContinue
                        }
                        (Some(';'), _) => {
                            self.index += 2;
                            Token::CaseEnd
                        }
                        (Some('&'), _) => {
                            self.index += 2;
                            Token::CaseFall
                        }
                        _ => {
                            self.index += 1;
                            Token::Seq
                        }
                    };
                    tokens.push(token);
                    self.command_start = true;
                }
                '&' if next == Some('&') => {
                    tokens.push(Token::And);
                    self.index += 2;
                    self.command_start = true;
                }
                '&' if next == Some('>') => {
                    let (op, end) = parse_redirection(chars, index)?;
                    tokens.push(Token::Redir(op));
                    self.index = end;
                    self.command_start = false;
                }
                '&' => {
                    tokens.push(Token::Amp);
                    self.index += 1;
                    self.command_start = true;
                }
                '|' if next == Some('|') => {
                    tokens.push(Token::Or);
                    self.index += 2;
                    self.command_start = true;
                }
                '|' if next == Some('&') => {
                    tokens.push(Token::PipeBoth);
                    self.index += 2;
                    self.command_start = true;
                }
                '|' => {
                    tokens.push(Token::Pipe);
                    self.index += 1;
                    self.command_start = true;
                }
                '(' if next == Some('(') && (self.command_start || self.after_for) => {
                    if let Some((inner, end)) = scan_arith_command(chars, index) {
                        tokens.push(if self.after_for {
                            Token::ArithFor(inner)
                        } else {
                            Token::Arith(inner)
                        });
                        self.index = end;
                        self.after_for = false;
                        self.command_start = false;
                    } else {
                        tokens.push(Token::LParen);
                        self.index += 1;
                        self.command_start = true;
                    }
                }
                '(' => {
                    tokens.push(Token::LParen);
                    self.index += 1;
                    self.command_start = true;
                }
                ')' => {
                    tokens.push(Token::RParen);
                    self.index += 1;
                    self.command_start = true;
                }
                '<' | '>' => {
                    self.lex_redirection(tokens, &mut pending)?;
                }
                '0'..='9' if fd_prefix_length(chars, index).is_some() => {
                    self.lex_redirection(tokens, &mut pending)?;
                }
                _ => {
                    let (word, end) = lex_word(chars, index, WordMode::Normal)?;
                    self.index = end;
                    if self.command_start && word.plain() == Some("[[") {
                        self.in_cond = true;
                    }
                    self.note_word(&word);
                    tokens.push(Token::Word(word));
                }
            }
        }

        if let Some(first) = pending.first() {
            // A script may end its last here-document with end of input, but a
            // line being typed needs the terminator.
            let _ = first;
            return Err(LexError::incomplete("unterminated here-document"));
        }
        Ok(())
    }

    fn lex_redirection(
        &mut self,
        tokens: &mut Vec<Token>,
        pending: &mut Vec<PendingHereDoc>,
    ) -> Result<(), LexError> {
        let (op, end) = parse_redirection(&self.chars, self.index)?;
        self.index = end;
        self.command_start = false;
        tokens.push(Token::Redir(op));
        if let RedirOp::HereDoc(_) = op {
            let strip_tabs = self.chars.get(self.index) == Some(&'-');
            if strip_tabs {
                self.index += 1;
            }
            while matches!(self.chars.get(self.index), Some(' ' | '\t')) {
                self.index += 1;
            }
            let (word, end) = lex_word(&self.chars, self.index, WordMode::Normal)?;
            if end == self.index {
                return Err(LexError::error("expected a here-document delimiter"));
            }
            self.index = end;
            let quoted = !word.simple;
            pending.push(PendingHereDoc {
                token: tokens.len(),
                delimiter: word.raw(),
                quoted,
                strip_tabs,
            });
            // The body replaces this placeholder when the line ends.
            tokens.push(Token::Word(Word::literal("")));
        }
        Ok(())
    }

    fn read_heredocs(
        &mut self,
        tokens: &mut [Token],
        pending: &mut Vec<PendingHereDoc>,
    ) -> Result<(), LexError> {
        for doc in pending.drain(..) {
            let mut body = String::new();
            let mut closed = false;
            while self.index < self.chars.len() {
                let start = self.index;
                while self.index < self.chars.len() && self.chars[self.index] != '\n' {
                    self.index += 1;
                }
                let mut line: String = self.chars[start..self.index].iter().collect();
                let had_newline = self.index < self.chars.len();
                if had_newline {
                    self.index += 1;
                }
                if doc.strip_tabs {
                    line = line.trim_start_matches('\t').to_string();
                }
                if line == doc.delimiter {
                    closed = true;
                    break;
                }
                body.push_str(&line);
                if had_newline {
                    body.push('\n');
                }
            }
            if !closed {
                return Err(LexError::incomplete("unterminated here-document"));
            }
            let word = if doc.quoted {
                Word {
                    segments: alloc::vec![Segment::Literal(body, true)],
                    simple: false,
                }
            } else {
                lex_heredoc_body(&body)?
            };
            if let Some(slot) = tokens.get_mut(doc.token) {
                *slot = Token::Word(word);
            }
        }
        Ok(())
    }

    /// Inside `[[ ]]`, operators are ordinary words for the parser to read.
    fn lex_cond_token(&mut self, tokens: &mut Vec<Token>) -> Result<(), LexError> {
        let chars = &self.chars;
        let index = self.index;
        if self.next_is_regex {
            self.next_is_regex = false;
            let (word, end) = lex_word(chars, index, WordMode::Regex)?;
            self.index = end;
            tokens.push(Token::Word(word));
            return Ok(());
        }
        let operator = match (chars[index], chars.get(index + 1)) {
            ('&', Some('&')) => Some("&&"),
            ('|', Some('|')) => Some("||"),
            ('(', _) => Some("("),
            (')', _) => Some(")"),
            ('<', _) => Some("<"),
            ('>', _) => Some(">"),
            _ => None,
        };
        if let Some(operator) = operator {
            self.index += operator.len();
            tokens.push(Token::Word(Word::literal(operator)));
            return Ok(());
        }
        let (word, end) = lex_word(chars, index, WordMode::Normal)?;
        if end == index {
            return Err(LexError::error("syntax error in conditional expression"));
        }
        self.index = end;
        match word.plain() {
            Some("]]") => {
                self.in_cond = false;
                self.command_start = false;
            }
            Some("=~") => self.next_is_regex = true,
            _ => {}
        }
        tokens.push(Token::Word(word));
        Ok(())
    }
}

/// Tokenizes `input`.
pub fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    let mut lexer = Lexer::new(input);
    let mut tokens = Vec::new();
    while !lexer.at_end() {
        lexer.next_line(&mut tokens)?;
    }
    Ok(tokens)
}

/// Returns the length of a digit run that forms a redirection's fd.
fn fd_prefix_length(chars: &[char], start: usize) -> Option<usize> {
    let mut end = start;
    while chars.get(end).is_some_and(char::is_ascii_digit) {
        end += 1;
    }
    match chars.get(end) {
        Some('<' | '>') if chars.get(end + 1) != Some(&'(') => Some(end - start),
        _ => None,
    }
}

fn parse_redirection(chars: &[char], start: usize) -> Result<(RedirOp, usize), LexError> {
    let mut index = start;
    let digits = fd_prefix_length(chars, start).unwrap_or(0);
    let explicit_fd = digits > 0;
    let fd = if explicit_fd {
        let mut value = 0u32;
        for ch in &chars[start..start + digits] {
            value = value.saturating_mul(10).saturating_add(ch.to_digit(10).unwrap_or(0));
        }
        if value > 9 {
            return Err(LexError::error("file descriptor out of range"));
        }
        index += digits;
        value as u8
    } else if chars.get(index) == Some(&'<') {
        0
    } else {
        1
    };

    match chars.get(index) {
        Some('&') if chars.get(index + 1) == Some(&'>') => {
            index += 2;
            if chars.get(index) == Some(&'>') {
                Ok((RedirOp::AppendBoth, index + 1))
            } else {
                Ok((RedirOp::WriteBoth, index))
            }
        }
        Some('<') => {
            index += 1;
            match chars.get(index) {
                Some('&') => parse_dup(chars, index + 1, fd, explicit_fd),
                Some('<') if chars.get(index + 1) == Some(&'<') => {
                    Ok((RedirOp::HereString(fd), index + 2))
                }
                Some('<') => Ok((RedirOp::HereDoc(fd), index + 1)),
                Some('>') => Ok((RedirOp::ReadWrite(fd), index + 1)),
                _ => Ok((RedirOp::Read(fd), index)),
            }
        }
        Some('>') => {
            index += 1;
            match chars.get(index) {
                Some('>') => Ok((RedirOp::Append(fd), index + 1)),
                Some('&') => parse_dup(chars, index + 1, fd, explicit_fd),
                Some('|') => Ok((RedirOp::Clobber(fd), index + 1)),
                _ => Ok((RedirOp::Write(fd), index)),
            }
        }
        _ => Err(LexError::error("expected a redirection")),
    }
}

/// Parses what follows `>&` or `<&`.
fn parse_dup(
    chars: &[char],
    index: usize,
    fd: u8,
    _explicit: bool,
) -> Result<(RedirOp, usize), LexError> {
    match chars.get(index) {
        Some('-') => Ok((RedirOp::Close(fd), index + 1)),
        Some(ch) if ch.is_ascii_digit() && chars.get(index + 1).is_none_or(|next| is_word_end(*next)) => {
            Ok((RedirOp::Dup(fd, *ch as u8 - b'0'), index + 1))
        }
        None => Err(LexError::incomplete("expected a file descriptor")),
        _ => Ok((RedirOp::DupWord(fd), index)),
    }
}

/// Finds `(( ... ))` when the parentheses really are arithmetic: the inner `(`
/// must close immediately before the outer one.
fn scan_arith_command(chars: &[char], open: usize) -> Option<(String, usize)> {
    let (inner, end) = scan_balanced(chars, open + 1).ok()?;
    if chars.get(end) == Some(&')') {
        Some((inner, end + 1))
    } else {
        None
    }
}

fn is_word_end(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordMode {
    /// Operators and blanks end the word.
    Normal,
    /// Only blanks end the word, for the right side of `=~`.
    Regex,
    /// Nothing ends the word, for the operand inside `${...}`.
    Whole,
}

fn lex_word(chars: &[char], start: usize, mode: WordMode) -> Result<(Word, usize), LexError> {
    let mut index = start;
    let mut segments: Vec<Segment> = Vec::new();
    let mut literal = String::new();
    let mut literal_quoted = false;
    let mut simple = true;

    while index < chars.len() {
        let ch = chars[index];
        let ends = match mode {
            WordMode::Normal => is_word_end(ch),
            WordMode::Regex => matches!(ch, ' ' | '\t' | '\r' | '\n'),
            WordMode::Whole => false,
        };
        if ends {
            break;
        }

        // A tilde prefix is only special alone or before a slash; `~foo` would
        // name a user, and there are no users to look up.
        if ch == '~'
            && index == start
            && mode == WordMode::Normal
            && chars
                .get(index + 1)
                .is_none_or(|next| *next == '/' || is_word_end(*next))
        {
            segments.push(Segment::Tilde);
            simple = false;
            index += 1;
            continue;
        }

        match ch {
            '\'' => {
                simple = false;
                flush(&mut segments, &mut literal, &mut literal_quoted);
                index += 1;
                let mut text = String::new();
                loop {
                    let Some(&next) = chars.get(index) else {
                        return Err(LexError::incomplete("unterminated single quote"));
                    };
                    index += 1;
                    if next == '\'' {
                        break;
                    }
                    text.push(next);
                }
                push_segment(&mut segments, Segment::Literal(text, true));
            }
            '"' => {
                simple = false;
                flush(&mut segments, &mut literal, &mut literal_quoted);
                index = lex_double_quote(chars, index + 1, &mut segments)?;
            }
            '\\' => {
                simple = false;
                flush(&mut segments, &mut literal, &mut literal_quoted);
                let Some(&escaped) = chars.get(index + 1) else {
                    return Err(LexError::incomplete("trailing backslash"));
                };
                if escaped != '\n' {
                    let mut text = String::new();
                    text.push(escaped);
                    push_segment(&mut segments, Segment::Literal(text, true));
                }
                index += 2;
            }
            '`' => {
                simple = false;
                flush(&mut segments, &mut literal, &mut literal_quoted);
                let (script, end) = scan_backticks(chars, index + 1)?;
                push_segment(&mut segments, Segment::CommandSub(script, false));
                index = end;
            }
            '$' if chars.get(index + 1) == Some(&'\'') => {
                simple = false;
                flush(&mut segments, &mut literal, &mut literal_quoted);
                let (text, end) = scan_ansi_c(chars, index + 2)?;
                push_segment(&mut segments, Segment::Literal(text, true));
                index = end;
            }
            '$' if chars.get(index + 1) == Some(&'"') => {
                simple = false;
                flush(&mut segments, &mut literal, &mut literal_quoted);
                index = lex_double_quote(chars, index + 2, &mut segments)?;
            }
            '$' => {
                simple = false;
                flush(&mut segments, &mut literal, &mut literal_quoted);
                index = parse_dollar(chars, index + 1, &mut segments, false)?;
            }
            other => {
                literal.push(other);
                index += 1;
            }
        }
    }

    flush(&mut segments, &mut literal, &mut literal_quoted);
    if segments.is_empty() {
        segments.push(Segment::Literal(String::new(), false));
    }
    Ok((Word { segments, simple }, index))
}

/// Lexes the inside of `"..."`, starting just past the opening quote.
fn lex_double_quote(
    chars: &[char],
    start: usize,
    segments: &mut Vec<Segment>,
) -> Result<usize, LexError> {
    let mut index = start;
    let mut text = String::new();
    let before = segments.len();
    loop {
        let Some(&next) = chars.get(index) else {
            return Err(LexError::incomplete("unterminated double quote"));
        };
        index += 1;
        match next {
            '"' => break,
            '\\' => {
                let Some(&escaped) = chars.get(index) else {
                    return Err(LexError::incomplete("unterminated escape"));
                };
                index += 1;
                match escaped {
                    '$' | '`' | '"' | '\\' => text.push(escaped),
                    '\n' => {}
                    other => {
                        text.push('\\');
                        text.push(other);
                    }
                }
            }
            '$' => {
                push_literal(segments, &mut text, true);
                index = parse_dollar(chars, index, segments, true)?;
            }
            '`' => {
                push_literal(segments, &mut text, true);
                let (script, end) = scan_backticks(chars, index)?;
                push_segment(segments, Segment::CommandSub(script, true));
                index = end;
            }
            other => text.push(other),
        }
    }
    // An empty pair of quotes is an empty word, but `"$@"` must stay able to
    // expand to no words at all.
    if !text.is_empty() || segments.len() == before {
        push_segment(segments, Segment::Literal(text, true));
    }
    Ok(index)
}

/// Lexes a here-document body: `$`, backticks and `\` work as in double quotes,
/// except that `\"` stays literal.
fn lex_heredoc_body(body: &str) -> Result<Word, LexError> {
    let chars: Vec<char> = body.chars().collect();
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        index += 1;
        match ch {
            '\\' => match chars.get(index) {
                Some(&escaped @ ('$' | '`' | '\\')) => {
                    text.push(escaped);
                    index += 1;
                }
                Some('\n') => index += 1,
                _ => text.push('\\'),
            },
            '$' => {
                push_literal(&mut segments, &mut text, true);
                index = parse_dollar(&chars, index, &mut segments, true)?;
            }
            '`' => {
                push_literal(&mut segments, &mut text, true);
                let (script, end) = scan_backticks(&chars, index)?;
                push_segment(&mut segments, Segment::CommandSub(script, true));
                index = end;
            }
            other => text.push(other),
        }
    }
    push_literal(&mut segments, &mut text, true);
    if segments.is_empty() {
        segments.push(Segment::Literal(String::new(), true));
    }
    Ok(Word {
        segments,
        simple: false,
    })
}

/// Lexes the operand text of `${name:-text}` into a word. `quoted` is true when
/// the whole expansion sits inside double quotes.
pub(crate) fn lex_operand(text: &str, quoted: bool) -> Result<Word, LexError> {
    let chars: Vec<char> = text.chars().collect();
    if !quoted {
        let (word, _) = lex_word(&chars, 0, WordMode::Whole)?;
        return Ok(word);
    }
    // Inside double quotes, only the double-quote rules apply, and a `"` just
    // toggles nothing because the operand was already found.
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        index += 1;
        match ch {
            '\\' => match chars.get(index) {
                Some(&escaped @ ('$' | '`' | '"' | '\\' | '}')) => {
                    literal.push(escaped);
                    index += 1;
                }
                Some('\n') => index += 1,
                _ => literal.push('\\'),
            },
            '"' => {
                // A nested `"..."` inside the operand groups without changing
                // the quoting.
                push_literal(&mut segments, &mut literal, true);
                index = lex_double_quote(&chars, index, &mut segments)?;
            }
            '$' => {
                push_literal(&mut segments, &mut literal, true);
                index = parse_dollar(&chars, index, &mut segments, true)?;
            }
            '`' => {
                push_literal(&mut segments, &mut literal, true);
                let (script, end) = scan_backticks(&chars, index)?;
                push_segment(&mut segments, Segment::CommandSub(script, true));
                index = end;
            }
            other => literal.push(other),
        }
    }
    if !literal.is_empty() || segments.is_empty() {
        push_literal(&mut segments, &mut literal, true);
        if segments.is_empty() {
            segments.push(Segment::Literal(String::new(), true));
        }
    }
    Ok(Word {
        segments,
        simple: false,
    })
}

fn flush(segments: &mut Vec<Segment>, literal: &mut String, quoted: &mut bool) {
    if literal.is_empty() {
        *quoted = false;
        return;
    }
    push_segment(
        segments,
        Segment::Literal(core::mem::take(literal), *quoted),
    );
    *quoted = false;
}

fn push_literal(segments: &mut Vec<Segment>, text: &mut String, quoted: bool) {
    if text.is_empty() {
        return;
    }
    push_segment(segments, Segment::Literal(core::mem::take(text), quoted));
}

fn push_segment(segments: &mut Vec<Segment>, segment: Segment) {
    if let Segment::Literal(text, quoted) = &segment {
        if let Some(Segment::Literal(previous, previous_quoted)) = segments.last_mut() {
            if *previous_quoted == *quoted {
                previous.push_str(text);
                return;
            }
        }
    }
    segments.push(segment);
}

/// Parses the part after `$`. `index` points just past the dollar sign.
fn parse_dollar(
    chars: &[char],
    index: usize,
    segments: &mut Vec<Segment>,
    quoted: bool,
) -> Result<usize, LexError> {
    let Some(&ch) = chars.get(index) else {
        push_segment(segments, Segment::Literal(String::from("$"), quoted));
        return Ok(index);
    };

    match ch {
        '{' => {
            let (text, end) = scan_param(chars, index + 1, quoted)?;
            push_segment(segments, Segment::Param(text, quoted));
            Ok(end)
        }
        '(' => {
            if chars.get(index + 1) == Some(&'(') {
                if let Some((inner, end)) = scan_arith_command(chars, index) {
                    push_segment(segments, Segment::Arith(inner, quoted));
                    return Ok(end);
                }
            }
            let (inner, end) = scan_balanced(chars, index)?;
            push_segment(segments, Segment::CommandSub(inner, quoted));
            Ok(end)
        }
        '?' | '$' | '#' | '@' | '*' | '!' | '-' | '0'..='9' => {
            push_segment(segments, Segment::Special(ch, quoted));
            Ok(index + 1)
        }
        _ if ch == '_' || ch.is_ascii_alphabetic() => {
            let mut end = index;
            while end < chars.len() && (chars[end] == '_' || chars[end].is_ascii_alphanumeric()) {
                end += 1;
            }
            let name: String = chars[index..end].iter().collect();
            push_segment(segments, Segment::Var(name, quoted));
            Ok(end)
        }
        _ => {
            push_segment(segments, Segment::Literal(String::from("$"), quoted));
            Ok(index)
        }
    }
}

/// Scans `${...}` from just past the brace. Quotes and nested expansions may
/// hold a `}` that does not close it.
fn scan_param(chars: &[char], start: usize, in_double: bool) -> Result<(String, usize), LexError> {
    const UNTERMINATED: &str = "unterminated parameter expansion";
    let mut index = start;
    let mut text = String::new();
    let mut depth = 1usize;
    let mut single = false;
    let mut double = false;

    while let Some(&ch) = chars.get(index) {
        if single {
            text.push(ch);
            single = ch != '\'';
            index += 1;
            continue;
        }
        match ch {
            '\\' => {
                text.push(ch);
                index += 1;
                if let Some(&next) = chars.get(index) {
                    text.push(next);
                    index += 1;
                }
                continue;
            }
            '\'' if !double && !in_double => single = true,
            '"' => double = !double,
            '$' if chars.get(index + 1) == Some(&'{') => {
                depth += 1;
                text.push_str("${");
                index += 2;
                continue;
            }
            '$' if chars.get(index + 1) == Some(&'(') => {
                let (inner, end) = scan_balanced(chars, index + 1)?;
                text.push_str("$(");
                text.push_str(&inner);
                text.push(')');
                index = end;
                continue;
            }
            '`' => {
                let (_, end) = scan_backticks(chars, index + 1)?;
                text.extend(&chars[index..end]);
                index = end;
                continue;
            }
            '}' if !double => {
                depth -= 1;
                if depth == 0 {
                    return Ok((text, index + 1));
                }
            }
            _ => {}
        }
        text.push(ch);
        index += 1;
    }
    Err(LexError::incomplete(UNTERMINATED))
}

/// Scans a backquoted command from just past the opening quote, removing the
/// backslashes that protect `\`, `$` and `` ` ``.
fn scan_backticks(chars: &[char], start: usize) -> Result<(String, usize), LexError> {
    let mut index = start;
    let mut text = String::new();
    while let Some(&ch) = chars.get(index) {
        index += 1;
        match ch {
            '`' => return Ok((text, index)),
            '\\' => match chars.get(index) {
                Some(&next @ ('\\' | '$' | '`')) => {
                    text.push(next);
                    index += 1;
                }
                _ => text.push('\\'),
            },
            other => text.push(other),
        }
    }
    Err(LexError::incomplete("unterminated command substitution"))
}

/// Decodes `$'...'` from just past the opening quote.
fn scan_ansi_c(chars: &[char], start: usize) -> Result<(String, usize), LexError> {
    let mut index = start;
    let mut bytes: Vec<u8> = Vec::new();
    let push_char = |bytes: &mut Vec<u8>, ch: char| {
        let mut buffer = [0u8; 4];
        bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
    };
    loop {
        let Some(&ch) = chars.get(index) else {
            return Err(LexError::incomplete("unterminated single quote"));
        };
        index += 1;
        if ch == '\'' {
            break;
        }
        if ch != '\\' {
            push_char(&mut bytes, ch);
            continue;
        }
        let Some(&escape) = chars.get(index) else {
            return Err(LexError::incomplete("unterminated escape"));
        };
        index += 1;
        let number = |index: &mut usize, radix: u32, max_digits: usize| -> u32 {
            let mut value = 0u32;
            let mut digits = 0usize;
            while digits < max_digits {
                let Some(digit) = chars.get(*index).and_then(|ch| ch.to_digit(radix)) else {
                    break;
                };
                value = value * radix + digit;
                *index += 1;
                digits += 1;
            }
            value
        };
        match escape {
            'a' => bytes.push(0x07),
            'b' => bytes.push(0x08),
            'e' | 'E' => bytes.push(0x1b),
            'f' => bytes.push(0x0c),
            'n' => bytes.push(b'\n'),
            'r' => bytes.push(b'\r'),
            't' => bytes.push(b'\t'),
            'v' => bytes.push(0x0b),
            '\\' | '\'' | '"' | '?' => bytes.push(escape as u8),
            '0'..='7' => {
                index -= 1;
                bytes.push((number(&mut index, 8, 3) & 0xff) as u8);
            }
            'x' => bytes.push((number(&mut index, 16, 2) & 0xff) as u8),
            'u' => push_char(
                &mut bytes,
                char::from_u32(number(&mut index, 16, 4)).unwrap_or('\u{fffd}'),
            ),
            'U' => push_char(
                &mut bytes,
                char::from_u32(number(&mut index, 16, 8)).unwrap_or('\u{fffd}'),
            ),
            'c' => {
                if let Some(&control) = chars.get(index) {
                    index += 1;
                    bytes.push((control as u32 & 0x1f) as u8);
                }
            }
            other => {
                bytes.push(b'\\');
                push_char(&mut bytes, other);
            }
        }
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), index))
}

/// Scans a balanced `(...)` starting at `open` (which must be `(`). Returns
/// the text inside and the index just past the closing `)`.
///
/// `case` patterns close with a lone `)`, so `case`/`esac` words are tracked to
/// keep them from ending the substitution early.
fn scan_balanced(chars: &[char], open: usize) -> Result<(String, usize), LexError> {
    let mut index = open + 1;
    let mut depth = 1usize;
    let mut case_depth = 0usize;
    let mut word_start = true;
    let mut text = String::new();
    let mut single = false;
    let mut double = false;

    while index < chars.len() {
        let ch = chars[index];
        if single {
            text.push(ch);
            if ch == '\'' {
                single = false;
            }
            index += 1;
            continue;
        }
        if double {
            if ch == '\\' {
                text.push(ch);
                index += 1;
                if let Some(&next) = chars.get(index) {
                    text.push(next);
                    index += 1;
                }
                continue;
            }
            if ch == '"' {
                double = false;
            }
            text.push(ch);
            index += 1;
            continue;
        }

        if word_start && ch.is_ascii_alphabetic() {
            let mut end = index;
            while chars.get(end).is_some_and(|next| !is_word_end(*next)) {
                end += 1;
            }
            let word: String = chars[index..end].iter().collect();
            match word.as_str() {
                "case" => case_depth += 1,
                "esac" => case_depth = case_depth.saturating_sub(1),
                _ => {}
            }
            text.push_str(&word);
            index = end;
            word_start = false;
            continue;
        }

        match ch {
            '\'' => {
                single = true;
                text.push(ch);
            }
            '"' => {
                double = true;
                text.push(ch);
            }
            '\\' => {
                text.push(ch);
                index += 1;
                if let Some(&next) = chars.get(index) {
                    text.push(next);
                }
            }
            '#' if word_start => {
                while chars.get(index).is_some_and(|next| *next != '\n') {
                    text.push(chars[index]);
                    index += 1;
                }
                continue;
            }
            '(' => {
                depth += 1;
                text.push(ch);
            }
            ')' => {
                if depth == 1 && case_depth > 0 {
                    text.push(ch);
                } else {
                    depth -= 1;
                    if depth == 0 {
                        return Ok((text, index + 1));
                    }
                    text.push(ch);
                }
            }
            _ => text.push(ch),
        }
        word_start = is_word_end(ch);
        index += 1;
    }

    Err(LexError::incomplete("unterminated command substitution"))
}

/// Returns `true` when the text ends inside a quote, a substitution or a
/// compound command, so an interactive prompt should read another line.
pub fn needs_more(text: &str) -> bool {
    crate::parser::check_complete(text).is_err_and(|incomplete| incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(tokens: &[Token]) -> Vec<String> {
        tokens
            .iter()
            .filter_map(|token| match token {
                Token::Word(word) => Some(word.raw()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn splits_words_and_operators() {
        let tokens = lex("echo hi | grep h && ls; pwd\n").unwrap();
        assert_eq!(words(&tokens), ["echo", "hi", "grep", "h", "ls", "pwd"]);
        assert!(tokens.contains(&Token::Pipe));
        assert!(tokens.contains(&Token::And));
        assert!(tokens.contains(&Token::Seq));
        assert!(tokens.contains(&Token::Newline));
    }

    #[test]
    fn keeps_quotes_and_escapes() {
        let tokens = lex(r#"echo "a b" 'c d' e\ f"#).unwrap();
        assert_eq!(words(&tokens), ["echo", "a b", "c d", "e f"]);
    }

    #[test]
    fn finds_parameters_and_substitutions() {
        let tokens = lex("echo $HOME ${USER} $(pwd) $((1 + 2)) $?").unwrap();
        let words = words(&tokens);
        assert_eq!(words.len(), 6);
        assert!(
            matches!(tokens[1], Token::Word(ref word) if matches!(word.segments[0], Segment::Var(_, _)))
        );
    }

    #[test]
    fn recognises_redirections() {
        let tokens = lex("cat < in > out 2>> err 2>&1 &> both").unwrap();
        let redirs: Vec<_> = tokens
            .iter()
            .filter_map(|token| match token {
                Token::Redir(op) => Some(*op),
                _ => None,
            })
            .collect();
        assert_eq!(
            redirs,
            [
                RedirOp::Read(0),
                RedirOp::Write(1),
                RedirOp::Append(2),
                RedirOp::Dup(2, 1),
                RedirOp::WriteBoth
            ]
        );
    }

    #[test]
    fn reports_incomplete_constructs() {
        assert!(lex("echo 'unterminated").unwrap_err().incomplete);
        assert!(lex("echo \"unterminated").unwrap_err().incomplete);
        assert!(lex("echo $(pwd").unwrap_err().incomplete);
        assert!(lex("echo done").is_ok());
        assert!(!needs_more("echo done"));
        assert!(needs_more("echo 'unterminated"));
    }

    #[test]
    fn reads_here_documents_at_the_end_of_the_line() {
        let tokens = lex("cat <<EOF | wc\nline $x\nEOF\necho next\n").unwrap();
        let Token::Word(body) = &tokens[2] else {
            panic!("expected the body, got {tokens:?}");
        };
        assert_eq!(
            body.segments,
            [
                Segment::Literal("line ".into(), true),
                Segment::Var("x".into(), true),
                Segment::Literal("\n".into(), true),
            ]
        );
        assert!(tokens.contains(&Token::Pipe));
        assert!(words(&tokens).contains(&String::from("next")));
    }

    #[test]
    fn quoted_delimiters_keep_here_documents_literal() {
        let tokens = lex("cat <<'E'\n$x\nE\n").unwrap();
        let Token::Word(body) = &tokens[2] else {
            panic!("expected the body");
        };
        assert_eq!(body.segments, [Segment::Literal("$x\n".into(), true)]);
        let tokens = lex("cat <<-E\n\tindented\n\tE\n").unwrap();
        let Token::Word(body) = &tokens[2] else {
            panic!("expected the body");
        };
        assert_eq!(body.segments, [Segment::Literal("indented\n".into(), true)]);
        assert!(lex("cat <<E\nnever closed\n").unwrap_err().incomplete);
    }

    #[test]
    fn decodes_ansi_c_quotes_and_backticks() {
        let tokens = lex("echo $'a\\tb\\x41\\303\\251' `pwd`").unwrap();
        let Token::Word(word) = &tokens[1] else {
            panic!();
        };
        assert_eq!(word.segments, [Segment::Literal("a\tbA\u{e9}".into(), true)]);
        assert!(matches!(tokens[2], Token::Word(ref word)
            if word.segments == [Segment::CommandSub("pwd".into(), false)]));
    }

    #[test]
    fn finds_arithmetic_commands_only_in_command_position() {
        assert_eq!(lex("((a < b))").unwrap(), [Token::Arith("a < b".into())]);
        assert_eq!(lex("((a);(b))").unwrap()[0], Token::LParen);
        assert!(matches!(lex("for ((i=0;i<2;i++))").unwrap()[1], Token::ArithFor(_)));
        let tokens = lex("echo $(((1+2)*(3)))").unwrap();
        let Token::Word(word) = &tokens[1] else {
            panic!();
        };
        assert_eq!(word.segments, [Segment::Arith("(1+2)*(3)".into(), false)]);
    }

    #[test]
    fn conditional_operators_become_words() {
        let tokens = lex("[[ a < b && (c) ]]").unwrap();
        assert_eq!(words(&tokens), ["[[", "a", "<", "b", "&&", "(", "c", ")", "]]"]);
        let tokens = lex("[[ a =~ (x|y)+ ]]; echo").unwrap();
        assert_eq!(words(&tokens)[3], "(x|y)+");
        assert!(tokens.contains(&Token::Seq));
    }

    #[test]
    fn substitutions_tolerate_case_and_comments() {
        let tokens = lex("x=$(case a in a) echo y;; esac) z").unwrap();
        assert_eq!(tokens.len(), 2);
        let tokens = lex("x=$(echo a # don't)\necho b\n)").unwrap();
        assert_eq!(tokens.len(), 1);
    }

    #[test]
    fn tilde_needs_a_slash_or_word_end() {
        let word = |text: &str| match &lex(text).unwrap()[0] {
            Token::Word(word) => word.segments.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(word("~")[0], Segment::Tilde);
        assert_eq!(word("~/x")[0], Segment::Tilde);
        assert_eq!(word("~foo"), [Segment::Literal("~foo".into(), false)]);
    }

    #[test]
    fn parses_extended_redirections() {
        let ops = |text: &str| -> Vec<RedirOp> {
            lex(text)
                .unwrap()
                .iter()
                .filter_map(|token| match token {
                    Token::Redir(op) => Some(*op),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(ops("a >&2"), [RedirOp::Dup(1, 2)]);
        assert_eq!(ops("a 2>&-"), [RedirOp::Close(2)]);
        assert_eq!(ops("a <<<x"), [RedirOp::HereString(0)]);
        assert_eq!(ops("a <>f >|g"), [RedirOp::ReadWrite(0), RedirOp::Clobber(1)]);
        assert_eq!(ops("a 2>&$fd"), [RedirOp::DupWord(2)]);
        assert!(lex("a 12>f").is_err());
    }
}
