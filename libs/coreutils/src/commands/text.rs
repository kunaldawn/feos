//! Text-processing utilities.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;

use super::opts::{bad_option, Opt, Opts};
use super::{
    error, parse_i64, parse_usize, path_error, read_path, split_lines, usage, write_line,
    write_out, write_path, Ctx, MAX_WALK_DEPTH,
};
use crate::fs::{FsError, Kind};
use crate::regex::{Options as RegexOptions, Regex, Syntax};
use crate::sha256;

/// Widest field `printf` will pad to. A larger width is almost certainly a
/// typo, and honouring it would take the pipeline buffers with it.
const MAX_PRINTF_WIDTH: usize = 1024 * 1024;

pub(super) fn echo(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut newline = true;
    let mut escapes = false;
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        let flags = arg.strip_prefix('-').unwrap_or("");
        if arg.len() > 1
            && !flags.is_empty()
            && flags.chars().all(|flag| matches!(flag, 'n' | 'e' | 'E'))
        {
            for flag in flags.chars() {
                match flag {
                    'n' => newline = false,
                    'e' => escapes = true,
                    'E' => escapes = false,
                    _ => {}
                }
            }
            index += 1;
        } else {
            break;
        }
    }

    let mut text = String::new();
    for (position, arg) in args[index..].iter().enumerate() {
        if position > 0 {
            text.push(' ');
        }
        text.push_str(arg);
    }

    if escapes {
        let (data, stop) = unescape(&text);
        ctx.stdout.write(&data);
        if newline && !stop {
            ctx.stdout.write(b"\n");
        }
    } else {
        ctx.stdout.write(text.as_bytes());
        if newline {
            ctx.stdout.write(b"\n");
        }
    }
    0
}

fn unescape(text: &str) -> (Vec<u8>, bool) {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    let mut stop = false;

    while index < bytes.len() {
        if bytes[index] != b'\\' || index + 1 >= bytes.len() {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let escape = bytes[index];
        index += 1;
        match escape {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'v' => out.push(0x0b),
            b'e' => out.push(0x1b),
            b'\\' => out.push(b'\\'),
            b'0' => {
                let mut value = 0u16;
                let mut digits = 0;
                while digits < 3 && index < bytes.len() && (b'0'..=b'7').contains(&bytes[index]) {
                    value = value * 8 + u16::from(bytes[index] - b'0');
                    index += 1;
                    digits += 1;
                }
                out.push((value & 0xff) as u8);
            }
            b'x' => {
                let mut value = 0u16;
                let mut digits = 0;
                while digits < 2 && index < bytes.len() {
                    let Some(digit) = hex_digit(bytes[index]) else {
                        break;
                    };
                    value = value * 16 + u16::from(digit);
                    index += 1;
                    digits += 1;
                }
                if digits == 0 {
                    out.extend_from_slice(b"\\x");
                } else {
                    out.push(value as u8);
                }
            }
            b'c' => {
                stop = true;
                break;
            }
            b'1'..=b'7' => {
                let mut value = u16::from(escape - b'0');
                let mut digits = 1;
                while digits < 3 && index < bytes.len() && (b'0'..=b'7').contains(&bytes[index]) {
                    value = value * 8 + u16::from(bytes[index] - b'0');
                    index += 1;
                    digits += 1;
                }
                out.push((value & 0xff) as u8);
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    (out, stop)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parses a `printf` numeric argument: decimal, `0x` hex, `0` octal, or a
/// leading quote for a character's value.
fn printf_integer(text: &str) -> Option<i64> {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix(['\'', '"']) {
        return Some(rest.chars().next().map_or(0, |ch| ch as i64));
    }
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let value = if let Some(hex) = digits.strip_prefix("0x").or(digits.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        u64::from_str_radix(&digits[1..], 8).ok()?
    } else {
        digits.parse::<u64>().ok()?
    } as i64;
    Some(if negative { value.wrapping_neg() } else { value })
}

fn printf_number(field: &str) -> Result<usize, ()> {
    match field.parse::<usize>() {
        Ok(value) if value <= MAX_PRINTF_WIDTH => Ok(value),
        _ => Err(()),
    }
}

pub(super) fn printf(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.is_empty() {
        return usage(ctx.stderr, "printf", "printf FORMAT [arg...]");
    }

    let (format_bytes, _) = unescape(&args[0]);
    let bytes = format_bytes.as_slice();
    let mut argument = 1usize;
    let mut status = 0;

    loop {
        let start = argument;
        let mut index = 0usize;

        while index < bytes.len() {
            if bytes[index] != b'%' {
                let next = bytes[index..]
                    .iter()
                    .position(|byte| *byte == b'%')
                    .map_or(bytes.len(), |position| index + position);
                ctx.stdout.write(&bytes[index..next]);
                index = next;
                continue;
            }

            index += 1;
            if index >= bytes.len() {
                ctx.stdout.write(b"%");
                break;
            }
            if bytes[index] == b'%' {
                ctx.stdout.write(b"%");
                index += 1;
                continue;
            }

            let mut left = false;
            let mut zero = false;
            let mut plus = false;
            let mut space = false;
            let mut alternate = false;
            loop {
                match bytes.get(index) {
                    Some(b'-') => left = true,
                    Some(b'0') => zero = true,
                    Some(b'+') => plus = true,
                    Some(b' ') => space = true,
                    Some(b'#') => alternate = true,
                    _ => break,
                }
                index += 1;
            }

            let digits_at = |mut at: usize| -> (usize, &[u8]) {
                let from = at;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                (at, &bytes[from..at])
            };

            let mut width: Option<usize> = None;
            let width_field: String;
            if bytes.get(index) == Some(&b'*') {
                let value = args.get(argument).and_then(|value| parse_i64(value)).unwrap_or(0);
                if value < 0 {
                    left = true;
                }
                width_field = value.unsigned_abs().to_string();
                argument += 1;
                index += 1;
            } else {
                let (end, field) = digits_at(index);
                width_field = String::from_utf8_lossy(field).into_owned();
                index = end;
            }
            if !width_field.is_empty() {
                match printf_number(&width_field) {
                    Ok(value) => width = Some(value),
                    Err(()) => {
                        error(ctx.stderr, "printf", "field width too large");
                        return 1;
                    }
                }
            }

            let mut precision: Option<usize> = None;
            if bytes.get(index) == Some(&b'.') {
                index += 1;
                let field = if bytes.get(index) == Some(&b'*') {
                    let value = args.get(argument).and_then(|value| parse_i64(value)).unwrap_or(0);
                    argument += 1;
                    index += 1;
                    value.unsigned_abs().to_string()
                } else {
                    let (end, field) = digits_at(index);
                    let field = String::from_utf8_lossy(field).into_owned();
                    index = end;
                    field
                };
                match printf_number(if field.is_empty() { "0" } else { &field }) {
                    Ok(value) => precision = Some(value),
                    Err(()) => {
                        error(ctx.stderr, "printf", "precision too large");
                        return 1;
                    }
                }
            }

            // C length modifiers mean nothing here.
            while matches!(bytes.get(index), Some(b'l' | b'h' | b'z' | b'j' | b't')) {
                index += 1;
            }

            let Some(&specifier) = bytes.get(index) else {
                ctx.stdout.write(b"%");
                break;
            };
            index += 1;

            let value = args.get(argument).map_or("", String::as_str);
            let missing = argument >= args.len();
            argument += 1;

            let mut integer = |value: &str, ctx: &mut Ctx<'_>| -> i64 {
                if missing {
                    return 0;
                }
                match printf_integer(value) {
                    Some(number) => number,
                    None => {
                        error(ctx.stderr, "printf", &format!("{value}: invalid number"));
                        status = 1;
                        0
                    }
                }
            };

            let mut numeric = true;
            let rendered: Vec<u8> = match specifier {
                b's' => {
                    numeric = false;
                    let text = match precision {
                        Some(limit) => value.chars().take(limit).collect::<String>(),
                        None => value.to_string(),
                    };
                    text.into_bytes()
                }
                b'b' => {
                    numeric = false;
                    unescape(value).0
                }
                b'c' => {
                    numeric = false;
                    value
                        .chars()
                        .next()
                        .map_or_else(Vec::new, |ch| ch.to_string().into_bytes())
                }
                b'd' | b'i' => {
                    let number = integer(value, ctx);
                    let mut text = if plus && number >= 0 {
                        format!("+{number}")
                    } else if space && number >= 0 {
                        format!(" {number}")
                    } else {
                        format!("{number}")
                    };
                    if let Some(limit) = precision {
                        text = pad_integer(text, limit, plus || space);
                    }
                    text.into_bytes()
                }
                b'u' => format!("{}", integer(value, ctx) as u64).into_bytes(),
                b'x' => {
                    let number = integer(value, ctx) as u64;
                    let prefix = if alternate { "0x" } else { "" };
                    format!("{prefix}{number:x}").into_bytes()
                }
                b'X' => {
                    let number = integer(value, ctx) as u64;
                    let prefix = if alternate { "0X" } else { "" };
                    format!("{prefix}{number:X}").into_bytes()
                }
                b'o' => {
                    let number = integer(value, ctx) as u64;
                    let prefix = if alternate { "0" } else { "" };
                    format!("{prefix}{number:o}").into_bytes()
                }
                b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                    let number: f64 = value.trim().parse().unwrap_or(0.0);
                    let digits = precision.unwrap_or(6).min(64);
                    let mut text = match specifier {
                        b'e' | b'E' => format!("{number:.digits$e}"),
                        b'g' | b'G' => format!("{number}"),
                        _ => format!("{number:.digits$}"),
                    };
                    if plus && number >= 0.0 {
                        text.insert(0, '+');
                    }
                    text.into_bytes()
                }
                other => {
                    numeric = false;
                    alloc::vec![b'%', other]
                }
            };

            match width {
                Some(width) => {
                    let padded = pad(&rendered, width, left, zero && numeric);
                    ctx.stdout.write(&padded);
                }
                None => ctx.stdout.write(&rendered),
            }
            if ctx.cancelled() {
                return 130;
            }
        }

        if argument >= args.len() || argument == start {
            break;
        }
    }
    status
}

fn pad(text: &[u8], width: usize, left: bool, zero: bool) -> Vec<u8> {
    let length = core::str::from_utf8(text).map_or(text.len(), |text| text.chars().count());
    if length >= width {
        return text.to_vec();
    }
    let fill = width - length;
    let mut result = Vec::new();
    if result.try_reserve_exact(text.len() + fill).is_err() {
        return text.to_vec();
    }
    if left {
        result.extend_from_slice(text);
        result.resize(result.len() + fill, b' ');
    } else if zero {
        let sign = usize::from(matches!(text.first(), Some(b'-' | b'+')));
        result.extend_from_slice(&text[..sign]);
        result.resize(result.len() + fill, b'0');
        result.extend_from_slice(&text[sign..]);
    } else {
        result.resize(fill, b' ');
        result.extend_from_slice(text);
    }
    result
}

fn pad_integer(text: String, precision: usize, signed: bool) -> String {
    let digits = text.trim_start_matches('-').trim_start_matches('+');
    if digits.len() >= precision {
        return text;
    }
    let mut result = String::new();
    if text.starts_with('-') || (signed && text.starts_with('+')) {
        result.push(text.as_bytes()[0] as char);
    }
    for _ in 0..(precision - digits.len()) {
        result.push('0');
    }
    result.push_str(digits);
    result
}

/// What `head` or `tail` keeps.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Span {
    /// The first N units.
    First(usize),
    /// Everything except the last N units.
    AllButLast(usize),
    /// The last N units.
    Last(usize),
    /// Everything from unit N on, counting from 1.
    From(usize),
}

#[derive(Clone, Copy)]
struct SliceOptions {
    span: Span,
    bytes: bool,
    quiet: bool,
    verbose: bool,
}

fn run_head_tail(args: &[String], ctx: &mut Ctx<'_>, head: bool) -> i32 {
    let cmd = if head { "head" } else { "tail" };
    let parse_span = |value: &str| -> Option<Span> {
        if head {
            match value.strip_prefix('-') {
                Some(rest) => Some(Span::AllButLast(rest.parse().ok()?)),
                None => Some(Span::First(
                    value.strip_prefix('+').unwrap_or(value).parse().ok()?,
                )),
            }
        } else if let Some(rest) = value.strip_prefix('+') {
            Some(Span::From(rest.parse().ok()?))
        } else {
            Some(Span::Last(
                value.strip_prefix('-').unwrap_or(value).parse().ok()?,
            ))
        }
    };

    // The historic `head -5` spelling.
    let converted: Vec<String> = args
        .iter()
        .map(|arg| match arg.strip_prefix('-') {
            Some(rest) if !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()) => {
                format!("-n{rest}")
            }
            _ => arg.clone(),
        })
        .collect();

    let mut options = SliceOptions {
        span: if head { Span::First(10) } else { Span::Last(10) },
        bytes: false,
        quiet: false,
        verbose: false,
    };
    let mut opts = Opts::new(&converted, "n:c:qvf");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value(flag @ ('n' | 'c'), value) => {
                let Some(span) = parse_span(value) else {
                    let kind = if flag == 'n' { "lines" } else { "bytes" };
                    error(
                        ctx.stderr,
                        cmd,
                        &format!("invalid number of {kind}: '{value}'"),
                    );
                    return 1;
                };
                options.span = span;
                options.bytes = flag == 'c';
            }
            Opt::Flag('q') => options.quiet = true,
            Opt::Flag('v') => options.verbose = true,
            Opt::Flag('f') => {}
            other => return bad_option(ctx, cmd, other),
        }
    }
    let mut files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }
    run_slice(ctx, cmd, &files, options)
}

