//! Small everyday utilities.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::opts::{bad_option, Opt, Opts};
use super::text::read_paths;
use super::{
    error, parse_usize, path_error, read_path, split_lines, usage, write_line, write_out, Ctx,
};
use crate::fs::{FsError, Kind};
use crate::io::MAX_STREAM_BYTES;

fn operands_or_stdin(operands: &mut Vec<String>) -> Vec<String> {
    let mut files = core::mem::take(operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }
    files
}

/// Parses a size with an optional `k`, `M` or `G` suffix.
fn parse_size(text: &str) -> Option<usize> {
    let digits = text.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let scale = match unit {
        "" | "c" => 1,
        "w" => 2,
        "b" => 512,
        "k" | "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        _ => return None,
    };
    number.parse::<usize>().ok()?.checked_mul(scale)
}

pub(super) fn basename(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut suffix: Option<String> = None;
    let mut multiple = false;
    let mut opts = Opts::new(args, "as:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('a') => multiple = true,
            Opt::Value('s', value) => {
                multiple = true;
                suffix = Some(value.to_string());
            }
            other => return bad_option(ctx, "basename", other),
        }
    }
    let operands = core::mem::take(&mut opts.operands);
    if operands.is_empty() || (!multiple && operands.len() > 2) {
        return usage(ctx.stderr, "basename", "basename PATH [SUFFIX]");
    }
    if !multiple {
        suffix = operands.get(1).cloned();
    }
    let names = if multiple {
        &operands[..]
    } else {
        &operands[..1]
    };
    for path in names {
        let trimmed = path.trim_end_matches('/');
        let mut base = if trimmed.is_empty() {
            if path.is_empty() { "" } else { "/" }
        } else {
            trimmed.rsplit('/').next().unwrap_or(trimmed)
        };
        if let Some(suffix) = &suffix {
            if base != suffix {
                base = base.strip_suffix(suffix.as_str()).unwrap_or(base);
            }
        }
        write_line(ctx.stdout, format_args!("{base}"));
    }
    0
}

pub(super) fn dirname(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.is_empty() {
        return usage(ctx.stderr, "dirname", "dirname PATH...");
    }
    for path in args {
        let trimmed = path.trim_end_matches('/');
        let dir = if trimmed.is_empty() {
            if path.is_empty() { "." } else { "/" }
        } else {
            match trimmed.rfind('/') {
                None => ".",
                Some(index) => {
                    let parent = trimmed[..index].trim_end_matches('/');
                    if parent.is_empty() { "/" } else { parent }
                }
            }
        };
        write_line(ctx.stdout, format_args!("{dir}"));
    }
    0
}

pub(super) fn xargs(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut max_args: Option<usize> = None;
    let mut replace: Option<String> = None;
    let mut trace = false;
    let mut no_run_empty = false;
    let mut delimiter: Option<u8> = None;
    let mut opts = Opts::new(args, "n:I:0d:trL:").stop_at_operand();
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('n' | 'L', value) => max_args = parse_usize(value).filter(|n| *n > 0),
            Opt::Value('I', value) => replace = Some(value.to_string()),
            Opt::Flag('0') => delimiter = Some(0),
            Opt::Value('d', value) => delimiter = value.bytes().next(),
            Opt::Flag('t') => trace = true,
            Opt::Flag('r') => no_run_empty = true,
            other => return bad_option(ctx, "xargs", other),
        }
    }

    let echo = [String::from("echo")];
    let command = if opts.rest().is_empty() {
        &echo[..]
    } else {
        opts.rest()
    };
    let (name, fixed) = match command.split_first() {
        Some(parts) => parts,
        None => return 1,
    };
    if !super::is_command(name) {
        error(ctx.stderr, "xargs", &format!("{name}: command not found"));
        return 127;
    }

    let data = match ctx.stdin.try_read_to_end(MAX_STREAM_BYTES) {
        Ok(data) => data,
        Err(err) => {
            error(ctx.stderr, "xargs", err.text());
            return 1;
        }
    };
    let items = split_items(&data, delimiter);
    if items.is_empty() && (no_run_empty || replace.is_some()) {
        return 0;
    }

    // The command gets an empty standard input, as with `xargs </dev/null`.
    let mut empty = crate::io::NullInput;
    let mut run_one = |argv: &[String], ctx: &mut Ctx<'_>| -> i32 {
        if trace {
            let line = format!("{name} {}", argv.join(" "));
            write_line(ctx.stderr, format_args!("{}", line.trim_end()));
        }
        let mut inner = Ctx {
            fs: &mut *ctx.fs,
            env: &mut *ctx.env,
            stdin: &mut empty,
            stdout: &mut *ctx.stdout,
            stderr: &mut *ctx.stderr,
            system: ctx.system,
            depth: ctx.depth,
        };
        super::run_nested(name, argv, &mut inner)
    };

    let mut status = 0;
    if let Some(token) = &replace {
        for item in &items {
            let argv: Vec<String> = fixed.iter().map(|arg| arg.replace(token.as_str(), item)).collect();
            if run_one(&argv, ctx) != 0 {
                status = 123;
            }
            if ctx.cancelled() {
                break;
            }
        }
        return status;
    }

    let batch = max_args.unwrap_or(items.len().max(1));
    let mut position = 0usize;
    loop {
        let end = (position + batch).min(items.len());
        let mut argv = fixed.to_vec();
        argv.extend_from_slice(&items[position..end]);
        let result = run_one(&argv, ctx);
        if result == 126 || result == 127 {
            return result;
        }
        if result != 0 {
            status = 123;
        }
        position = end;
        if position >= items.len() || ctx.cancelled() {
            return status;
        }
    }
}

