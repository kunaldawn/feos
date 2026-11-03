//! Shell tokenizer.
//!
//! The lexer understands the parts of Bash syntax the executor supports:
//! quoting, escapes, parameter and command substitution, redirections,
//! pipelines and list operators. It is deliberately byte/`char` oriented and
//! keeps the original quoting information so expansion can perform word
//! splitting and pathname generation correctly.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A fragment of a word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Literal text; the flag says whether it was quoted.
    Literal(String, bool),
    /// `$NAME`.
    Var(String, bool),
    /// `$?`, `$$`, `$#`, `$0` … `$9`.
    Special(char, bool),
    /// `${...}` with its inner text.
    Param(String, bool),
    /// `$(command)`.
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

    /// Splits `NAME=value` at the first literal `=` when `NAME` is valid.
    pub fn split_assignment(&self) -> Option<(String, Word)> {
        let (first, rest) = self.segments.split_first()?;
        let Segment::Literal(text, false) = first else {
            return None;
        };
        let index = text.find('=')?;
        let name = &text[..index];
        if !valid_name(name) {
            return None;
        }
        let value = &text[index + 1..];
        let mut segments = Vec::new();
        if !value.is_empty() {
            segments.push(Segment::Literal(value.into(), false));
        }
        segments.extend_from_slice(rest);
        Some((
            name.into(),
            Word {
                segments,
                simple: value.is_empty() && rest.is_empty(),
            },
        ))
    }
}

fn valid_name(name: &str) -> bool {
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
    /// `>>file`.
    Append(u8),
    /// `&>file`: stdout and stderr.
    WriteBoth,
    /// `&>>file`.
    AppendBoth,
    /// `n>&m`.
    Dup(u8, u8),
}

/// One lexical token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// A word.
    Word(Word),
    /// `;`
    Seq,
    /// Newline.
    Newline,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `|`
    Pipe,
    /// `&`
    Amp,
    /// `<`, `>`, `>>`, `2>` …
    Redir(RedirOp),
    /// `(`
    LParen,
    /// `)`
    RParen,
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

/// Tokenizes `input`.
pub fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0usize;
    let mut tokens = Vec::new();

    while index < chars.len() {
        let ch = chars[index];
        match ch {
            ' ' | '\t' | '\r' => index += 1,
            '\n' => {
                tokens.push(Token::Newline);
                index += 1;
            }
            '#' => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            ';' => {
                tokens.push(Token::Seq);
                index += 1;
            }
            '&' if chars.get(index + 1) == Some(&'&') => {
                tokens.push(Token::And);
                index += 2;
            }
            '&' if chars.get(index + 1) == Some(&'>') => {
                let (op, next) = parse_redirection(&chars, index)?;
                tokens.push(Token::Redir(op));
                index = next;
            }
            '&' => {
                tokens.push(Token::Amp);
                index += 1;
            }
            '|' if chars.get(index + 1) == Some(&'|') => {
                tokens.push(Token::Or);
                index += 2;
            }
            '|' => {
                tokens.push(Token::Pipe);
                index += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                index += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                index += 1;
            }
            '<' | '>' => {
                let (op, next) = parse_redirection(&chars, index)?;
                tokens.push(Token::Redir(op));
                index = next;
            }
            '0'..='9'
                if matches!(chars.get(index + 1), Some('>' | '<'))
                    && (chars.get(index + 2) != Some(&'(')) =>
            {
                let (op, next) = parse_redirection(&chars, index)?;
                tokens.push(Token::Redir(op));
                index = next;
            }
            _ => {
                let (word, next) = lex_word(&chars, index)?;
                tokens.push(Token::Word(word));
                index = next;
            }
        }
    }

    Ok(tokens)
}

