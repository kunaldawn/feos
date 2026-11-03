//! Text-processing utilities.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{
    error, is_option, parse_i64, parse_usize, path_error, read_path, split_lines, text_lines,
    usage, write_line, write_out, write_path, Ctx,
};
use crate::fs::{FsError, Kind};
use crate::regex;
use crate::sha256;

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

pub(super) fn printf(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.is_empty() {
        return usage(ctx.stderr, "printf", "printf FORMAT [arg...]");
    }

    let (format_bytes, _) = unescape(&args[0]);
    let format = String::from_utf8_lossy(&format_bytes).into_owned();
    let bytes = format.as_bytes();
    let mut argument = 1usize;

    loop {
        let start = argument;
        let mut index = 0usize;

        while index < bytes.len() {
            if bytes[index] != b'%' {
                let next = bytes[index..]
                    .iter()
                    .position(|byte| *byte == b'%')
                    .map(|position| index + position)
                    .unwrap_or(bytes.len());
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

            let mut width: Option<usize> = None;
            if bytes.get(index) == Some(&b'*') {
                width = args
                    .get(argument)
                    .and_then(|value| parse_i64(value))
                    .map(|v| v.unsigned_abs() as usize);
                argument += 1;
                index += 1;
            } else {
                let start_width = index;
                while bytes
                    .get(index)
                    .map(|byte| byte.is_ascii_digit())
                    .unwrap_or(false)
                {
                    index += 1;
                }
                if index > start_width {
                    width = format[start_width..index].parse().ok();
                }
            }

            let mut precision: Option<usize> = None;
            if bytes.get(index) == Some(&b'.') {
                index += 1;
                if bytes.get(index) == Some(&b'*') {
                    precision = args
                        .get(argument)
                        .and_then(|value| parse_i64(value))
                        .map(|v| v.unsigned_abs() as usize);
                    argument += 1;
                    index += 1;
                } else {
                    let start_precision = index;
                    while bytes
                        .get(index)
                        .map(|byte| byte.is_ascii_digit())
                        .unwrap_or(false)
                    {
                        index += 1;
                    }
                    precision = Some(format[start_precision..index].parse().unwrap_or(0));
                }
            }

            let Some(&specifier) = bytes.get(index) else {
                ctx.stdout.write(b"%");
                break;
            };
            index += 1;

            let value = args.get(argument).cloned().unwrap_or_default();
            argument += 1;

            let mut rendered = match specifier {
                b's' => {
                    let mut text = value;
                    if let Some(limit) = precision {
                        text = text.chars().take(limit).collect();
                    }
                    text
                }
                b'b' => {
                    let (data, _) = unescape(&value);
                    String::from_utf8_lossy(&data).into_owned()
                }
                b'c' => value
                    .chars()
                    .next()
                    .map(|ch| ch.to_string())
                    .unwrap_or_default(),
                b'd' | b'i' => {
                    let number = parse_i64(&value).unwrap_or(0);
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
                    text
                }
                b'u' => format!("{}", value.parse::<u64>().unwrap_or(0)),
                b'x' => {
                    let number = value.parse::<u64>().unwrap_or(0);
                    if alternate {
                        format!("0x{number:x}")
                    } else {
                        format!("{number:x}")
                    }
                }
                b'X' => {
                    let number = value.parse::<u64>().unwrap_or(0);
                    if alternate {
                        format!("0X{number:X}")
                    } else {
                        format!("{number:X}")
                    }
                }
                b'o' => {
                    let number = value.parse::<u64>().unwrap_or(0);
                    if alternate {
                        format!("0{number:o}")
                    } else {
                        format!("{number:o}")
                    }
                }
                other => {
                    let mut text = String::from("%");
                    text.push(other as char);
                    text
                }
            };

            if let Some(width) = width {
                rendered = pad(&rendered, width, left, zero);
            }
            ctx.stdout.write(rendered.as_bytes());
        }

        if argument >= args.len() || argument == start {
            break;
        }
    }
    0
}

fn pad(text: &str, width: usize, left: bool, zero: bool) -> String {
    let length = text.chars().count();
    if length >= width {
        return text.to_string();
    }
    let fill = width - length;
    if left {
        let mut result = text.to_string();
        for _ in 0..fill {
            result.push(' ');
        }
        result
    } else if zero && !text.starts_with('-') && !text.starts_with('+') {
        let mut result = String::new();
        for _ in 0..fill {
            result.push('0');
        }
        result.push_str(text);
        result
    } else {
        let mut result = String::new();
        for _ in 0..fill {
            result.push(' ');
        }
        result.push_str(text);
        result
    }
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

#[derive(Clone, Copy)]
struct SliceOptions {
    lines: Option<usize>,
    bytes: Option<usize>,
    quiet: bool,
    verbose: bool,
}

fn parse_slice_options(
    args: &[String],
    cmd: &str,
    ctx: &mut Ctx<'_>,
) -> Result<(SliceOptions, Vec<String>), i32> {
    let mut options = SliceOptions {
        lines: Some(10),
        bytes: None,
        quiet: false,
        verbose: false,
    };
    let mut files = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        if arg == "-n" || arg == "-c" {
            let Some(value) = args.get(index + 1) else {
                return Err(usage(
                    ctx.stderr,
                    cmd,
                    &format!("{cmd} [-n N|-c N] [file...]"),
                ));
            };
            let count = parse_usize(value).unwrap_or(10);
            if arg == "-n" {
                options.lines = Some(count);
                options.bytes = None;
            } else {
                options.bytes = Some(count);
                options.lines = None;
            }
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("-n") {
            if !value.is_empty() {
                options.lines = Some(parse_usize(value).unwrap_or(10));
                options.bytes = None;
                index += 1;
                continue;
            }
        }
        if let Some(value) = arg.strip_prefix("-c") {
            if !value.is_empty() {
                options.bytes = Some(parse_usize(value).unwrap_or(10));
                options.lines = None;
                index += 1;
                continue;
            }
        }
        if is_option(arg) {
            for flag in arg[1..].chars() {
                match flag {
                    'q' => options.quiet = true,
                    'v' => options.verbose = true,
                    other => {
                        error(ctx.stderr, cmd, &format!("invalid option -- '{other}'"));
                        return Err(2);
                    }
                }
            }
        } else {
            files.push(arg.clone());
        }
        index += 1;
    }

    if files.is_empty() {
        files.push(String::from("-"));
    }
    Ok((options, files))
}

pub(super) fn head(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let (options, files) = match parse_slice_options(args, "head", ctx) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    run_slice(ctx, &files, options, true)
}

pub(super) fn tail(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let (options, files) = match parse_slice_options(args, "tail", ctx) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    run_slice(ctx, &files, options, false)
}

fn run_slice(ctx: &mut Ctx<'_>, files: &[String], options: SliceOptions, from_start: bool) -> i32 {
    let mut status = 0;
    let multiple = files.len() > 1;
    let show_headers = options.verbose || (multiple && !options.quiet);

    for (index, file) in files.iter().enumerate() {
        let data = match read_path(ctx, file) {
            Ok(data) => data,
            Err(err) => {
                path_error(
                    ctx.stderr,
                    if from_start { "head" } else { "tail" },
                    file,
                    err,
                );
                status = 1;
                continue;
            }
        };

        if show_headers {
            if index > 0 {
                write_line(ctx.stdout, format_args!(""));
            }
            write_line(ctx.stdout, format_args!("==> {file} <=="));
        }

        let slice = if let Some(count) = options.bytes {
            if from_start {
                &data[..data.len().min(count)]
            } else {
                &data[data.len().saturating_sub(count)..]
            }
        } else {
            let count = options.lines.unwrap_or(10);
            let lines = split_lines(&data);
            if from_start {
                let mut selected = Vec::new();
                for line in lines.into_iter().take(count) {
                    selected.extend_from_slice(line);
                }
                ctx.stdout.write(&selected);
                continue;
            } else {
                let start = lines.len().saturating_sub(count);
                let mut selected = Vec::new();
                for line in &lines[start..] {
                    selected.extend_from_slice(line);
                }
                ctx.stdout.write(&selected);
                continue;
            }
        };
        ctx.stdout.write(slice);
    }
    status
}

pub(super) fn wc(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut lines_flag = false;
    let mut words_flag = false;
    let mut bytes_flag = false;
    let mut chars_flag = false;
    let mut files: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    'l' => lines_flag = true,
                    'w' => words_flag = true,
                    'c' => bytes_flag = true,
                    'm' => chars_flag = true,
                    other => {
                        error(ctx.stderr, "wc", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            files.push(arg.clone());
        }
    }
    if !(lines_flag || words_flag || bytes_flag || chars_flag) {
        lines_flag = true;
        words_flag = true;
        bytes_flag = true;
    }
    if files.is_empty() {
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
            Some(file),
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
    ignore_case: bool,
    invert: bool,
    line_number: bool,
    count: bool,
    files_with_matches: bool,
    files_without_matches: bool,
    quiet: bool,
    word: bool,
    whole_line: bool,
    fixed: bool,
    recursive: bool,
}

pub(super) fn grep(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut options = GrepOptions {
        ignore_case: false,
        invert: false,
        line_number: false,
        count: false,
        files_with_matches: false,
        files_without_matches: false,
        quiet: false,
        word: false,
        whole_line: false,
        fixed: false,
        recursive: false,
    };
    let mut patterns: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        if arg == "-e" {
            index += 1;
            if let Some(pattern) = args.get(index) {
                patterns.push(pattern.clone());
            }
        } else if is_option(arg) && arg != "-" {
            if arg.starts_with("--") {
                match arg.as_str() {
                    "--ignore-case" => options.ignore_case = true,
                    "--invert-match" => options.invert = true,
                    "--line-number" => options.line_number = true,
                    "--count" => options.count = true,
                    "--fixed-strings" => options.fixed = true,
                    "--recursive" => options.recursive = true,
                    "--word-regexp" => options.word = true,
                    "--line-regexp" => options.whole_line = true,
                    "--quiet" | "--silent" => options.quiet = true,
                    _ => {
                        error(ctx.stderr, "grep", &format!("unknown option '{arg}'"));
                        return 2;
                    }
                }
            } else {
                for flag in arg[1..].chars() {
                    match flag {
                        'i' => options.ignore_case = true,
                        'v' => options.invert = true,
                        'n' => options.line_number = true,
                        'c' => options.count = true,
                        'l' => options.files_with_matches = true,
                        'L' => options.files_without_matches = true,
                        'q' => options.quiet = true,
                        'w' => options.word = true,
                        'x' => options.whole_line = true,
                        'F' => options.fixed = true,
                        'E' | 'G' => {}
                        'r' | 'R' => options.recursive = true,
                        other => {
                            error(ctx.stderr, "grep", &format!("invalid option -- '{other}'"));
                            return 2;
                        }
                    }
                }
            }
        } else if patterns.is_empty() {
            patterns.push(arg.clone());
        } else {
            files.push(arg.clone());
        }
        index += 1;
    }

    if patterns.is_empty() {
        return usage(ctx.stderr, "grep", "grep [-ivnclLqwxFr] PATTERN [file...]");
    }
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let mut matched = false;
    let mut status = 0;
    let multiple = files.len() > 1;

    for file in files {
        if options.recursive && file != "-" && ctx.fs.is_dir(&file) {
            if let Err(err) = grep_tree(ctx, &file, &patterns, &options, &mut matched, &mut status)
            {
                path_error(ctx.stderr, "grep", &file, err);
                status = 2;
            }
        } else {
            match grep_file(ctx, &file, &patterns, &options, multiple, &mut matched) {
                Ok(()) => {}
                Err(err) => {
                    path_error(ctx.stderr, "grep", &file, err);
                    status = 2;
                }
            }
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
    patterns: &[String],
    options: &GrepOptions,
    matched: &mut bool,
    status: &mut i32,
) -> Result<(), FsError> {
    let entries = ctx.fs.read_dir(path)?;
    for entry in entries {
        let child = crate::fs::join(path, &entry.name);
        if entry.kind == Kind::Dir {
            grep_tree(ctx, &child, patterns, options, matched, status)?;
        } else {
            if let Err(err) = grep_file(ctx, &child, patterns, options, true, matched) {
                path_error(ctx.stderr, "grep", &child, err);
                *status = 2;
            }
        }
    }
    Ok(())
}

fn grep_file(
    ctx: &mut Ctx<'_>,
    file: &str,
    patterns: &[String],
    options: &GrepOptions,
    multiple: bool,
    matched_any: &mut bool,
) -> Result<(), FsError> {
    let data = read_path(ctx, file)?;
    let lines = split_lines(&data);
    let mut count = 0usize;
    let mut file_matched = false;
    let show_name = multiple && file != "-";

    for (index, line) in lines.iter().enumerate() {
        let raw = line.strip_suffix(b"\n").unwrap_or(line);
        let mut text = String::from_utf8_lossy(raw).into_owned();
        if options.ignore_case {
            text = text.to_ascii_lowercase();
        }

        let mut is_match = false;
        for pattern in patterns {
            let pattern = if options.ignore_case {
                pattern.to_ascii_lowercase()
            } else {
                pattern.clone()
            };
            let candidate = if options.fixed {
                text.contains(&pattern)
            } else {
                regex::is_match(&pattern, &text)
            };
            let candidate = if options.whole_line {
                if options.fixed {
                    text == pattern
                } else {
                    regex::is_match(&format!("^{pattern}$"), &text)
                }
            } else {
                candidate
            };
            let candidate = if options.word {
                candidate && has_word_match(&text, &pattern, options.fixed)
            } else {
                candidate
            };
            if candidate {
                is_match = true;
                break;
            }
        }

        if is_match != options.invert {
            file_matched = true;
            count += 1;
            *matched_any = true;
            if options.quiet {
                break;
            }
            if !options.count && !options.files_with_matches && !options.files_without_matches {
                if show_name {
                    write_out(ctx.stdout, format_args!("{file}:"));
                }
                if options.line_number {
                    write_out(ctx.stdout, format_args!("{}:", index + 1));
                }
                ctx.stdout.write(raw);
                ctx.stdout.write(b"\n");
            }
        }
    }

    if options.count {
        if show_name {
            write_line(ctx.stdout, format_args!("{file}:{count}"));
        } else {
            write_line(ctx.stdout, format_args!("{count}"));
        }
    } else if options.files_with_matches && file_matched {
        write_line(ctx.stdout, format_args!("{file}"));
    } else if options.files_without_matches && !file_matched {
        write_line(ctx.stdout, format_args!("{file}"));
    }

    Ok(())
}

fn has_word_match(text: &str, pattern: &str, fixed: bool) -> bool {
    let bytes = text.as_bytes();
    for start in 0..=bytes.len() {
        if start > 0 && is_word_byte(bytes[start - 1]) {
            continue;
        }
        for end in (start..=bytes.len()).rev() {
            if end < bytes.len() && is_word_byte(bytes[end]) {
                continue;
            }
            let slice = &text[start..end];
            let matches = if fixed {
                slice == pattern
            } else {
                regex::is_match(&format!("^{pattern}$"), slice)
            };
            if matches {
                return true;
            }
        }
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[derive(Clone)]
struct SortOptions {
    reverse: bool,
    numeric: bool,
    unique: bool,
    fold: bool,
    ignore_blanks: bool,
    delimiter: Option<char>,
    key: Option<(usize, usize)>,
}

pub(super) fn sort(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut options = SortOptions {
        reverse: false,
        numeric: false,
        unique: false,
        fold: false,
        ignore_blanks: false,
        delimiter: None,
        key: None,
    };
    let mut files: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-t" => {
                index += 1;
                options.delimiter = args.get(index).and_then(|value| value.chars().next());
            }
            "-k" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    options.key = parse_key(value);
                }
            }
            _ if is_option(arg) && arg != "-" => {
                for flag in arg[1..].chars() {
                    match flag {
                        'r' => options.reverse = true,
                        'n' => options.numeric = true,
                        'u' => options.unique = true,
                        'f' => options.fold = true,
                        'b' => options.ignore_blanks = true,
                        other => {
                            error(ctx.stderr, "sort", &format!("invalid option -- '{other}'"));
                            return 2;
                        }
                    }
                }
            }
            _ => files.push(arg.clone()),
        }
        index += 1;
    }

    if files.is_empty() {
        files.push(String::from("-"));
    }
    let mut lines = read_lines(ctx, &files, "sort");
    if lines.is_empty() {
        return 0;
    }

    lines.sort_by(|left, right| {
        let left_key = sort_key(left, &options);
        let right_key = sort_key(right, &options);
        let ordering = if options.numeric {
            parse_number(&left_key)
                .partial_cmp(&parse_number(&right_key))
                .unwrap_or(core::cmp::Ordering::Equal)
        } else {
            left_key.cmp(&right_key)
        };
        if options.reverse {
            ordering.reverse()
        } else {
            ordering
        }
    });

    if options.unique {
        let mut previous: Option<String> = None;
        for line in &lines {
            let key = sort_key(line, &options);
            if previous.as_deref() == Some(key.as_str()) {
                continue;
            }
            previous = Some(key);
            write_line(ctx.stdout, format_args!("{line}"));
        }
    } else {
        for line in &lines {
            write_line(ctx.stdout, format_args!("{line}"));
        }
    }
    0
}

fn parse_key(value: &str) -> Option<(usize, usize)> {
    let value = value.split(',').next().unwrap_or(value);
    let start = value.split('.').next().unwrap_or(value).parse().ok()?;
    let end = value
        .split(',')
        .nth(1)
        .and_then(|part| part.split('.').next())
        .and_then(|part| part.parse().ok())
        .unwrap_or(start);
    Some((start, end))
}

fn sort_key(line: &str, options: &SortOptions) -> String {
    let mut text = if let Some((start, end)) = options.key {
        let fields: Vec<&str> = match options.delimiter {
            Some(delimiter) => line.split(delimiter).collect(),
            None => line.split_whitespace().collect(),
        };
        let from = start.saturating_sub(1).min(fields.len());
        let to = end.min(fields.len());
        fields[from..to].join(" ")
    } else {
        line.to_string()
    };

    if options.ignore_blanks {
        text = text.trim_start().to_string();
    }
    if options.fold {
        text = text.to_ascii_lowercase();
    }
    text
}

fn parse_number(text: &str) -> f64 {
    let text = text.trim_start();
    let mut end = 0usize;
    let bytes = text.as_bytes();
    if bytes.first() == Some(&b'-') || bytes.first() == Some(&b'+') {
        end = 1;
    }
    let mut seen_dot = false;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => {}
            b'.' if !seen_dot => seen_dot = true,
            _ => break,
        }
        end += 1;
    }
    text[..end].parse().unwrap_or(0.0)
}