/// Splits `xargs` input: on the delimiter, or on blanks with shell-like quotes.
fn split_items(data: &[u8], delimiter: Option<u8>) -> Vec<String> {
    let mut items = Vec::new();
    if let Some(byte) = delimiter {
        for part in data.split(|candidate| *candidate == byte) {
            items.push(String::from_utf8_lossy(part).into_owned());
        }
        if items.last().is_some_and(String::is_empty) {
            items.pop();
        }
        return items;
    }

    let text = String::from_utf8_lossy(data);
    let mut current = String::new();
    let mut active = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                active = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                active = true;
            }
            ' ' | '\t' | '\n' => {
                if active {
                    items.push(core::mem::take(&mut current));
                    active = false;
                }
            }
            other => {
                current.push(other);
                active = true;
            }
        }
    }
    if active {
        items.push(current);
    }
    items
}

pub(super) fn od(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut radix = 'o';
    let mut format_kind = String::from("o2");
    let mut limit: Option<usize> = None;
    let mut skip = 0usize;
    let mut opts = Opts::new(args, "A:t:N:j:cxbdo");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('A', value) => radix = value.chars().next().unwrap_or('o'),
            Opt::Value('t', value) => format_kind = value.to_string(),
            Opt::Value('N', value) => limit = parse_size(value),
            Opt::Value('j', value) => skip = parse_size(value).unwrap_or(0),
            Opt::Flag('c') => format_kind = String::from("c"),
            Opt::Flag('x') => format_kind = String::from("x2"),
            Opt::Flag('b') => format_kind = String::from("o1"),
            Opt::Flag('d') => format_kind = String::from("u2"),
            Opt::Flag('o') => format_kind = String::from("o2"),
            other => return bad_option(ctx, "od", other),
        }
    }
    let files = operands_or_stdin(&mut opts.operands);
    let Some(data) = read_paths(ctx, &files, "od") else {
        return 1;
    };
    let start = skip.min(data.len());
    let end = limit.map_or(data.len(), |count| (start.saturating_add(count)).min(data.len()));
    let data = &data[start..end];

    let kind = format_kind.chars().next().unwrap_or('o');
    let size: usize = match format_kind[kind.len_utf8()..].parse::<usize>() {
        Ok(size @ (1 | 2 | 4 | 8)) => size,
        _ if kind == 'c' => 1,
        _ => 2,
    };
    if !matches!(kind, 'o' | 'x' | 'd' | 'u' | 'c') {
        error(ctx.stderr, "od", &format!("invalid type '{format_kind}'"));
        return 1;
    }

    let offset_text = |offset: usize| -> String {
        match radix {
            'd' => format!("{offset:07}"),
            'x' => format!("{offset:06x}"),
            'n' => String::new(),
            _ => format!("{offset:07o}"),
        }
    };

    for (row, line) in data.chunks(16).enumerate() {
        let mut text = offset_text(start + row * 16);
        if kind == 'c' {
            for byte in line {
                let cell = match byte {
                    0 => String::from("\\0"),
                    7 => String::from("\\a"),
                    8 => String::from("\\b"),
                    9 => String::from("\\t"),
                    10 => String::from("\\n"),
                    11 => String::from("\\v"),
                    12 => String::from("\\f"),
                    13 => String::from("\\r"),
                    32..=126 => (*byte as char).to_string(),
                    other => format!("{other:03o}"),
                };
                text.push_str(&format!("{cell:>4}"));
            }
        } else {
            for word in line.chunks(size) {
                let mut value = 0u64;
                for (index, byte) in word.iter().enumerate() {
                    value |= u64::from(*byte) << (index * 8);
                }
                let cell = match kind {
                    'x' => format!(" {value:0width$x}", width = size * 2),
                    'u' => format!(" {value:>width$}", width = size * 3),
                    'd' => {
                        let bits = (size * 8) as u32;
                        let signed = if bits < 64 && value >> (bits - 1) & 1 == 1 {
                            value as i64 - (1i64 << bits)
                        } else {
                            value as i64
                        };
                        format!(" {signed:>width$}", width = size * 3)
                    }
                    _ => format!(" {value:0width$o}", width = size * 3),
                };
                text.push_str(&cell);
            }
        }
        write_line(ctx.stdout, format_args!("{text}"));
        if ctx.cancelled() {
            return 130;
        }
    }
    if radix != 'n' {
        write_line(ctx.stdout, format_args!("{}", offset_text(start + data.len())));
    }
    0
}