pub(super) fn head(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    run_head_tail(args, ctx, true)
}

pub(super) fn tail(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    run_head_tail(args, ctx, false)
}

fn run_slice(ctx: &mut Ctx<'_>, cmd: &str, files: &[String], options: SliceOptions) -> i32 {
    let mut status = 0;
    let show_headers = options.verbose || (files.len() > 1 && !options.quiet);

    for (index, file) in files.iter().enumerate() {
        if show_headers {
            if index > 0 {
                write_line(ctx.stdout, format_args!(""));
            }
            write_line(ctx.stdout, format_args!("==> {file} <=="));
        }
        if let Err(err) = slice_file(ctx, file, options) {
            path_error(ctx.stderr, cmd, file, err);
            status = 1;
        }
    }
    status
}

fn slice_file(ctx: &mut Ctx<'_>, file: &str, options: SliceOptions) -> Result<(), FsError> {
    if let Span::First(count) = options.span {
        return copy_first(ctx, file, count, options.bytes);
    }

    let data = read_path(ctx, file)?;
    if options.bytes {
        let selected = match options.span {
            Span::First(count) => &data[..count.min(data.len())],
            Span::AllButLast(count) => &data[..data.len().saturating_sub(count)],
            Span::Last(count) => &data[data.len().saturating_sub(count)..],
            Span::From(from) => &data[from.saturating_sub(1).min(data.len())..],
        };
        ctx.stdout.write(selected);
        return Ok(());
    }

    let lines = split_lines(&data);
    let selected = match options.span {
        Span::First(count) => &lines[..count.min(lines.len())],
        Span::AllButLast(count) => &lines[..lines.len().saturating_sub(count)],
        Span::Last(count) => &lines[lines.len().saturating_sub(count)..],
        Span::From(from) => &lines[from.saturating_sub(1).min(lines.len())..],
    };
    for line in selected {
        ctx.stdout.write(line);
    }
    Ok(())
}

/// `head` reads only what it prints, so `head -n 1` of a huge file is cheap.
fn copy_first(ctx: &mut Ctx<'_>, file: &str, count: usize, bytes: bool) -> Result<(), FsError> {
    let mut remaining = count;
    let mut chunk = [0u8; 4096];
    let mut offset = 0usize;
    while remaining > 0 {
        let wanted = if bytes {
            remaining.min(chunk.len())
        } else {
            chunk.len()
        };
        let read = if file == "-" {
            ctx.stdin.read(&mut chunk[..wanted])
        } else {
            ctx.fs.read_at(file, offset, &mut chunk[..wanted])?
        };
        if read == 0 {
            break;
        }
        offset += read;
        let mut end = read;
        if bytes {
            remaining -= read;
        } else {
            for (position, byte) in chunk[..read].iter().enumerate() {
                if *byte == b'\n' {
                    remaining -= 1;
                    if remaining == 0 {
                        end = position + 1;
                        break;
                    }
                }
            }
        }
        ctx.stdout.write(&chunk[..end]);
        if ctx.cancelled() {
            break;
        }
    }
    Ok(())
}