fn parse_redirection(chars: &[char], start: usize) -> Result<(RedirOp, usize), LexError> {
    let mut index = start;
    let fd = if chars[index].is_ascii_digit() {
        let fd = chars[index] as u8 - b'0';
        index += 1;
        fd
    } else if chars[index] == '<' {
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
            if chars.get(index) == Some(&'&') {
                index += 1;
                let target = parse_fd(chars, index)?;
                Ok((RedirOp::Dup(0, target), index + 1))
            } else {
                Ok((RedirOp::Read(fd), index))
            }
        }
        Some('>') => {
            index += 1;
            if chars.get(index) == Some(&'>') {
                Ok((RedirOp::Append(fd), index + 1))
            } else if chars.get(index) == Some(&'&') {
                index += 1;
                let target = parse_fd(chars, index)?;
                Ok((RedirOp::Dup(fd, target), index + 1))
            } else if chars.get(index) == Some(&'|') {
                Ok((RedirOp::Write(fd), index + 1))
            } else {
                Ok((RedirOp::Write(fd), index))
            }
        }
        _ => Err(LexError::error("expected a redirection")),
    }
}

fn parse_fd(chars: &[char], index: usize) -> Result<u8, LexError> {
    match chars.get(index) {
        Some(ch) if ch.is_ascii_digit() => Ok(*ch as u8 - b'0'),
        _ => Err(LexError::error("expected a file descriptor")),
    }
}

fn is_word_end(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
    )
}

fn lex_word(chars: &[char], start: usize) -> Result<(Word, usize), LexError> {
    let mut index = start;
    let mut segments: Vec<Segment> = Vec::new();
    let mut literal = String::new();
    let mut literal_quoted = false;
    let mut simple = true;
    let mut at_start = true;

    while index < chars.len() && !is_word_end(chars[index]) {
        let ch = chars[index];

        if ch == '~' && at_start {
            flush(&mut segments, &mut literal, &mut literal_quoted);
            segments.push(Segment::Tilde);
            simple = false;
            index += 1;
            at_start = false;
            continue;
        }
        at_start = false;

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
                index += 1;
                let mut text = String::new();
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
                            push_literal(&mut segments, &mut text, true);
                            index = parse_dollar(chars, index, &mut segments, true)?;
                        }
                        other => text.push(other),
                    }
                }
                push_segment(&mut segments, Segment::Literal(text, true));
            }
            '\\' => {
                simple = false;
                flush(&mut segments, &mut literal, &mut literal_quoted);
                let Some(&escaped) = chars.get(index + 1) else {
                    return Err(LexError::incomplete("trailing backslash"));
                };
                if escaped == '\n' {
                    index += 2;
                } else {
                    let mut text = String::new();
                    text.push(escaped);
                    push_segment(&mut segments, Segment::Literal(text, true));
                    index += 2;
                }
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
            let mut end = index + 1;
            let mut depth = 1usize;
            let mut text = String::new();
            while end < chars.len() {
                let current = chars[end];
                if current == '}' && depth == 1 {
                    end += 1;
                    break;
                }
                if current == '{' {
                    depth += 1;
                } else if current == '}' {
                    depth -= 1;
                }
                text.push(current);
                end += 1;
            }
            if end > chars.len() || chars.get(end - 1) != Some(&'}') {
                return Err(LexError::incomplete("unterminated parameter expansion"));
            }
            push_segment(segments, Segment::Param(text, quoted));
            Ok(end)
        }
        '(' => {
            if chars.get(index + 1) == Some(&'(') {
                let (inner, end) = scan_balanced(chars, index)?;
                let inner = inner
                    .strip_prefix('(')
                    .and_then(|text| text.strip_suffix(')'))
                    .unwrap_or(&inner)
                    .to_string();
                push_segment(segments, Segment::Arith(inner, quoted));
                Ok(end)
            } else {
                let (inner, end) = scan_balanced(chars, index)?;
                push_segment(segments, Segment::CommandSub(inner, quoted));
                Ok(end)
            }
        }
        '?' | '$' | '#' | '0'..='9' => {
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

/// Scans a balanced `(...)` starting at `open` (which must be `(`). Returns
/// the text inside and the index just past the closing `)`.
fn scan_balanced(chars: &[char], open: usize) -> Result<(String, usize), LexError> {
    let mut index = open + 1;
    let mut depth = 1usize;
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
            '(' => {
                depth += 1;
                text.push(ch);
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((text, index + 1));
                }
                text.push(ch);
            }
            _ => text.push(ch),
        }
        index += 1;
    }

    Err(LexError::incomplete("unterminated command substitution"))
}

/// Returns `true` when the text ends inside a quote or substitution.
pub fn needs_more(text: &str) -> bool {
    match lex(text) {
        Ok(_) => false,
        Err(error) => error.incomplete,
    }
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
}