pub(super) fn uniq(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut count = false;
    let mut duplicates = false;
    let mut unique_only = false;
    let mut ignore_case = false;
    let mut files: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    'c' => count = true,
                    'd' => duplicates = true,
                    'u' => unique_only = true,
                    'i' => ignore_case = true,
                    other => {
                        error(ctx.stderr, "uniq", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            files.push(arg.clone());
        }
    }
    if files.len() > 1 {
        return usage(ctx.stderr, "uniq", "uniq [-cdui] [file]");
    }
    let files = if files.is_empty() {
        alloc::vec![String::from("-")]
    } else {
        files
    };

    let lines = read_lines(ctx, &files, "uniq");
    let mut index = 0usize;
    while index < lines.len() {
        let key = normalize_key(&lines[index], ignore_case);
        let mut end = index + 1;
        while end < lines.len() && normalize_key(&lines[end], ignore_case) == key {
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
                write_out(ctx.stdout, format_args!("{repeats:>7} "));
            }
            write_line(ctx.stdout, format_args!("{}", lines[index]));
        }
        index = end;
    }
    0
}

fn normalize_key(text: &str, ignore_case: bool) -> String {
    if ignore_case {
        text.to_ascii_lowercase()
    } else {
        text.to_string()
    }
}

pub(super) fn cut(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut delimiter = '\t';
    let mut fields: Option<Vec<(usize, usize)>> = None;
    let mut characters: Option<Vec<(usize, usize)>> = None;
    let mut suppress = false;
    let mut files: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-d" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    delimiter = value.chars().next().unwrap_or('\t');
                }
            }
            "-f" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    fields = parse_list(value);
                }
            }
            "-c" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    characters = parse_list(value);
                }
            }
            "-s" => suppress = true,
            _ if is_option(arg) && arg != "-" => {
                error(ctx.stderr, "cut", &format!("invalid option '{arg}'"));
                return 2;
            }
            _ => files.push(arg.clone()),
        }
        index += 1;
    }

    if fields.is_none() && characters.is_none() {
        return usage(ctx.stderr, "cut", "cut -d C -f LIST [file...]");
    }
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let data = match read_paths(ctx, &files, "cut") {
        Some(data) => data,
        None => return 1,
    };

    for line in split_lines(&data) {
        let raw = line.strip_suffix(b"\n").unwrap_or(line);
        let text = String::from_utf8_lossy(raw);
        let mut rendered = Vec::new();

        if let Some(characters) = &characters {
            for (start, end) in characters {
                let low = start.saturating_sub(1);
                let high = (*end).min(text.chars().count());
                if low < high {
                    rendered.extend(
                        text.chars()
                            .skip(low)
                            .take(high - low)
                            .collect::<String>()
                            .into_bytes(),
                    );
                }
            }
        } else if let Some(fields_list) = &fields {
            let parts: Vec<&str> = text.split(delimiter).collect();
            if parts.len() == 1 && !text.contains(delimiter) && suppress {
                continue;
            }
            let mut selected: Vec<&str> = Vec::new();
            for (start, end) in fields_list {
                let low = start.saturating_sub(1).min(parts.len());
                let high = (*end).min(parts.len());
                if low < high {
                    selected.extend_from_slice(&parts[low..high]);
                }
            }
            let mut joined = String::new();
            for (position, part) in selected.iter().enumerate() {
                if position > 0 {
                    joined.push(delimiter);
                }
                joined.push_str(part);
            }
            rendered = joined.into_bytes();
        }

        ctx.stdout.write(&rendered);
        ctx.stdout.write(b"\n");
    }
    0
}