pub(super) fn wc(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut lines_flag = false;
    let mut words_flag = false;
    let mut bytes_flag = false;
    let mut chars_flag = false;
    let mut opts = Opts::new(args, "lwcm");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('l') => lines_flag = true,
            Opt::Flag('w') => words_flag = true,
            Opt::Flag('c') => bytes_flag = true,
            Opt::Flag('m') => chars_flag = true,
            other => return bad_option(ctx, "wc", other),
        }
    }
    if !(lines_flag || words_flag || bytes_flag || chars_flag) {
        lines_flag = true;
        words_flag = true;
        bytes_flag = true;
    }
    let mut files = core::mem::take(&mut opts.operands);
    // Standard input has no name unless the user wrote `-`.
    let implicit_stdin = files.is_empty();
    if implicit_stdin {
        files.push(String::from("-"));
    }

    let mut status = 0;
    let mut totals = Counts::default();
    let multiple = files.len() > 1;

    for file in &files {
        let data = match read_path(ctx, file) {
            Ok(data) => data,
            Err(err) => {
                path_error(ctx.stderr, "wc", file, err);
                status = 1;
                continue;
            }
        };
        let counts = Counts::of(&data);
        totals.add(counts);
        print_counts(
            ctx,
            counts,
            lines_flag,
            words_flag,
            bytes_flag,
            chars_flag,
            if implicit_stdin { None } else { Some(file) },
        );
    }

    if multiple {
        print_counts(
            ctx,
            totals,
            lines_flag,
            words_flag,
            bytes_flag,
            chars_flag,
            Some("total"),
        );
    }
    status
}

#[derive(Clone, Copy, Default)]
struct Counts {
    lines: usize,
    words: usize,
    bytes: usize,
    chars: usize,
}

impl Counts {
    fn of(data: &[u8]) -> Self {
        let lines = data.iter().filter(|byte| **byte == b'\n').count();
        let words = data
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|word| !word.is_empty())
            .count();
        let chars = core::str::from_utf8(data)
            .map(|text| text.chars().count())
            .unwrap_or(data.len());
        Self {
            lines,
            words,
            bytes: data.len(),
            chars,
        }
    }

    fn add(&mut self, other: Self) {
        self.lines += other.lines;
        self.words += other.words;
        self.bytes += other.bytes;
        self.chars += other.chars;
    }
}

fn print_counts(
    ctx: &mut Ctx<'_>,
    counts: Counts,
    lines_flag: bool,
    words_flag: bool,
    bytes_flag: bool,
    chars_flag: bool,
    name: Option<&str>,
) {
    let mut fields: Vec<usize> = Vec::new();
    if lines_flag {
        fields.push(counts.lines);
    }
    if words_flag {
        fields.push(counts.words);
    }
    if bytes_flag {
        fields.push(counts.bytes);
    }
    if chars_flag {
        fields.push(counts.chars);
    }

    if fields.len() == 1 {
        if let Some(name) = name {
            write_line(ctx.stdout, format_args!("{} {name}", fields[0]));
        } else {
            write_line(ctx.stdout, format_args!("{}", fields[0]));
        }
        return;
    }

    write_out(ctx.stdout, format_args!(""));
    for (index, field) in fields.iter().enumerate() {
        if index == 0 {
            write_out(ctx.stdout, format_args!("{field:>7}"));
        } else {
            write_out(ctx.stdout, format_args!("{field:>8}"));
        }
    }
    if let Some(name) = name {
        write_out(ctx.stdout, format_args!(" {name}"));
    }
    ctx.stdout.write(b"\n");
}

#[derive(Clone)]
struct GrepOptions {
    invert: bool,
    line_number: bool,
    count: bool,
    files_with_matches: bool,
    files_without_matches: bool,
    quiet: bool,
    only_matching: bool,
    no_messages: bool,
    with_name: Option<bool>,
    recursive: bool,
    max_count: Option<usize>,
    after: usize,
    before: usize,
}

const GREP_USAGE: &str = "grep [-EFGivnclLqwxrohHs] [-m N] [-A N] [-B N] [-C N] [-e PATTERN] [-f FILE] PATTERN [file...]";

pub(super) fn grep(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut options = GrepOptions {
        invert: false,
        line_number: false,
        count: false,
        files_with_matches: false,
        files_without_matches: false,
        quiet: false,
        only_matching: false,
        no_messages: false,
        with_name: None,
        recursive: false,
        max_count: None,
        after: 0,
        before: 0,
    };
    let mut syntax = Syntax::Basic;
    let mut ignore_case = false;
    let mut whole_word = false;
    let mut whole_line = false;
    let mut patterns: Vec<String> = Vec::new();
    let mut have_pattern = false;

    let mut opts = Opts::new(args, "EFGivnclLqwxrRohHsze:f:m:A:B:C:");
    while let Some(opt) = opts.next() {
        let context_value = |value: &str| value.parse::<usize>().ok();
        match opt {
            Opt::Flag('E') | Opt::Long("extended-regexp", None) => syntax = Syntax::Extended,
            Opt::Flag('F') | Opt::Long("fixed-strings", None) => syntax = Syntax::Fixed,
            Opt::Flag('G') | Opt::Long("basic-regexp", None) => syntax = Syntax::Basic,
            Opt::Flag('i') | Opt::Long("ignore-case", None) => ignore_case = true,
            Opt::Flag('v') | Opt::Long("invert-match", None) => options.invert = true,
            Opt::Flag('n') | Opt::Long("line-number", None) => options.line_number = true,
            Opt::Flag('c') | Opt::Long("count", None) => options.count = true,
            Opt::Flag('l') | Opt::Long("files-with-matches", None) => {
                options.files_with_matches = true
            }
            Opt::Flag('L') | Opt::Long("files-without-match", None) => {
                options.files_without_matches = true
            }
            Opt::Flag('q') | Opt::Long("quiet" | "silent", None) => options.quiet = true,
            Opt::Flag('w') | Opt::Long("word-regexp", None) => whole_word = true,
            Opt::Flag('x') | Opt::Long("line-regexp", None) => whole_line = true,
            Opt::Flag('r' | 'R') | Opt::Long("recursive", None) => options.recursive = true,
            Opt::Flag('o') | Opt::Long("only-matching", None) => options.only_matching = true,
            Opt::Flag('h') | Opt::Long("no-filename", None) => options.with_name = Some(false),
            Opt::Flag('H') | Opt::Long("with-filename", None) => options.with_name = Some(true),
            Opt::Flag('s') | Opt::Long("no-messages", None) => options.no_messages = true,
            Opt::Flag('z') => {}
            Opt::Value('e', pattern) => {
                have_pattern = true;
                patterns.extend(pattern.split('\n').map(str::to_string));
            }
            Opt::Value('f', path) => {
                have_pattern = true;
                match ctx.fs.read_file(path) {
                    Ok(data) => {
                        for line in split_lines(&data) {
                            let line = line.strip_suffix(b"\n").unwrap_or(line);
                            patterns.push(String::from_utf8_lossy(line).into_owned());
                        }
                    }
                    Err(err) => {
                        path_error(ctx.stderr, "grep", path, err);
                        return 2;
                    }
                }
            }
            Opt::Value(flag @ ('m' | 'A' | 'B' | 'C'), value) => {
                let Some(number) = context_value(value) else {
                    error(ctx.stderr, "grep", &format!("{value}: invalid number"));
                    return 2;
                };
                match flag {
                    'm' => options.max_count = Some(number),
                    'A' => options.after = number,
                    'B' => options.before = number,
                    _ => {
                        options.after = number;
                        options.before = number;
                    }
                }
            }
            other => return bad_option(ctx, "grep", other),
        }
    }

    let mut files = core::mem::take(&mut opts.operands);
    if !have_pattern {
        if files.is_empty() {
            return usage(ctx.stderr, "grep", GREP_USAGE);
        }
        patterns.extend(files.remove(0).split('\n').map(str::to_string));
    }
    if files.is_empty() {
        files.push(String::from(if options.recursive { "." } else { "-" }));
    }

    let mut regexes = Vec::new();
    for pattern in &patterns {
        let compile = RegexOptions {
            syntax,
            ignore_case,
            whole_word,
            whole_line,
        };
        match Regex::with_options(pattern, compile) {
            Ok(regex) => regexes.push(regex),
            Err(message) => {
                error(ctx.stderr, "grep", message);
                return 2;
            }
        }
    }

    let mut matched = false;
    let mut status = 0;
    let multiple = files.len() > 1 || options.recursive;

    for file in files {
        if ctx.cancelled() {
            break;
        }
        if file != "-" && ctx.fs.is_dir(&file) {
            if !options.recursive {
                if !options.no_messages {
                    error(ctx.stderr, "grep", &format!("{file}: Is a directory"));
                }
                status = 2;
                continue;
            }
            if let Err(err) = grep_tree(ctx, &file, &regexes, &options, &mut matched, &mut status, 0)
            {
                path_error(ctx.stderr, "grep", &file, err);
                status = 2;
            }
        } else if let Err(err) = grep_file(ctx, &file, &regexes, &options, multiple, &mut matched) {
            if !options.no_messages {
                path_error(ctx.stderr, "grep", &file, err);
            }
            status = 2;
        }
        if options.quiet && matched {
            return 0;
        }
    }

    if status == 2 {
        2
    } else if matched {
        0
    } else {
        1
    }
}