pub(super) fn fold(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut width = 80usize;
    let mut spaces = false;
    let mut opts = Opts::new(args, "w:sb");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('w', value) => match parse_usize(value) {
                Some(number) if number > 0 => width = number,
                _ => {
                    error(ctx.stderr, "fold", &format!("invalid width: '{value}'"));
                    return 1;
                }
            },
            Opt::Flag('s') => spaces = true,
            Opt::Flag('b') => {}
            other => return bad_option(ctx, "fold", other),
        }
    }
    let files = operands_or_stdin(&mut opts.operands);
    let Some(data) = read_paths(ctx, &files, "fold") else {
        return 1;
    };
    let mut output = Vec::new();
    for line in split_lines(&data) {
        let (body, newline) = match line.strip_suffix(b"\n") {
            Some(body) => (body, true),
            None => (line, false),
        };
        let mut rest = body;
        while rest.len() > width {
            let mut cut = width;
            if spaces {
                if let Some(blank) = rest[..width].iter().rposition(|byte| *byte == b' ') {
                    cut = blank + 1;
                }
            }
            output.extend_from_slice(&rest[..cut]);
            output.push(b'\n');
            rest = &rest[cut..];
        }
        output.extend_from_slice(rest);
        if newline {
            output.push(b'\n');
        }
    }
    ctx.stdout.write(&output);
    0
}

pub(super) fn comm(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut hide = [false; 3];
    let mut opts = Opts::new(args, "123");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('1') => hide[0] = true,
            Opt::Flag('2') => hide[1] = true,
            Opt::Flag('3') => hide[2] = true,
            other => return bad_option(ctx, "comm", other),
        }
    }
    let files = core::mem::take(&mut opts.operands);
    if files.len() != 2 {
        return usage(ctx.stderr, "comm", "comm [-123] FILE1 FILE2");
    }
    let mut buffers = Vec::new();
    for file in &files {
        match read_path(ctx, file) {
            Ok(data) => buffers.push(data),
            Err(err) => {
                path_error(ctx.stderr, "comm", file, err);
                return 1;
            }
        }
    }
    let left = split_lines(&buffers[0]);
    let right = split_lines(&buffers[1]);
    let strip = |line: &'_ [u8]| -> Vec<u8> { line.strip_suffix(b"\n").unwrap_or(line).to_vec() };

    let mut output = Vec::new();
    let mut emit = |column: usize, line: &[u8]| {
        if hide[column] {
            return;
        }
        for previous in 0..column {
            if !hide[previous] {
                output.push(b'\t');
            }
        }
        output.extend_from_slice(&strip(line));
        output.push(b'\n');
    };
    let (mut l, mut r) = (0usize, 0usize);
    while l < left.len() || r < right.len() {
        match (left.get(l), right.get(r)) {
            (Some(a), Some(b)) => match strip(a).cmp(&strip(b)) {
                core::cmp::Ordering::Less => {
                    emit(0, a);
                    l += 1;
                }
                core::cmp::Ordering::Greater => {
                    emit(1, b);
                    r += 1;
                }
                core::cmp::Ordering::Equal => {
                    emit(2, a);
                    l += 1;
                    r += 1;
                }
            },
            (Some(a), None) => {
                emit(0, a);
                l += 1;
            }
            (None, Some(b)) => {
                emit(1, b);
                r += 1;
            }
            (None, None) => break,
        }
    }
    ctx.stdout.write(&output);
    0
}

