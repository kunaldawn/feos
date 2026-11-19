//! `#!` scripts (section 7.4 of `docs/feos-abi-v2.md`): the first line of a file that
//! starts `#!` names the program that runs it, and at most one argument for it.
//!
//! Like the rest of this crate, the input is whatever someone put in a file: anything
//! that isn't a line this can use is a [`ShebangError`], never a panic.

/// Most bytes of a `#!` line after the `#!`, not counting its newline. A longer line is
/// [`ShebangError::TooLong`], however the file goes on.
pub const SHEBANG_MAX: usize = 255;

/// Bytes of a file to read to judge its first line: the `#!`, the longest line, and its
/// newline.
pub const SHEBANG_BYTES: usize = 2 + SHEBANG_MAX + 1;

/// What a script's first line asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shebang<'a> {
    /// The interpreter's absolute path.
    pub interpreter: &'a str,
    /// What follows it on the line, past the spaces and tabs between, without the ones
    /// at the end: one argument however many words it has, as on Linux. `None` if
    /// nothing follows.
    pub argument: Option<&'a [u8]>,
}

/// Why a `#!` line can't be used. Each is `ENOEXEC` to a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShebangError {
    /// No newline within [`SHEBANG_MAX`] bytes of the `#!`.
    TooLong,
    /// Nothing but spaces and tabs after the `#!`.
    NoInterpreter,
    /// The interpreter isn't an absolute path.
    NotAbsolute,
    /// A NUL in the line, or an interpreter path that isn't UTF-8 (no path is).
    BadBytes,
}

impl ShebangError {
    /// What went wrong, for a person.
    pub const fn text(self) -> &'static str {
        match self {
            Self::TooLong => "its #! line is too long",
            Self::NoInterpreter => "its #! line names no interpreter",
            Self::NotAbsolute => "its #! interpreter isn't an absolute path",
            Self::BadBytes => "its #! line has bytes no path can",
        }
    }
}

const fn is_blank(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// Reads the `#!` line at the start of `head`, which holds the file's first bytes (at
/// least [`SHEBANG_BYTES`] of them, or the whole file if it's shorter). `whole` says
/// `head` is the whole file, whose end then ends the line as a newline would. `None` if
/// the file doesn't start with `#!`, so isn't a script.
pub fn shebang(head: &[u8], whole: bool) -> Option<Result<Shebang<'_>, ShebangError>> {
    let rest = head.strip_prefix(b"#!")?;
    Some(parse_line(rest, whole))
}