fn grep_tree(
    ctx: &mut Ctx<'_>,
    path: &str,
    regexes: &[Regex],
    options: &GrepOptions,
    matched: &mut bool,
    status: &mut i32,
    depth: usize,
) -> Result<(), FsError> {
    if depth > MAX_WALK_DEPTH {
        return Err(FsError::TooDeep);
    }
    let entries = ctx.fs.read_dir(path)?;
    for entry in entries {
        if ctx.cancelled() {
            break;
        }
        let child = crate::fs::join(path, &entry.name);
        if entry.kind == Kind::Dir {
            grep_tree(ctx, &child, regexes, options, matched, status, depth + 1)?;
        } else if let Err(err) = grep_file(ctx, &child, regexes, options, true, matched) {
            path_error(ctx.stderr, "grep", &child, err);
            *status = 2;
        }
    }
    Ok(())
}

fn print_grep_line(
    ctx: &mut Ctx<'_>,
    file: &str,
    show_name: bool,
    number: Option<usize>,
    separator: char,
    line: &[u8],
) {
    if show_name {
        write_out(ctx.stdout, format_args!("{file}{separator}"));
    }
    if let Some(number) = number {
        write_out(ctx.stdout, format_args!("{number}{separator}"));
    }
    ctx.stdout.write(line);
    ctx.stdout.write(b"\n");
}

fn grep_file(
    ctx: &mut Ctx<'_>,
    file: &str,
    regexes: &[Regex],
    options: &GrepOptions,
    multiple: bool,
    matched_any: &mut bool,
) -> Result<(), FsError> {
    let data = read_path(ctx, file)?;
    let display = if file == "-" { "(standard input)" } else { file };
    let show_name = options.with_name.unwrap_or(multiple);
    let lister = options.count || options.files_with_matches || options.files_without_matches;
    let context = (options.after > 0 || options.before > 0) && !lister && !options.only_matching;

    let mut count = 0usize;
    let mut after_left = 0usize;
    let mut last_printed: Option<usize> = None;
    let lines = split_lines(&data);

    for (index, line) in lines.iter().enumerate() {
        if index % 256 == 0 && ctx.cancelled() {
            break;
        }
        let raw = line.strip_suffix(b"\n").unwrap_or(line);
        let is_match = regexes.iter().any(|regex| regex.is_match(raw));

        if is_match != options.invert {
            count += 1;
            *matched_any = true;
            if options.quiet || options.files_with_matches {
                break;
            }
            if !lister {
                if context {
                    let first = index.saturating_sub(options.before);
                    let start = match last_printed {
                        Some(last) if last + 1 >= first => last + 1,
                        Some(_) => {
                            ctx.stdout.write(b"--\n");
                            first
                        }
                        None => first,
                    };
                    for (offset, before) in lines[start..index].iter().enumerate() {
                        let before = before.strip_suffix(b"\n").unwrap_or(before);
                        let number = options.line_number.then_some(start + offset + 1);
                        print_grep_line(ctx, display, show_name, number, '-', before);
                    }
                    after_left = options.after;
                    last_printed = Some(index);
                }
                let number = options.line_number.then_some(index + 1);
                if options.only_matching {
                    if !options.invert {
                        print_matches(ctx, display, show_name, number, regexes, raw);
                    }
                } else {
                    print_grep_line(ctx, display, show_name, number, ':', raw);
                }
            }
            if options.max_count.is_some_and(|max| count >= max) {
                // Trailing context still prints after the last match.
                for (offset, tail) in lines[index + 1..].iter().take(after_left).enumerate() {
                    let tail = tail.strip_suffix(b"\n").unwrap_or(tail);
                    let number = options.line_number.then_some(index + offset + 2);
                    print_grep_line(ctx, display, show_name, number, '-', tail);
                }
                break;
            }
        } else if context && after_left > 0 {
            after_left -= 1;
            let number = options.line_number.then_some(index + 1);
            print_grep_line(ctx, display, show_name, number, '-', raw);
            last_printed = Some(index);
        }
    }

    if options.count {
        if show_name {
            write_line(ctx.stdout, format_args!("{display}:{count}"));
        } else {
            write_line(ctx.stdout, format_args!("{count}"));
        }
    }
    if options.files_with_matches && count > 0 {
        write_line(ctx.stdout, format_args!("{display}"));
    }
    if options.files_without_matches && count == 0 {
        write_line(ctx.stdout, format_args!("{display}"));
    }

    Ok(())
}

/// Prints every non-empty match on a line, for `grep -o`.
fn print_matches(
    ctx: &mut Ctx<'_>,
    file: &str,
    show_name: bool,
    number: Option<usize>,
    regexes: &[Regex],
    raw: &[u8],
) {
    let mut from = 0usize;
    while from <= raw.len() {
        let mut best: Option<(usize, usize)> = None;
        for regex in regexes {
            if let Some(caps) = regex.find_at(raw, from) {
                let span = caps.span();
                let better = match best {
                    Some((start, end)) => {
                        span.0 < start || (span.0 == start && span.1 > end)
                    }
                    None => true,
                };
                if better {
                    best = Some(span);
                }
            }
        }
        let Some((start, end)) = best else {
            return;
        };
        if end > start {
            print_grep_line(ctx, file, show_name, number, ':', &raw[start..end]);
            from = end;
        } else {
            from = start + 1;
        }
    }
}

/// Comparison rules for one `sort` key, or the whole line.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct SortFlags {
    reverse: bool,
    numeric: bool,
    human: bool,
    fold: bool,
    ignore_blanks: bool,
    dictionary: bool,
}

impl SortFlags {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn apply(&mut self, flag: char) -> bool {
        match flag {
            'r' => self.reverse = true,
            'n' | 'g' => self.numeric = true,
            'h' => self.human = true,
            'f' => self.fold = true,
            'b' => self.ignore_blanks = true,
            'd' => self.dictionary = true,
            _ => return false,
        }
        true
    }
}

#[derive(Clone, Copy)]
struct SortKey {
    start_field: usize,
    start_char: usize,
    end_field: usize,
    end_char: usize,
    flags: SortFlags,
}

#[derive(Clone)]
struct SortOptions {
    flags: SortFlags,
    unique: bool,
    stable: bool,
    delimiter: Option<u8>,
    keys: Vec<SortKey>,
}

/// Most lines `sort` will hold, which bounds its index to 16 MiB of heap.
const MAX_SORT_LINES: usize = 1024 * 1024;