pub(super) fn split(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut lines = 1000usize;
    let mut bytes: Option<usize> = None;
    let mut suffix_len = 2usize;
    let mut numeric = false;
    let mut opts = Opts::new(args, "l:b:a:d");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('l', value) => match parse_usize(value) {
                Some(number) if number > 0 => lines = number,
                _ => {
                    error(ctx.stderr, "split", "invalid number of lines");
                    return 1;
                }
            },
            Opt::Value('b', value) => match parse_size(value) {
                Some(number) if number > 0 => bytes = Some(number),
                _ => {
                    error(ctx.stderr, "split", "invalid number of bytes");
                    return 1;
                }
            },
            Opt::Value('a', value) => suffix_len = parse_usize(value).unwrap_or(2).clamp(1, 16),
            Opt::Flag('d') => numeric = true,
            other => return bad_option(ctx, "split", other),
        }
    }
    let operands = core::mem::take(&mut opts.operands);
    let input = operands.first().map_or("-", String::as_str);
    let prefix = operands.get(1).map_or("x", String::as_str);

    let data = match read_path(ctx, input) {
        Ok(data) => data,
        Err(err) => {
            path_error(ctx.stderr, "split", input, err);
            return 1;
        }
    };

    let mut pieces: Vec<&[u8]> = Vec::new();
    if let Some(size) = bytes {
        pieces.extend(data.chunks(size));
    } else {
        let all = split_lines(&data);
        for group in all.chunks(lines) {
            let start = group.first().map_or(0, |line| {
                line.as_ptr() as usize - data.as_ptr() as usize
            });
            let total: usize = group.iter().map(|line| line.len()).sum();
            pieces.push(&data[start..start + total]);
        }
    }

    for (index, piece) in pieces.iter().enumerate() {
        let Some(name) = split_name(prefix, index, suffix_len, numeric) else {
            error(ctx.stderr, "split", "output file suffixes exhausted");
            return 1;
        };
        if let Err(err) = ctx.fs.write_file(&name, piece, false) {
            path_error(ctx.stderr, "split", &name, err);
            return 1;
        }
    }
    0
}

fn split_name(prefix: &str, mut index: usize, length: usize, numeric: bool) -> Option<String> {
    let base = if numeric { 10 } else { 26 };
    let mut suffix = alloc::vec![0u8; length];
    for slot in suffix.iter_mut().rev() {
        let digit = (index % base) as u8;
        *slot = if numeric { b'0' + digit } else { b'a' + digit };
        index /= base;
    }
    if index > 0 {
        return None;
    }
    let mut name = String::from(prefix);
    name.push_str(core::str::from_utf8(&suffix).ok()?);
    Some(name)
}