fn parse_list(text: &str) -> Option<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    for part in text.split(',') {
        if let Some((start, end)) = part.split_once('-') {
            let start = parse_usize(start)?;
            let end = if end.is_empty() {
                usize::MAX
            } else {
                parse_usize(end)?
            };
            ranges.push((start.max(1), end));
        } else {
            let value = parse_usize(part)?;
            ranges.push((value, value));
        }
    }
    Some(ranges)
}

pub(super) fn tr(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut delete = false;
    let mut squeeze = false;
    let mut complement = false;
    let mut sets: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    'd' => delete = true,
                    's' => squeeze = true,
                    'c' | 'C' => complement = true,
                    other => {
                        error(ctx.stderr, "tr", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            sets.push(arg.clone());
        }
    }
    if sets.is_empty() {
        return usage(ctx.stderr, "tr", "tr [-dsc] SET1 [SET2]");
    }

    let mut set1 = expand_set(&sets[0]);
    if complement {
        let mut complemented = Vec::new();
        for byte in 0u16..=255 {
            let byte = byte as u8;
            if !set1.contains(&byte) {
                complemented.push(byte);
            }
        }
        set1 = complemented;
    }
    let set2 = sets.get(1).map(|set| expand_set(set)).unwrap_or_default();

    let mut table = [0u8; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        *slot = index as u8;
    }
    if !delete {
        if !set2.is_empty() {
            for (index, byte) in set1.iter().enumerate() {
                let replacement = set2.get(index).copied().unwrap_or(*set2.last().unwrap());
                table[*byte as usize] = replacement;
            }
        }
    }

    let data = ctx.stdin.read_to_end();
    let mut output = Vec::with_capacity(data.len());
    let mut previous: Option<u8> = None;
    let squeeze_set: &[u8] = if delete {
        &set1
    } else if set2.is_empty() {
        &set1
    } else {
        &set2
    };
    for byte in data {
        if delete && set1.contains(&byte) {
            continue;
        }
        let mapped = table[byte as usize];
        if squeeze && previous == Some(mapped) && squeeze_set.contains(&mapped) {
            continue;
        }
        output.push(mapped);
        previous = Some(mapped);
    }
    ctx.stdout.write(&output);
    0
}