pub(super) fn sort(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut options = SortOptions {
        flags: SortFlags::default(),
        unique: false,
        stable: false,
        delimiter: None,
        keys: Vec::new(),
    };
    let mut check = false;
    let mut output: Option<String> = None;

    let mut opts = Opts::new(args, "rnghfbdusmcCt:k:o:T:S:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('u') => options.unique = true,
            Opt::Flag('s') => options.stable = true,
            Opt::Flag('m') => {}
            Opt::Flag('c' | 'C') => check = true,
            Opt::Flag(flag) if options.flags.apply(flag) => {}
            Opt::Value('t', value) => {
                options.delimiter = match value.as_bytes() {
                    [byte] => Some(*byte),
                    b"\\t" => Some(b'\t'),
                    _ => {
                        error(ctx.stderr, "sort", "multi-character tab");
                        return 2;
                    }
                };
            }
            Opt::Value('k', value) => match parse_key(value) {
                Some(key) => options.keys.push(key),
                None => {
                    error(ctx.stderr, "sort", &format!("invalid key: '{value}'"));
                    return 2;
                }
            },
            Opt::Value('o', value) => output = Some(value.to_string()),
            Opt::Value('T' | 'S', _) => {}
            other => return bad_option(ctx, "sort", other),
        }
    }
    let mut files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let Some(data) = read_paths(ctx, &files, "sort") else {
        return 2;
    };
    let mut lines = split_lines(&data);
    for line in &mut lines {
        *line = line.strip_suffix(b"\n").unwrap_or(line);
    }
    if lines.len() > MAX_SORT_LINES {
        error(ctx.stderr, "sort", "input has too many lines");
        return 2;
    }

    if check {
        for (index, pair) in lines.windows(2).enumerate() {
            let ordering = compare_lines(pair[0], pair[1], &options);
            if ordering == Ordering::Greater || (options.unique && ordering == Ordering::Equal) {
                let line = String::from_utf8_lossy(pair[1]);
                error(
                    ctx.stderr,
                    "sort",
                    &format!("disorder at line {}: {line}", index + 2),
                );
                return 1;
            }
        }
        return 0;
    }

    // Sorting slices in place needs no scratch buffer and no per-line
    // allocation, so `seq 1000000 | sort` fits the kernel heap. The last-resort
    // comparison makes the order deterministic without a stable sort.
    lines.sort_unstable_by(|left, right| {
        compare_lines(left, right, &options).then_with(|| {
            if options.stable || options.unique {
                // Equal keys keep input order: slices of one buffer compare by
                // their address, which is their position in the input.
                (left.as_ptr() as usize).cmp(&(right.as_ptr() as usize))
            } else if options.flags.reverse {
                right.cmp(left)
            } else {
                left.cmp(right)
            }
        })
    });

    let mut rendered = Vec::new();
    if rendered.try_reserve(data.len() + 1).is_err() {
        error(ctx.stderr, "sort", "out of memory");
        return 2;
    }
    let mut previous: Option<&[u8]> = None;
    for line in &lines {
        if options.unique {
            if let Some(last) = previous {
                if compare_lines(last, line, &options) == Ordering::Equal {
                    continue;
                }
            }
            previous = Some(line);
        }
        rendered.extend_from_slice(line);
        rendered.push(b'\n');
    }

    match output {
        Some(path) => {
            if !write_path(ctx, "sort", &path, &rendered, false) {
                return 2;
            }
        }
        None => ctx.stdout.write(&rendered),
    }
    0
}

/// Parses `F[.C][OPTS][,F[.C][OPTS]]`.
fn parse_key(value: &str) -> Option<SortKey> {
    let (start, end) = match value.split_once(',') {
        Some((start, end)) => (start, Some(end)),
        None => (value, None),
    };
    let mut flags = SortFlags::default();
    let mut position = |text: &str| -> Option<(usize, usize)> {
        let digits_end = text
            .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
            .unwrap_or(text.len());
        let (numbers, suffix) = text.split_at(digits_end);
        for flag in suffix.chars() {
            if !flags.apply(flag) {
                return None;
            }
        }
        match numbers.split_once('.') {
            Some((field, column)) => Some((field.parse().ok()?, column.parse().ok()?)),
            None => Some((numbers.parse().ok()?, 0)),
        }
    };
    let (start_field, start_char) = position(start)?;
    if start_field == 0 {
        return None;
    }
    let (end_field, end_char) = match end {
        Some(end) => position(end)?,
        None => (usize::MAX, 0),
    };
    Some(SortKey {
        start_field,
        start_char,
        end_field,
        end_char,
        flags,
    })
}

/// Finds where field `field` (from 1) starts and ends.
fn field_span(line: &[u8], field: usize, delimiter: Option<u8>) -> (usize, usize) {
    let mut start = 0usize;
    let mut current = 1usize;
    loop {
        let mut end = start;
        match delimiter {
            Some(byte) => {
                while end < line.len() && line[end] != byte {
                    end += 1;
                }
            }
            None => {
                // A field includes the blanks that lead up to it.
                while end < line.len() && matches!(line[end], b' ' | b'\t') {
                    end += 1;
                }
                while end < line.len() && !matches!(line[end], b' ' | b'\t') {
                    end += 1;
                }
            }
        }
        if current == field || end >= line.len() {
            if current < field {
                return (line.len(), line.len());
            }
            return (start, end);
        }
        start = if delimiter.is_some() { end + 1 } else { end };
        current += 1;
    }
}

fn key_slice<'a>(line: &'a [u8], key: &SortKey, options: &SortOptions) -> &'a [u8] {
    let delimiter = options.delimiter;
    let skip_blanks = key.flags.ignore_blanks || options.flags.ignore_blanks;
    let (field_start, field_end) = field_span(line, key.start_field, delimiter);
    let mut from = field_start;
    if skip_blanks {
        while from < field_end && matches!(line[from], b' ' | b'\t') {
            from += 1;
        }
    }
    if key.start_char > 1 {
        from = (from + key.start_char - 1).min(line.len());
    }

    let to = if key.end_field == usize::MAX {
        line.len()
    } else {
        let (end_start, end_end) = field_span(line, key.end_field, delimiter);
        if key.end_char == 0 {
            end_end
        } else {
            let mut base = end_start;
            if skip_blanks {
                while base < end_end && matches!(line[base], b' ' | b'\t') {
                    base += 1;
                }
            }
            (base + key.end_char).min(line.len())
        }
    };
    line.get(from..to.max(from)).unwrap_or(&[])
}

fn compare_with(left: &[u8], right: &[u8], flags: &SortFlags) -> Ordering {
    if flags.numeric || flags.human {
        let number = |text: &[u8]| {
            if flags.human {
                human_number(text)
            } else {
                parse_number(text)
            }
        };
        return number(left)
            .partial_cmp(&number(right))
            .unwrap_or(Ordering::Equal);
    }
    let skip = |byte: &u8| {
        !flags.dictionary || byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'\t')
    };
    let fold = |byte: &u8| {
        if flags.fold {
            byte.to_ascii_lowercase()
        } else {
            *byte
        }
    };
    let (left, right) = if flags.ignore_blanks {
        (trim_blanks(left), trim_blanks(right))
    } else {
        (left, right)
    };
    left.iter()
        .filter(|byte| skip(byte))
        .map(fold)
        .cmp(right.iter().filter(|byte| skip(byte)).map(fold))
}

fn trim_blanks(text: &[u8]) -> &[u8] {
    let start = text
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(text.len());
    &text[start..]
}