pub(super) fn dd(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut input: Option<&str> = None;
    let mut output: Option<&str> = None;
    let mut block = 512usize;
    let mut count: Option<usize> = None;
    let mut skip = 0usize;
    let mut seek = 0usize;
    let mut quiet = false;
    for arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            error(ctx.stderr, "dd", &format!("unrecognized operand '{arg}'"));
            return 1;
        };
        let size = parse_size(value);
        match key {
            "if" => input = Some(value),
            "of" => output = Some(value),
            "bs" | "ibs" | "obs" => match size {
                Some(number) if number > 0 => block = number,
                _ => {
                    error(ctx.stderr, "dd", &format!("invalid number: '{value}'"));
                    return 1;
                }
            },
            "count" | "skip" | "seek" => {
                let Some(number) = size else {
                    error(ctx.stderr, "dd", &format!("invalid number: '{value}'"));
                    return 1;
                };
                match key {
                    "count" => count = Some(number),
                    "skip" => skip = number,
                    _ => seek = number,
                }
            }
            "status" => quiet = value == "none",
            "conv" => {}
            other => {
                error(ctx.stderr, "dd", &format!("unrecognized operand '{other}'"));
                return 1;
            }
        }
    }

    let wanted = count.and_then(|count| count.checked_mul(block));
    if wanted.is_some_and(|bytes| bytes > MAX_STREAM_BYTES) {
        path_error(ctx.stderr, "dd", "input", FsError::NoSpace);
        return 1;
    }

    let start = skip.saturating_mul(block);
    let data: Vec<u8> = match input {
        // Reading a device of endless zeros needs a count.
        Some("/dev/zero") => {
            let Some(bytes) = wanted else {
                error(ctx.stderr, "dd", "reading /dev/zero needs count=");
                return 1;
            };
            alloc::vec![0u8; bytes]
        }
        Some(path) if path != "-" => {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut offset = start;
            loop {
                let room = wanted.map_or(chunk.len(), |bytes| {
                    (bytes - buffer.len()).min(chunk.len())
                });
                if room == 0 {
                    break;
                }
                match ctx.fs.read_at(path, offset, &mut chunk[..room]) {
                    Ok(0) => break,
                    Ok(read) => {
                        if buffer.len() + read > MAX_STREAM_BYTES
                            || buffer.try_reserve(read).is_err()
                        {
                            path_error(ctx.stderr, "dd", path, FsError::NoSpace);
                            return 1;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        offset += read;
                    }
                    Err(err) => {
                        path_error(ctx.stderr, "dd", path, err);
                        return 1;
                    }
                }
            }
            buffer
        }
        _ => {
            let all = match ctx.stdin.try_read_to_end(MAX_STREAM_BYTES) {
                Ok(all) => all,
                Err(err) => {
                    error(ctx.stderr, "dd", err.text());
                    return 1;
                }
            };
            let from = start.min(all.len());
            let to = wanted.map_or(all.len(), |bytes| (from + bytes).min(all.len()));
            all[from..to].to_vec()
        }
    };

    match output {
        Some(path) if path != "-" => {
            let result = if seek > 0 {
                // Keep what lies before the seek point, and pad a short file.
                let mut existing = ctx.fs.read_file(path).unwrap_or_default();
                existing.resize(seek.saturating_mul(block).min(MAX_STREAM_BYTES), 0);
                existing.extend_from_slice(&data);
                ctx.fs.write_file(path, &existing, false)
            } else {
                ctx.fs.write_file(path, &data, false)
            };
            if let Err(err) = result {
                path_error(ctx.stderr, "dd", path, err);
                return 1;
            }
        }
        _ => ctx.stdout.write(&data),
    }

    if !quiet {
        let whole = data.len() / block;
        let partial = usize::from(data.len() % block != 0);
        let message = format!(
            "{whole}+{partial} records in\n{whole}+{partial} records out\n{} bytes copied\n",
            data.len()
        );
        ctx.stderr.write(message.as_bytes());
    }
    0
}

pub(super) fn strings(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut minimum = 4usize;
    let mut opts = Opts::new(args, "n:a");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Value('n', value) => minimum = parse_usize(value).unwrap_or(4).max(1),
            Opt::Flag('a') => {}
            other => return bad_option(ctx, "strings", other),
        }
    }
    let files = operands_or_stdin(&mut opts.operands);
    let Some(data) = read_paths(ctx, &files, "strings") else {
        return 1;
    };
    let printable = |byte: &u8| byte.is_ascii_graphic() || matches!(byte, b' ' | b'\t');
    for run in data.split(|byte| !printable(byte)) {
        if run.len() >= minimum {
            ctx.stdout.write(run);
            ctx.stdout.write(b"\n");
        }
    }
    0
}

pub(super) fn realpath(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut must_exist = false;
    let mut opts = Opts::new(args, "emq");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('e') => must_exist = true,
            Opt::Flag('m' | 'q') => {}
            other => return bad_option(ctx, "realpath", other),
        }
    }
    let operands = core::mem::take(&mut opts.operands);
    if operands.is_empty() {
        return usage(ctx.stderr, "realpath", "realpath [-em] PATH...");
    }
    let mut status = 0;
    for path in &operands {
        let absolute = ctx.fs.resolve(path);
        if must_exist {
            if let Err(err) = ctx.fs.meta(path) {
                path_error(ctx.stderr, "realpath", path, err);
                status = 1;
                continue;
            }
        }
        write_line(ctx.stdout, format_args!("{absolute}"));
    }
    status
}