fn expand_set(set: &str) -> Vec<u8> {
    let (bytes, _) = unescape(set);
    let mut result = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
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
    let mut numbers: Vec<String> = Vec::new();

    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-s" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    separator = value.clone();
                }
            }
            "-w" => equal_width = true,
            _ if is_option(arg) && arg != "-" => {
                error(ctx.stderr, "seq", &format!("invalid option '{arg}'"));
                return 2;
            }
            _ => numbers.push(arg.clone()),
        }
        index += 1;
    }

    let (first, step, last) = match numbers.len() {
        1 => {
            let last = match parse_i64(&numbers[0]) {
                Some(value) => value,
                None => return invalid_number(ctx, "seq", &numbers[0]),
            };
            (1, 1, last)
        }
        2 => {
            let first = match parse_i64(&numbers[0]) {
                Some(value) => value,
                None => return invalid_number(ctx, "seq", &numbers[0]),
            };
            let last = match parse_i64(&numbers[1]) {
                Some(value) => value,
                None => return invalid_number(ctx, "seq", &numbers[1]),
            };
            (first, 1, last)
        }
        3 => {
            let first = match parse_i64(&numbers[0]) {
                Some(value) => value,
                None => return invalid_number(ctx, "seq", &numbers[0]),
            };
            let step = match parse_i64(&numbers[1]) {
                Some(value) => value,
                None => return invalid_number(ctx, "seq", &numbers[1]),
            };
            let last = match parse_i64(&numbers[2]) {
                Some(value) => value,
                None => return invalid_number(ctx, "seq", &numbers[2]),
            };
            (first, step, last)
        }
        _ => return usage(ctx.stderr, "seq", "seq [-w] [-s SEP] [first [step]] last"),
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
    ctx.stdout.write(b"\n");
    0
}