fn compare_lines(left: &[u8], right: &[u8], options: &SortOptions) -> Ordering {
    for key in &options.keys {
        let flags = if key.flags.is_default() {
            options.flags
        } else {
            key.flags
        };
        let ordering = compare_with(
            key_slice(left, key, options),
            key_slice(right, key, options),
            &flags,
        );
        let ordering = if flags.reverse {
            ordering.reverse()
        } else {
            ordering
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    if !options.keys.is_empty() {
        return Ordering::Equal;
    }
    let ordering = compare_with(left, right, &options.flags);
    if options.flags.reverse {
        ordering.reverse()
    } else {
        ordering
    }
}

fn parse_number(text: &[u8]) -> f64 {
    let text = trim_blanks(text);
    let mut end = 0usize;
    if matches!(text.first(), Some(b'-' | b'+')) {
        end = 1;
    }
    let mut seen_dot = false;
    while end < text.len() {
        match text[end] {
            b'0'..=b'9' => {}
            b'.' if !seen_dot => seen_dot = true,
            _ => break,
        }
        end += 1;
    }
    core::str::from_utf8(&text[..end])
        .ok()
        .and_then(|number| number.parse().ok())
        .unwrap_or(0.0)
}

fn human_number(text: &[u8]) -> f64 {
    let trimmed = trim_blanks(text);
    let digits = trimmed
        .iter()
        .position(|byte| !byte.is_ascii_digit() && !matches!(byte, b'.' | b'-' | b'+'))
        .unwrap_or(trimmed.len());
    let scale = match trimmed.get(digits) {
        Some(b'k' | b'K') => 1e3,
        Some(b'M') => 1e6,
        Some(b'G') => 1e9,
        Some(b'T') => 1e12,
        _ => 1.0,
    };
    parse_number(&trimmed[..digits]) * scale
}

pub(super) fn uniq(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut count = false;
    let mut duplicates = false;
    let mut unique_only = false;
    let mut ignore_case = false;
    let mut skip_fields = 0usize;
    let mut skip_chars = 0usize;
    let mut width: Option<usize> = None;

    let mut opts = Opts::new(args, "cduif:s:w:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('c') => count = true,
            Opt::Flag('d') => duplicates = true,
            Opt::Flag('u') => unique_only = true,
            Opt::Flag('i') => ignore_case = true,
            Opt::Value('f', value) => skip_fields = parse_usize(value).unwrap_or(0),
            Opt::Value('s', value) => skip_chars = parse_usize(value).unwrap_or(0),
            Opt::Value('w', value) => width = parse_usize(value),
            other => return bad_option(ctx, "uniq", other),
        }
    }
    let files = core::mem::take(&mut opts.operands);
    if files.len() > 2 {
        return usage(ctx.stderr, "uniq", "uniq [-cdui] [-f N] [-s N] [-w N] [input [output]]");
    }
    let input = files.first().map_or("-", String::as_str);

    let data = match read_path(ctx, input) {
        Ok(data) => data,
        Err(err) => {
            path_error(ctx.stderr, "uniq", input, err);
            return 1;
        }
    };
    let key_of = |line: &'_ [u8]| -> (usize, usize) {
        let body = line.strip_suffix(b"\n").map_or(line.len(), <[u8]>::len);
        let mut at = 0usize;
        for _ in 0..skip_fields {
            while at < body && matches!(line[at], b' ' | b'\t') {
                at += 1;
            }
            while at < body && !matches!(line[at], b' ' | b'\t') {
                at += 1;
            }
        }
        at = (at + skip_chars).min(body);
        let end = width.map_or(body, |width| (at.saturating_add(width)).min(body));
        (at, end)
    };
    let same = |left: &[u8], right: &[u8]| -> bool {
        let (left_at, left_end) = key_of(left);
        let (right_at, right_end) = key_of(right);
        let (left, right) = (&left[left_at..left_end], &right[right_at..right_end]);
        if ignore_case {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };

    let lines = split_lines(&data);
    let mut rendered = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let mut end = index + 1;
        while end < lines.len() && same(lines[index], lines[end]) {
            end += 1;
        }
        let repeats = end - index;
        let emit = if duplicates {
            repeats > 1
        } else if unique_only {
            repeats == 1
        } else {
            true
        };
        if emit {
            if count {
                rendered.extend_from_slice(format!("{repeats:>7} ").as_bytes());
            }
            let line = lines[index].strip_suffix(b"\n").unwrap_or(lines[index]);
            rendered.extend_from_slice(line);
            rendered.push(b'\n');
        }
        index = end;
    }
    match files.get(1) {
        Some(path) => {
            if !write_path(ctx, "uniq", path, &rendered, false) {
                return 1;
            }
        }
        None => ctx.stdout.write(&rendered),
    }
    0
}

pub(super) fn cut(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut delimiter = String::from("\t");
    let mut fields: Option<Vec<(usize, usize)>> = None;
    let mut characters: Option<Vec<(usize, usize)>> = None;
    let mut bytes_mode = false;
    let mut suppress = false;

    let mut opts = Opts::new(args, "d:f:c:b:sn");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('d', value) => {
                if value.chars().count() > 1 {
                    error(ctx.stderr, "cut", "the delimiter must be a single character");
                    return 1;
                }
                delimiter = value.to_string();
            }
            Opt::Value(flag @ ('f' | 'c' | 'b'), value) => {
                let Some(list) = parse_list(value) else {
                    error(ctx.stderr, "cut", &format!("invalid list: '{value}'"));
                    return 1;
                };
                match flag {
                    'f' => fields = Some(list),
                    'b' => {
                        characters = Some(list);
                        bytes_mode = true;
                    }
                    _ => characters = Some(list),
                }
            }
            Opt::Flag('s') => suppress = true,
            Opt::Flag('n') => {}
            other => return bad_option(ctx, "cut", other),
        }
    }

    if fields.is_none() && characters.is_none() {
        return usage(ctx.stderr, "cut", "cut -d C -f LIST [file...]");
    }
    let mut files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let data = match read_paths(ctx, &files, "cut") {
        Some(data) => data,
        None => return 1,
    };
    let wanted =
        |ranges: &[(usize, usize)], position: usize| ranges.iter().any(|(a, b)| (*a..=*b).contains(&position));
    let separator = if delimiter.is_empty() {
        alloc::vec![b'\t']
    } else {
        delimiter.into_bytes()
    };

    for line in split_lines(&data) {
        let raw = line.strip_suffix(b"\n").unwrap_or(line);
        let mut rendered: Vec<u8> = Vec::new();

        if let Some(ranges) = &characters {
            if bytes_mode {
                for (index, byte) in raw.iter().enumerate() {
                    if wanted(ranges, index + 1) {
                        rendered.push(*byte);
                    }
                }
            } else {
                let text = String::from_utf8_lossy(raw);
                for (index, ch) in text.chars().enumerate() {
                    if wanted(ranges, index + 1) {
                        let mut buffer = [0u8; 4];
                        rendered.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                    }
                }
            }
        } else if let Some(ranges) = &fields {
            if !contains_bytes(raw, &separator) {
                // A line without the delimiter passes through whole.
                if suppress {
                    continue;
                }
                rendered.extend_from_slice(raw);
            } else {
                let mut first = true;
                let mut position = 1usize;
                let mut rest = raw;
                loop {
                    let (part, next) = match find_bytes(rest, &separator) {
                        Some(at) => (&rest[..at], Some(&rest[at + separator.len()..])),
                        None => (rest, None),
                    };
                    if wanted(ranges, position) {
                        if !first {
                            rendered.extend_from_slice(&separator);
                        }
                        rendered.extend_from_slice(part);
                        first = false;
                    }
                    position += 1;
                    match next {
                        Some(next) => rest = next,
                        None => break,
                    }
                }
            }
        }

        rendered.push(b'\n');
        ctx.stdout.write(&rendered);
    }
    0
}

fn find_bytes(text: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    text.windows(needle.len()).position(|window| window == needle)
}

fn contains_bytes(text: &[u8], needle: &[u8]) -> bool {
    find_bytes(text, needle).is_some()
}

/// Parses a `cut` list such as `1,3-5,7-` or `-2`. Positions start at 1.
fn parse_list(text: &str) -> Option<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    for part in text.split([',', ' ']) {
        let range = if let Some((start, end)) = part.split_once('-') {
            let start = if start.is_empty() {
                1
            } else {
                parse_usize(start)?
            };
            let end = if end.is_empty() {
                usize::MAX
            } else {
                parse_usize(end)?
            };
            if start > end {
                return None;
            }
            (start, end)
        } else {
            let value = parse_usize(part)?;
            (value, value)
        };
        if range.0 == 0 {
            return None;
        }
        ranges.push(range);
    }
    Some(ranges)
}

pub(super) fn tr(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut delete = false;
    let mut squeeze = false;
    let mut complement = false;
    let mut opts = Opts::new(args, "dscCt");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('d') => delete = true,
            Opt::Flag('s') => squeeze = true,
            Opt::Flag('c' | 'C') => complement = true,
            Opt::Flag('t') => {}
            other => return bad_option(ctx, "tr", other),
        }
    }
    let sets = core::mem::take(&mut opts.operands);
    if sets.is_empty() || sets.len() > 2 {
        return usage(ctx.stderr, "tr", "tr [-dsc] SET1 [SET2]");
    }

    let mut set1 = expand_set(&sets[0]);
    if complement {
        let mut member = [false; 256];
        for byte in &set1 {
            member[usize::from(*byte)] = true;
        }
        set1 = (0u8..=255).filter(|byte| !member[usize::from(*byte)]).collect();
    }
    let set2 = sets.get(1).map(|set| expand_set(set)).unwrap_or_default();

    let mut in_set1 = [false; 256];
    for byte in &set1 {
        in_set1[usize::from(*byte)] = true;
    }
    let mut table = [0u8; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        *slot = index as u8;
    }
    if !delete {
        if let Some(last) = set2.last() {
            for (index, byte) in set1.iter().enumerate() {
                table[usize::from(*byte)] = set2.get(index).copied().unwrap_or(*last);
            }
        }
    }

    // `-s` squeezes the last set given: SET2 when translating or deleting.
    let squeeze_source = if set2.is_empty() { &set1 } else { &set2 };
    let mut squeezable = [false; 256];
    for byte in squeeze_source {
        squeezable[usize::from(*byte)] = true;
    }

    let data = match ctx.stdin.try_read_to_end(crate::io::MAX_STREAM_BYTES) {
        Ok(data) => data,
        Err(err) => {
            error(ctx.stderr, "tr", err.text());
            return 1;
        }
    };
    let mut output = Vec::with_capacity(data.len());
    let mut previous: Option<u8> = None;
    for byte in data {
        if delete && in_set1[usize::from(byte)] {
            continue;
        }
        let mapped = table[usize::from(byte)];
        if squeeze && previous == Some(mapped) && squeezable[usize::from(mapped)] {
            continue;
        }
        output.push(mapped);
        previous = Some(mapped);
    }
    ctx.stdout.write(&output);
    0
}