pub(super) fn mktemp(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut directory = false;
    let mut parent: Option<String> = None;
    let mut opts = Opts::new(args, "dp:tq");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('d') => directory = true,
            Opt::Value('p', value) => parent = Some(value.to_string()),
            Opt::Flag('t' | 'q') => {}
            other => return bad_option(ctx, "mktemp", other),
        }
    }
    let operands = core::mem::take(&mut opts.operands);
    let explicit = operands.first();
    let template = explicit.map_or("tmp.XXXXXXXXXX", String::as_str);
    let run = template.len() - template.trim_end_matches('X').len();
    if run < 3 {
        error(ctx.stderr, "mktemp", &format!("too few X's in template '{template}'"));
        return 1;
    }
    let stem = &template[..template.len() - run];
    let base = match parent {
        Some(parent) => Some(parent),
        None if explicit.is_none() => Some(
            ctx.env.get("TMPDIR").map_or_else(|| String::from("/tmp"), str::to_string),
        ),
        None => None,
    };

    // There is no randomness source, so mix what changes and probe for a free
    // name. Uniqueness comes from the existence check.
    let mut state = ctx.system.uptime_ms().wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ u64::from(ctx.system.pid()).rotate_left(32);
    const ALPHABET: &[u8; 62] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    for _ in 0..4096 {
        let mut name = String::from(stem);
        for _ in 0..run {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state = state.wrapping_add(0x2545_f491_4f6c_dd1d);
            name.push(ALPHABET[(state % 62) as usize] as char);
        }
        let path = match &base {
            Some(base) => crate::fs::join(base.trim_end_matches('/'), &name),
            None => name,
        };
        if ctx.fs.exists(&path) {
            continue;
        }
        let result = if directory {
            ctx.fs.create_dir(&path)
        } else {
            ctx.fs.write_file(&path, b"", false)
        };
        return match result {
            Ok(()) => {
                write_line(ctx.stdout, format_args!("{path}"));
                0
            }
            Err(err) => {
                path_error(ctx.stderr, "mktemp", &path, err);
                1
            }
        };
    }
    error(ctx.stderr, "mktemp", "could not find a free name");
    1
}

pub(super) fn file(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut brief = false;
    let mut opts = Opts::new(args, "b");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('b') => brief = true,
            other => return bad_option(ctx, "file", other),
        }
    }
    let operands = core::mem::take(&mut opts.operands);
    if operands.is_empty() {
        return usage(ctx.stderr, "file", "file [-b] PATH...");
    }
    let mut status = 0;
    for path in &operands {
        let description = match ctx.fs.meta(path) {
            Ok(meta) if meta.kind == Kind::Dir => String::from("directory"),
            Ok(meta) if meta.size == 0 => String::from("empty"),
            Ok(_) => {
                let mut head = [0u8; 512];
                match ctx.fs.read_at(path, 0, &mut head) {
                    Ok(read) => String::from(identify(&head[..read])),
                    Err(err) => format!("cannot open: {}", err.text()),
                }
            }
            Err(err) => {
                status = 1;
                format!("cannot open: {}", err.text())
            }
        };
        if brief {
            write_line(ctx.stdout, format_args!("{description}"));
        } else {
            write_line(ctx.stdout, format_args!("{path}: {description}"));
        }
    }
    status
}

fn identify(head: &[u8]) -> &'static str {
    let tar_magic = head.get(257..262) == Some(b"ustar".as_slice());
    match head {
        [0x1f, 0x8b, ..] => "gzip compressed data",
        [b'P', b'K', 3, 4, ..] => "Zip archive data",
        _ if tar_magic => "POSIX tar archive",
        [0x7f, b'E', b'L', b'F', ..] => "ELF executable",
        [0x89, b'P', b'N', b'G', ..] => "PNG image data",
        [0xff, 0xd8, 0xff, ..] => "JPEG image data",
        [b'G', b'I', b'F', b'8', ..] => "GIF image data",
        [b'B', b'M', ..] => "BMP image data",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'A', b'V', b'E', ..] => "RIFF WAVE audio",
        [b'R', b'I', b'F', b'F', ..] => "RIFF data",
        [b'O', b'g', b'g', b'S', ..] => "Ogg data",
        [b'f', b'L', b'a', b'C', ..] => "FLAC audio",
        [b'I', b'D', b'3', ..] | [0xff, 0xfb, ..] => "MPEG audio",
        [b'F', b'L', b'V', ..] => "FLV video",
        [0x1a, 0x45, 0xdf, 0xa3, ..] => "Matroska data",
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => "ISO Media (MP4)",
        [b'%', b'P', b'D', b'F', ..] => "PDF document",
        [b'#', b'!', ..] => "script text",
        _ => {
            if head.contains(&0) {
                "data"
            } else if core::str::from_utf8(head).is_ok() {
                if head.is_ascii() {
                    "ASCII text"
                } else {
                    "UTF-8 text"
                }
            } else {
                "data"
            }
        }
    }
}