fn invalid_number(ctx: &mut Ctx<'_>, cmd: &str, value: &str) -> i32 {
    error(ctx.stderr, cmd, &format!("invalid number '{value}'"));
    1
}

pub(super) fn nl(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut number_all = false;
    let mut separator = String::from("\t");
    let mut width = 6usize;
    let mut start = 1i64;
    let mut increment = 1i64;
    let mut files: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-b" => {
                index += 1;
                number_all = args.get(index).map(|value| value == "a").unwrap_or(false);
            }
            "-s" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    separator = value.clone();
                }
            }
            "-w" => {
                index += 1;
                width = args
                    .get(index)
                    .and_then(|value| parse_usize(value))
                    .unwrap_or(6);
            }
            "-v" => {
                index += 1;
                start = args
                    .get(index)
                    .and_then(|value| parse_i64(value))
                    .unwrap_or(1);
            }
            "-i" => {
                index += 1;
                increment = args
                    .get(index)
                    .and_then(|value| parse_i64(value))
                    .unwrap_or(1);
            }
            _ if is_option(arg) && arg != "-" => {
                error(ctx.stderr, "nl", &format!("invalid option '{arg}'"));
                return 2;
            }
            _ => files.push(arg.clone()),
        }
        index += 1;
    }
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
        let text = String::from_utf8_lossy(raw);
        if number_all || !text.is_empty() {
            write_out(
                ctx.stdout,
                format_args!("{number:>width$}{separator}{text}\n"),
            );
            number += increment;
        } else {
            write_out(ctx.stdout, format_args!("{:width$}{separator}\n", ""));
        }
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
    let mut delimiters = String::from("\t");
    let mut serial = false;
    let mut files: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-d" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    delimiters = value.clone();
                }
            }
            "-s" => serial = true,
            _ if is_option(arg) && arg != "-" => {
                error(ctx.stderr, "paste", &format!("invalid option '{arg}'"));
                return 2;
            }
            _ => files.push(arg.clone()),
        }
        index += 1;
    }
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let mut inputs: Vec<Vec<String>> = Vec::new();
    for file in &files {
        let data = match read_path(ctx, file) {
            Ok(data) => data,
            Err(err) => {
                path_error(ctx.stderr, "paste", file, err);
                return 1;
            }
        };
        inputs.push(text_lines(&data));
    }

    if serial {
        for lines in &inputs {
            write_line(
                ctx.stdout,
                format_args!("{}", join_cyclic(lines, &delimiters)),
            );
        }
        return 0;
    }

    let max = inputs.iter().map(Vec::len).max().unwrap_or(0);
    for row in 0..max {
        let mut parts: Vec<String> = Vec::new();
        for lines in &inputs {
            parts.push(lines.get(row).cloned().unwrap_or_default());
        }
        let mut line = String::new();
        for (position, part) in parts.iter().enumerate() {
            if position > 0 {
                line.push(delimiter_at(&delimiters, position - 1));
            }
            line.push_str(part);
        }
        write_line(ctx.stdout, format_args!("{line}"));
    }
    0
}