fn parse_line(rest: &[u8], whole: bool) -> Result<Shebang<'_>, ShebangError> {
    let window = rest.get(..rest.len().min(SHEBANG_MAX + 1)).unwrap_or(rest);
    let line = match window.iter().position(|&byte| byte == b'\n') {
        Some(end) => window.get(..end).unwrap_or(window),
        None if whole && rest.len() <= SHEBANG_MAX => rest,
        None => return Err(ShebangError::TooLong),
    };
    if line.contains(&0) {
        return Err(ShebangError::BadBytes);
    }
    let start = line.iter().position(|&byte| !is_blank(byte)).ok_or(ShebangError::NoInterpreter)?;
    // `start` is a non-blank byte's, so there's at least one, and an end past it.
    let end = line.iter().rposition(|&byte| !is_blank(byte)).ok_or(ShebangError::NoInterpreter)?;
    let text = line.get(start..=end).ok_or(ShebangError::NoInterpreter)?;
    let (interpreter, argument) = match text.iter().position(|&byte| is_blank(byte)) {
        Some(split) => {
            let (interpreter, after) = text.split_at(split);
            let skip = after.iter().position(|&byte| !is_blank(byte)).unwrap_or(after.len());
            (interpreter, after.get(skip..).filter(|argument| !argument.is_empty()))
        }
        None => (text, None),
    };
    let interpreter = core::str::from_utf8(interpreter).map_err(|_| ShebangError::BadBytes)?;
    if !interpreter.starts_with('/') {
        return Err(ShebangError::NotAbsolute);
    }
    Ok(Shebang { interpreter, argument })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    fn parse(text: &[u8]) -> Result<Shebang<'_>, ShebangError> {
        shebang(text, false).expect("a #! file")
    }

    #[test]
    fn shebang_interpreter_alone() {
        assert_eq!(parse(b"#!/bin/sh\necho hi\n"), Ok(Shebang { interpreter: "/bin/sh", argument: None }));
        // Spaces and tabs around it go, and a carriage return is part of the name.
        assert_eq!(parse(b"#! \t/bin/sh \t\n"), Ok(Shebang { interpreter: "/bin/sh", argument: None }));
        assert_eq!(parse(b"#!/bin/sh\r\n"), Ok(Shebang { interpreter: "/bin/sh\r", argument: None }));
    }

    #[test]
    fn shebang_one_argument_however_many_words() {
        let got = parse(b"#!/usr/bin/env python3\n").unwrap();
        assert_eq!((got.interpreter, got.argument), ("/usr/bin/env", Some(b"python3".as_slice())));
        let got = parse(b"#!/bin/ptest  --script two\twords \t\n").unwrap();
        assert_eq!(got.argument, Some(b"--script two\twords".as_slice()));
        // Arguments needn't be UTF-8; paths must.
        assert_eq!(parse(b"#!/bin/x \xff\n").unwrap().argument, Some(b"\xff".as_slice()));
        assert_eq!(parse(b"#!/bin/\xff\n"), Err(ShebangError::BadBytes));
    }

    #[test]
    fn shebang_refusals() {
        assert_eq!(shebang(b"\x7fELF", false), None);
        assert_eq!(shebang(b"# !/bin/sh\n", false), None);
        assert_eq!(parse(b"#!\n"), Err(ShebangError::NoInterpreter));
        assert_eq!(parse(b"#!  \t \n"), Err(ShebangError::NoInterpreter));
        assert_eq!(parse(b"#!sh\n"), Err(ShebangError::NotAbsolute));
        assert_eq!(parse(b"#!/bin/sh\0x\n"), Err(ShebangError::BadBytes));
        // No newline: only the file's end ends the line, and only in a short file.
        assert_eq!(parse(b"#!/bin/sh"), Err(ShebangError::TooLong));
        assert_eq!(shebang(b"#!/bin/sh -e", true), Some(Ok(Shebang { interpreter: "/bin/sh", argument: Some(b"-e".as_slice()) })));
    }

    #[test]
    fn shebang_length_limit() {
        let mut line = b"#!/".to_vec();
        line.resize(2 + SHEBANG_MAX, b'a');
        line.push(b'\n');
        assert_eq!(parse(&line).unwrap().interpreter.len(), SHEBANG_MAX);
        // A byte more, and the newline is past the limit.
        line.insert(3, b'a');
        assert_eq!(parse(&line), Err(ShebangError::TooLong));
        assert_eq!(shebang(&line, true), Some(Err(ShebangError::TooLong)));
        let mut whole = b"#!/".to_vec();
        whole.resize(2 + SHEBANG_MAX, b'b');
        assert!(shebang(&whole, true).unwrap().is_ok());
        whole.push(b'b');
        assert_eq!(shebang(&whole, true), Some(Err(ShebangError::TooLong)));
    }

    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Random first lines, of bytes that matter here more often than not: what's
    /// accepted is always a line that fits, an absolute UTF-8 interpreter with no blanks
    /// in it, and an argument with none at either end.
    #[test]
    fn shebang_fuzz_random_first_lines() {
        let alphabet = b" \t\n\0/ab\xff-";
        let mut state = 0x5eed_0f_5c41_7e57u64;
        for round in 0..200_000 {
            let length = (next(&mut state) % 300) as usize;
            let mut file = b"#!".to_vec();
            for _ in 0..length {
                let pick = next(&mut state);
                file.push(if pick % 4 == 0 { pick as u8 } else { alphabet[(pick >> 8) as usize % alphabet.len()] });
            }
            let whole = round % 3 == 0;
            let Some(Ok(found)) = shebang(&file, whole) else {
                continue;
            };
            let line_end = file[2..].iter().position(|&byte| byte == b'\n').unwrap_or(file.len() - 2);
            assert!(line_end <= SHEBANG_MAX, "{file:?}");
            assert!(found.interpreter.starts_with('/'));
            assert!(!found.interpreter.bytes().any(|byte| is_blank(byte) || byte == 0 || byte == b'\n'));
            if let Some(argument) = found.argument {
                assert!(!argument.is_empty() && !argument.contains(&0) && !argument.contains(&b'\n'));
                assert!(!is_blank(argument[0]) && !is_blank(argument[argument.len() - 1]));
            }
        }
    }
}