/// Expands a `tr` set: escapes, ranges, `[:class:]` and `[x*n]`.
fn expand_set(set: &str) -> Vec<u8> {
    let (bytes, _) = unescape(set);
    let mut result = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'[' && bytes.get(index + 1) == Some(&b':') {
            let rest = &bytes[index + 2..];
            if let Some(length) = rest.windows(2).position(|window| window == b":]") {
                let name = core::str::from_utf8(&rest[..length]).unwrap_or("");
                if crate::glob::posix_class(name, 'a').is_some() {
                    // Classes expand in byte order, so upper and lower line up.
                    for candidate in 0u8..128 {
                        if crate::glob::posix_class(name, candidate as char) == Some(true) {
                            result.push(candidate);
                        }
                    }
                    index += 2 + length + 2;
                    continue;
                }
            }
        }
        if bytes[index] == b'[' && bytes.get(index + 2) == Some(&b'*') {
            let rest = &bytes[index + 3..];
            if let Some(length) = rest.iter().position(|byte| *byte == b']') {
                let count: usize = core::str::from_utf8(&rest[..length])
                    .ok()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(256);
                let value = bytes[index + 1];
                result.resize(result.len() + count.min(256), value);
                index += 3 + length + 1;
                continue;
            }
        }
        if index + 2 < bytes.len() && bytes[index + 1] == b'-' && bytes[index] <= bytes[index + 2] {
            for byte in bytes[index]..=bytes[index + 2] {
                result.push(byte);
            }
            index += 3;
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    result
}

pub(super) fn seq(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut separator = String::from("\n");
    let mut equal_width = false;
    let mut opts = Opts::new(args, "s:w").numbers_are_operands();
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('s', value) => separator = value.to_string(),
            Opt::Flag('w') => equal_width = true,
            other => return bad_option(ctx, "seq", other),
        }
    }

    let mut numbers = [0i64; 3];
    let operands = core::mem::take(&mut opts.operands);
    if operands.is_empty() || operands.len() > 3 {
        return usage(ctx.stderr, "seq", "seq [-w] [-s SEP] [first [step]] last");
    }
    for (slot, text) in numbers.iter_mut().zip(&operands) {
        match parse_i64(text) {
            Some(value) => *slot = value,
            None => return invalid_number(ctx, "seq", text),
        }
    }
    let (first, step, last) = match operands.len() {
        1 => (1, 1, numbers[0]),
        2 => (numbers[0], 1, numbers[1]),
        _ => (numbers[0], numbers[1], numbers[2]),
    };

    if step == 0 {
        error(ctx.stderr, "seq", "step cannot be zero");
        return 1;
    }

    let width = if equal_width {
        core::cmp::max(format!("{first}").len(), format!("{last}").len())
    } else {
        0
    };

    let mut value = first;
    let mut printed = 0usize;
    let mut first_item = true;
    loop {
        if (step > 0 && value > last) || (step < 0 && value < last) {
            break;
        }
        if printed >= 1_000_000 {
            error(ctx.stderr, "seq", "too many values (limit 1000000)");
            return 1;
        }
        if printed % 256 == 0 && ctx.cancelled() {
            return 130;
        }
        if !first_item {
            ctx.stdout.write(separator.as_bytes());
        }
        let text = if equal_width {
            format!("{value:0width$}")
        } else {
            format!("{value}")
        };
        ctx.stdout.write(text.as_bytes());
        first_item = false;
        printed += 1;
        let Some(next) = value.checked_add(step) else {
            break;
        };
        value = next;
    }
    if !first_item {
        ctx.stdout.write(b"\n");
    }
    0
}

fn invalid_number(ctx: &mut Ctx<'_>, cmd: &str, value: &str) -> i32 {
    error(ctx.stderr, cmd, &format!("invalid number '{value}'"));
    1
}

/// Widest number column `nl` will pad to.
const MAX_NL_WIDTH: usize = 1024;

pub(super) fn nl(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut style = 't';
    let mut separator = String::from("\t");
    let mut width = 6usize;
    let mut start = 1i64;
    let mut increment = 1i64;
    let mut format_style = String::from("rn");

    let mut opts = Opts::new(args, "b:s:w:v:i:n:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('b', value) => style = value.chars().next().unwrap_or('t'),
            Opt::Value('s', value) => separator = value.to_string(),
            Opt::Value('w', value) => {
                width = parse_usize(value).unwrap_or(6).min(MAX_NL_WIDTH);
            }
            Opt::Value('v', value) => start = parse_i64(value).unwrap_or(1),
            Opt::Value('i', value) => increment = parse_i64(value).unwrap_or(1),
            Opt::Value('n', value) => format_style = value.to_string(),
            other => return bad_option(ctx, "nl", other),
        }
    }
    let mut files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let data = match read_paths(ctx, &files, "nl") {
        Some(data) => data,
        None => return 1,
    };
    let mut number = start;
    for line in split_lines(&data) {
        let raw = line.strip_suffix(b"\n").unwrap_or(line);
        let numbered = match style {
            'a' => true,
            'n' => false,
            _ => !raw.is_empty(),
        };
        let mut rendered = if numbered {
            let digits = format!("{number}");
            number = number.wrapping_add(increment);
            let fill = width.saturating_sub(digits.len());
            match format_style.as_str() {
                "ln" => digits.into_bytes().into_iter().chain(core::iter::repeat_n(b' ', fill)).collect(),
                "rz" => core::iter::repeat_n(b'0', fill).chain(digits.into_bytes()).collect(),
                _ => core::iter::repeat_n(b' ', fill).chain(digits.into_bytes()).collect::<Vec<u8>>(),
            }
        } else {
            alloc::vec![b' '; width]
        };
        rendered.extend_from_slice(separator.as_bytes());
        rendered.extend_from_slice(raw);
        rendered.push(b'\n');
        ctx.stdout.write(&rendered);
    }
    0
}

pub(super) fn rev(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let files = if args.is_empty() {
        alloc::vec![String::from("-")]
    } else {
        args.to_vec()
    };
    let data = match read_paths(ctx, &files, "rev") {
        Some(data) => data,
        None => return 1,
    };
    for line in split_lines(&data) {
        let raw = line.strip_suffix(b"\n").unwrap_or(line);
        let text = String::from_utf8_lossy(raw);
        let reversed: String = text.chars().rev().collect();
        write_line(ctx.stdout, format_args!("{reversed}"));
    }
    0
}

pub(super) fn tac(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let files = if args.is_empty() {
        alloc::vec![String::from("-")]
    } else {
        args.to_vec()
    };
    let data = match read_paths(ctx, &files, "tac") {
        Some(data) => data,
        None => return 1,
    };
    let lines = split_lines(&data);
    for line in lines.iter().rev() {
        ctx.stdout.write(line);
        if !line.ends_with(b"\n") {
            ctx.stdout.write(b"\n");
        }
    }
    0
}

pub(super) fn paste(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut delimiters: Vec<u8> = alloc::vec![b'\t'];
    let mut serial = false;
    let mut opts = Opts::new(args, "d:s");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('d', value) => delimiters = unescape(value).0,
            Opt::Flag('s') => serial = true,
            other => return bad_option(ctx, "paste", other),
        }
    }
    let mut files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let mut buffers: Vec<Vec<u8>> = Vec::new();
    for file in &files {
        match read_path(ctx, file) {
            Ok(data) => buffers.push(data),
            Err(err) => {
                path_error(ctx.stderr, "paste", file, err);
                return 1;
            }
        }
    }
    let inputs: Vec<Vec<&[u8]>> = buffers.iter().map(|data| split_lines(data)).collect();
    let body = |line: &'_ [u8]| -> usize { line.strip_suffix(b"\n").map_or(line.len(), <[u8]>::len) };
    let delimiter = |index: usize| -> Option<u8> {
        if delimiters.is_empty() {
            None
        } else {
            Some(delimiters[index % delimiters.len()])
        }
    };

    if serial {
        for lines in &inputs {
            let mut rendered = Vec::new();
            for (position, line) in lines.iter().enumerate() {
                if position > 0 {
                    rendered.extend(delimiter(position - 1));
                }
                rendered.extend_from_slice(&line[..body(line)]);
            }
            rendered.push(b'\n');
            ctx.stdout.write(&rendered);
        }
        return 0;
    }

    let max = inputs.iter().map(Vec::len).max().unwrap_or(0);
    for row in 0..max {
        let mut rendered = Vec::new();
        for (position, lines) in inputs.iter().enumerate() {
            if position > 0 {
                rendered.extend(delimiter(position - 1));
            }
            if let Some(line) = lines.get(row) {
                rendered.extend_from_slice(&line[..body(line)]);
            }
        }
        rendered.push(b'\n');
        ctx.stdout.write(&rendered);
    }
    0
}