/// Splits Unix seconds into `(year, month, day)`, using Howard Hinnant's
/// days-from-civil inverse.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub(super) fn date(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut seconds = ctx
        .system
        .unix_time()
        .unwrap_or(ctx.system.uptime_ms() / 1000) as i64;
    let mut opts = Opts::new(args, "ud:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('u') => {}
            Opt::Value('d', value) => match value.strip_prefix('@').and_then(|v| v.parse().ok()) {
                Some(explicit) => seconds = explicit,
                None => {
                    error(ctx.stderr, "date", &format!("invalid date '{value}'"));
                    return 1;
                }
            },
            other => return bad_option(ctx, "date", other),
        }
    }
    let operands = core::mem::take(&mut opts.operands);
    let format_text = match operands.first() {
        Some(text) if text.starts_with('+') => &text[1..],
        Some(text) => {
            error(ctx.stderr, "date", &format!("setting the date is not supported: '{text}'"));
            return 1;
        }
        None => "%a %b %e %H:%M:%S UTC %Y",
    };

    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (time / 3600, time / 60 % 60, time % 60);
    let weekday = (days + 4).rem_euclid(7) as usize;
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let mut output = String::new();
    let mut chars = format_text.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('Y') => output.push_str(&format!("{year}")),
            Some('y') => output.push_str(&format!("{:02}", year.rem_euclid(100))),
            Some('m') => output.push_str(&format!("{month:02}")),
            Some('d') => output.push_str(&format!("{day:02}")),
            Some('e') => output.push_str(&format!("{day:>2}")),
            Some('H') => output.push_str(&format!("{hour:02}")),
            Some('M') => output.push_str(&format!("{minute:02}")),
            Some('S') => output.push_str(&format!("{second:02}")),
            Some('s') => output.push_str(&format!("{seconds}")),
            Some('a') => output.push_str(DAYS[weekday]),
            Some('b') | Some('h') => {
                output.push_str(MONTHS[(month as usize - 1).min(11)]);
            }
            Some('F') => output.push_str(&format!("{year}-{month:02}-{day:02}")),
            Some('T') => output.push_str(&format!("{hour:02}:{minute:02}:{second:02}")),
            Some('D') => {
                output.push_str(&format!("{month:02}/{day:02}/{:02}", year.rem_euclid(100)))
            }
            Some('Z') => output.push_str("UTC"),
            Some('z') => output.push_str("+0000"),
            Some('n') => output.push('\n'),
            Some('t') => output.push('\t'),
            Some('%') => output.push('%'),
            Some(other) => {
                output.push('%');
                output.push(other);
            }
            None => output.push('%'),
        }
    }
    write_out(ctx.stdout, format_args!("{output}\n"));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_xargs_input_with_quotes() {
        assert_eq!(split_items(b"a 'b c'\n\"d\" e\\ f", None), ["a", "b c", "d", "e f"]);
        assert_eq!(split_items(b"a b\0c\0", Some(0)), ["a b", "c"]);
    }

    #[test]
    fn converts_days_to_civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn names_split_output_files() {
        assert_eq!(split_name("x", 0, 2, false).as_deref(), Some("xaa"));
        assert_eq!(split_name("x", 27, 2, false).as_deref(), Some("xbb"));
        assert_eq!(split_name("p", 7, 3, true).as_deref(), Some("p007"));
        assert_eq!(split_name("x", 26 * 26, 2, false), None);
    }

    #[test]
    fn identifies_common_formats() {
        assert_eq!(identify(&[0x1f, 0x8b, 8]), "gzip compressed data");
        assert_eq!(identify(b"hello\n"), "ASCII text");
        assert_eq!(identify(&[0, 1, 2]), "data");
    }
}