fn join_cyclic(parts: &[String], delimiters: &str) -> String {
    let mut line = String::new();
    for (position, part) in parts.iter().enumerate() {
        if position > 0 {
            line.push(delimiter_at(delimiters, position - 1));
        }
        line.push_str(part);
    }
    line
}

fn delimiter_at(delimiters: &str, index: usize) -> char {
    let chars: Vec<char> = delimiters.chars().collect();
    if chars.is_empty() {
        '\t'
    } else {
        chars[index % chars.len()]
    }
}

pub(super) fn tee(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut append = false;
    let mut files: Vec<String> = Vec::new();
    for arg in args {
        if arg == "-a" {
            append = true;
        } else if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    'a' => append = true,
                    'i' => {}
                    other => {
                        error(ctx.stderr, "tee", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            files.push(arg.clone());
        }
    }

    let data = ctx.stdin.read_to_end();
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
    let mut files: Vec<String> = Vec::new();
    for arg in args {
        if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    's' => silent = true,
                    'l' => verbose = true,
                    other => {
                        error(ctx.stderr, "cmp", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            files.push(arg.clone());
        }
    }
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
    let limit = first.len().max(second.len());
    for offset in 0..limit {
        let left = first.get(offset);
        let right = second.get(offset);
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
                format_args!(
                    "{:>7} {:03o} {:03o}",
                    offset + 1,
                    left.copied().unwrap_or(0),
                    right.copied().unwrap_or(0)
                ),
            );
        } else {
            write_line(
                ctx.stdout,
                format_args!(
                    "{} {} differ: byte {}, line {}",
                    files[0],
                    files[1],
                    offset + 1,
                    first[..offset.min(first.len())]
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count()
                        + 1
                ),
            );
            return 1;
        }
    }
    if difference {
        1
    } else {
        0
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(super) fn base64(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut decode = false;
    let mut wrap = 76usize;
    let mut files: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-d" | "--decode" => decode = true,
            "-w" => {
                index += 1;
                wrap = args
                    .get(index)
                    .and_then(|value| parse_usize(value))
                    .unwrap_or(76);
            }
            _ if is_option(arg) && arg != "-" => {
                error(ctx.stderr, "base64", &format!("invalid option '{arg}'"));
                return 2;
            }
            _ => files.push(arg.clone()),
        }
        index += 1;
    }
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
    let mut files: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-C" => canonical = true,
            "-n" => {
                index += 1;
                limit = args.get(index).and_then(|value| parse_usize(value));
            }
            "-s" => {
                index += 1;
                skip = args
                    .get(index)
                    .and_then(|value| parse_usize(value))
                    .unwrap_or(0);
            }
            _ if is_option(arg) && arg != "-" => {
                error(ctx.stderr, "hexdump", &format!("invalid option '{arg}'"));
                return 2;
            }
            _ => files.push(arg.clone()),
        }
        index += 1;
    }
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
        }
    }
    ctx.stdout.write(&chunk);

    if ctx.stdout.is_terminal() {
        error(
            ctx.stderr,
            "yes",
            "stopped after 10000 lines (feos has no signals)",
        );
    }
    0
}

fn read_lines(ctx: &mut Ctx<'_>, files: &[String], cmd: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for file in files {
        match read_path(ctx, file) {
            Ok(data) => lines.extend(text_lines(&data)),
            Err(err) => path_error(ctx.stderr, cmd, file, err),
        }
    }
    lines
}

fn read_paths(ctx: &mut Ctx<'_>, files: &[String], cmd: &str) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    let mut status = true;
    for file in files {
        match read_path(ctx, file) {
            Ok(mut chunk) => data.append(&mut chunk),
            Err(err) => {
                path_error(ctx.stderr, cmd, file, err);
                status = false;
            }
        }
    }
    if status {
        Some(data)
    } else {
        None
    }
}