pub(super) fn tee(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut append = false;
    let mut opts = Opts::new(args, "ai");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('a') => append = true,
            Opt::Flag('i') => {}
            other => return bad_option(ctx, "tee", other),
        }
    }
    let files = core::mem::take(&mut opts.operands);

    let data = match ctx.stdin.try_read_to_end(crate::io::MAX_STREAM_BYTES) {
        Ok(data) => data,
        Err(err) => {
            error(ctx.stderr, "tee", err.text());
            return 1;
        }
    };
    ctx.stdout.write(&data);
    let mut status = 0;
    for file in files {
        if !write_path(ctx, "tee", &file, &data, append) {
            status = 1;
        }
    }
    status
}

pub(super) fn cmp(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut silent = false;
    let mut verbose = false;
    let mut opts = Opts::new(args, "sl");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('s') => silent = true,
            Opt::Flag('l') => verbose = true,
            other => return bad_option(ctx, "cmp", other),
        }
    }
    let files = core::mem::take(&mut opts.operands);
    if files.len() != 2 {
        return usage(ctx.stderr, "cmp", "cmp [-sl] FILE1 FILE2");
    }

    let first = match read_path(ctx, &files[0]) {
        Ok(data) => data,
        Err(err) => {
            path_error(ctx.stderr, "cmp", &files[0], err);
            return 2;
        }
    };
    let second = match read_path(ctx, &files[1]) {
        Ok(data) => data,
        Err(err) => {
            path_error(ctx.stderr, "cmp", &files[1], err);
            return 2;
        }
    };

    let mut difference = false;
    let shared = first.len().min(second.len());
    for offset in 0..shared {
        let (left, right) = (first[offset], second[offset]);
        if left == right {
            continue;
        }
        difference = true;
        if silent {
            return 1;
        }
        if verbose {
            write_line(
                ctx.stdout,
                format_args!("{:>7} {left:03o} {right:03o}", offset + 1),
            );
        } else {
            let line = first[..offset].iter().filter(|byte| **byte == b'\n').count() + 1;
            write_line(
                ctx.stdout,
                format_args!(
                    "{} {} differ: byte {}, line {line}",
                    files[0],
                    files[1],
                    offset + 1
                ),
            );
            return 1;
        }
    }
    if first.len() != second.len() {
        if !silent {
            let shorter = if first.len() < second.len() {
                &files[0]
            } else {
                &files[1]
            };
            error(ctx.stderr, "cmp", &format!("EOF on {shorter}"));
        }
        return 1;
    }
    i32::from(difference)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(super) fn base64(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut decode = false;
    let mut wrap = 76usize;
    let mut opts = Opts::new(args, "dw:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('d') | Opt::Long("decode", None) => decode = true,
            Opt::Value('w', value) | Opt::Long("wrap", Some(value)) => {
                wrap = parse_usize(value).unwrap_or(76)
            }
            other => return bad_option(ctx, "base64", other),
        }
    }
    let mut files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let data = match read_paths(ctx, &files, "base64") {
        Some(data) => data,
        None => return 1,
    };

    if decode {
        match decode_base64(&data) {
            Some(decoded) => {
                ctx.stdout.write(&decoded);
                0
            }
            None => {
                error(ctx.stderr, "base64", "invalid input");
                1
            }
        }
    } else {
        let encoded = encode_base64(&data);
        if wrap == 0 {
            write_line(ctx.stdout, format_args!("{encoded}"));
        } else {
            for chunk in encoded.as_bytes().chunks(wrap) {
                ctx.stdout.write(chunk);
                ctx.stdout.write(b"\n");
            }
        }
        0
    }
}

fn encode_base64(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from).unwrap_or(0);
        let b2 = chunk.get(2).copied().map(u32::from).unwrap_or(0);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64[((triple >> 18) & 0x3f) as usize] as char);
        out.push(BASE64[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn decode_base64(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in data {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if *byte == b'=' {
            break;
        }
        let value = BASE64.iter().position(|candidate| candidate == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

pub(super) fn hexdump(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut canonical = false;
    let mut limit: Option<usize> = None;
    let mut skip = 0usize;
    let mut opts = Opts::new(args, "Cn:s:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('C') => canonical = true,
            Opt::Value('n', value) => limit = parse_usize(value),
            Opt::Value('s', value) => skip = parse_usize(value).unwrap_or(0),
            other => return bad_option(ctx, "hexdump", other),
        }
    }
    let mut files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let data = match read_paths(ctx, &files, "hexdump") {
        Some(data) => data,
        None => return 1,
    };
    let data = &data[skip.min(data.len())..];
    let data = &data[..data.len().min(limit.unwrap_or(data.len()))];

    for (row, chunk) in data.chunks(16).enumerate() {
        let offset = row * 16;
        if canonical {
            write_out(ctx.stdout, format_args!("{offset:08x}  "));
            for index in 0..16 {
                if let Some(byte) = chunk.get(index) {
                    write_out(ctx.stdout, format_args!("{byte:02x} "));
                } else {
                    write_out(ctx.stdout, format_args!("   "));
                }
                if index == 7 {
                    write_out(ctx.stdout, format_args!(" "));
                }
            }
            write_out(ctx.stdout, format_args!(" |"));
            for byte in chunk {
                let ch = if byte.is_ascii_graphic() || *byte == b' ' {
                    *byte as char
                } else {
                    '.'
                };
                write_out(ctx.stdout, format_args!("{ch}"));
            }
            write_line(ctx.stdout, format_args!("|"));
        } else {
            write_out(ctx.stdout, format_args!("{offset:07x} "));
            for byte in chunk {
                write_out(ctx.stdout, format_args!("{byte:02x} "));
            }
            write_line(ctx.stdout, format_args!(""));
        }
    }
    0
}

pub(super) fn sha256sum(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let files = if args.is_empty() {
        alloc::vec![String::from("-")]
    } else {
        args.to_vec()
    };
    let mut status = 0;
    for file in files {
        let data = match read_path(ctx, &file) {
            Ok(data) => data,
            Err(err) => {
                path_error(ctx.stderr, "sha256sum", &file, err);
                status = 1;
                continue;
            }
        };
        let digest = sha256::sha256(&data);
        write_line(ctx.stdout, format_args!("{}  {file}", sha256::hex(&digest)));
    }
    status
}

pub(super) fn yes(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let text = if args.is_empty() {
        String::from("y")
    } else {
        args.join(" ")
    };
    const LIMIT: usize = 10_000;
    let mut line = text.into_bytes();
    line.push(b'\n');

    let mut chunk = Vec::new();
    for _ in 0..LIMIT {
        chunk.extend_from_slice(&line);
        if chunk.len() >= 4096 {
            ctx.stdout.write(&chunk);
            chunk.clear();
            if ctx.cancelled() {
                return 130;
            }
        }
    }
    ctx.stdout.write(&chunk);

    if ctx.stdout.is_terminal() {
        error(ctx.stderr, "yes", "stopped after 10000 lines");
    }
    0
}

/// Reads and concatenates files, or returns `None` after reporting an error.
pub(super) fn read_paths(ctx: &mut Ctx<'_>, files: &[String], cmd: &str) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    let mut ok = true;
    for file in files {
        match read_path(ctx, file) {
            Ok(mut chunk) => {
                if data.len() + chunk.len() > crate::io::MAX_STREAM_BYTES
                    || data.try_reserve(chunk.len()).is_err()
                {
                    path_error(ctx.stderr, cmd, file, FsError::NoSpace);
                    return None;
                }
                data.append(&mut chunk);
            }
            Err(err) => {
                path_error(ctx.stderr, cmd, file, err);
                ok = false;
            }
        }
    }
    ok.then_some(data)
}
