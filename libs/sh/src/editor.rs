//! Interactive line editor.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::lexer;
use crate::{Host, Io, Shell};

/// Result of one `read_line` call.
pub enum ReadResult {
    /// A complete line (possibly containing embedded newlines from a
    /// continuation).
    Line(String),
    /// The input reached end of file (Ctrl-D on an empty line).
    Eof,
    /// The user pressed Ctrl-C.
    Interrupted,
}

enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Tab,
    CtrlC,
    CtrlD,
    CtrlL,
    CtrlU,
    CtrlK,
    CtrlW,
    CtrlA,
    CtrlE,
    CtrlR,
    Eof,
    Unknown,
}

/// Reads and edits one command line, printing the prompt.
pub fn read_line(shell: &mut Shell<'_>, host: &mut dyn Host, io: &mut Io<'_>) -> ReadResult {
    let mut prompt = shell.prompt();
    let mut buffer: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut pending = String::new();
    let mut history_index = shell.history.len();
    let mut saved_line = String::new();

    draw(io, &prompt, &buffer, cursor);

    loop {
        match read_key(io) {
            Key::Char(ch) => {
                if cursor < buffer.len() {
                    buffer.insert(cursor, ch);
                } else {
                    buffer.push(ch);
                }
                cursor += 1;
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::Enter => {
                let line: String = buffer.iter().collect();
                if lexer::needs_more(&line) {
                    pending.push_str(&line);
                    pending.push('\n');
                    buffer.clear();
                    cursor = 0;
                    prompt = String::from("> ");
                    io.stdout.write(b"\n");
                    draw(io, &prompt, &buffer, cursor);
                    continue;
                }
                io.stdout.write(b"\n");
                let mut complete = pending;
                complete.push_str(&line);
                return ReadResult::Line(complete);
            }
            Key::Backspace => {
                if cursor > 0 {
                    cursor -= 1;
                    buffer.remove(cursor);
                    redraw(io, &prompt, &buffer, cursor);
                }
            }
            Key::Delete => {
                if cursor < buffer.len() {
                    buffer.remove(cursor);
                    redraw(io, &prompt, &buffer, cursor);
                }
            }
            Key::Left => {
                if cursor > 0 {
                    cursor -= 1;
                    redraw(io, &prompt, &buffer, cursor);
                }
            }
            Key::Right => {
                if cursor < buffer.len() {
                    cursor += 1;
                    redraw(io, &prompt, &buffer, cursor);
                }
            }
            Key::Home | Key::CtrlA => {
                cursor = 0;
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::End | Key::CtrlE => {
                cursor = buffer.len();
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::Up => {
                if !shell.history.is_empty() {
                    if history_index == shell.history.len() {
                        saved_line = buffer.iter().collect();
                    }
                    if history_index > 0 {
                        history_index -= 1;
                        buffer = shell.history[history_index].chars().collect();
                        cursor = buffer.len();
                        redraw(io, &prompt, &buffer, cursor);
                    }
                }
            }
            Key::Down => {
                if history_index < shell.history.len() {
                    history_index += 1;
                    if history_index == shell.history.len() {
                        buffer = saved_line.chars().collect();
                    } else {
                        buffer = shell.history[history_index].chars().collect();
                    }
                    cursor = buffer.len();
                    redraw(io, &prompt, &buffer, cursor);
                }
            }
            Key::CtrlU => {
                buffer.drain(..cursor);
                cursor = 0;
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::CtrlK => {
                buffer.truncate(cursor);
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::CtrlW => {
                let mut start = cursor;
                while start > 0 && buffer[start - 1].is_whitespace() {
                    start -= 1;
                }
                while start > 0 && !buffer[start - 1].is_whitespace() {
                    start -= 1;
                }
                buffer.drain(start..cursor);
                cursor = start;
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::CtrlL => {
                io.stdout.write(b"\x1b[2J\x1b[H");
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::CtrlC => {
                io.stdout.write(b"^C\n");
                return ReadResult::Interrupted;
            }
            Key::CtrlD => {
                if buffer.is_empty() && pending.is_empty() {
                    return ReadResult::Eof;
                }
                if cursor < buffer.len() {
                    buffer.remove(cursor);
                    redraw(io, &prompt, &buffer, cursor);
                }
            }
            Key::Tab => {
                let line: String = buffer.iter().collect();
                let byte_cursor = line
                    .char_indices()
                    .nth(cursor)
                    .map(|(index, _)| index)
                    .unwrap_or(line.len());
                let completion = shell.complete(&line, byte_cursor, host);
                buffer = completion.line.chars().collect();
                cursor = completion.line[..completion.cursor].chars().count();
                if !completion.candidates.is_empty() {
                    io.stdout.write(b"\n");
                    for candidate in &completion.candidates {
                        io.stdout.write(candidate.as_bytes());
                        io.stdout.write(b"  ");
                    }
                    io.stdout.write(b"\n");
                }
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::CtrlR => {
                reverse_search(shell, io, &mut buffer, &mut cursor);
                redraw(io, &prompt, &buffer, cursor);
            }
            Key::Eof => return ReadResult::Eof,
            Key::Unknown => {}
        }
    }
}

fn reverse_search(shell: &Shell<'_>, io: &mut Io<'_>, buffer: &mut Vec<char>, cursor: &mut usize) {
    let mut query = String::new();
    let mut found: Option<String> = None;

    loop {
        let shown = found.clone().unwrap_or_default();
        let line = format!("\r\x1b[K(reverse-i-search)`{query}': {shown}");
        io.stdout.write(line.as_bytes());

        match read_key(io) {
            Key::Char(ch) => {
                query.push(ch);
                found = shell
                    .history
                    .iter()
                    .rev()
                    .find(|entry| entry.contains(&query))
                    .cloned();
            }
            Key::Backspace => {
                query.pop();
                found = if query.is_empty() {
                    None
                } else {
                    shell
                        .history
                        .iter()
                        .rev()
                        .find(|entry| entry.contains(&query))
                        .cloned()
                };
            }
            Key::Enter => {
                if let Some(found) = found {
                    *buffer = found.chars().collect();
                    *cursor = buffer.len();
                }
                return;
            }
            Key::CtrlC | Key::Eof => return,
            _ => {}
        }
    }
}

fn draw(io: &mut Io<'_>, prompt: &str, buffer: &[char], cursor: usize) {
    io.stdout.write(prompt.as_bytes());
    let line: String = buffer.iter().collect();
    io.stdout.write(line.as_bytes());
    let behind = buffer.len().saturating_sub(cursor);
    if behind > 0 {
        let _ = io.stdout.write(format!("\x1b[{behind}D").as_bytes());
    }
}

fn redraw(io: &mut Io<'_>, prompt: &str, buffer: &[char], cursor: usize) {
    io.stdout.write(b"\r\x1b[K");
    draw(io, prompt, buffer, cursor);
}

fn read_key(io: &mut Io<'_>) -> Key {
    let mut byte = [0u8; 1];
    if io.stdin.read(&mut byte) == 0 {
        return Key::Eof;
    }

    match byte[0] {
        0x1b => read_escape(io),
        0x0d | 0x0a => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        0x03 => Key::CtrlC,
        0x04 => Key::CtrlD,
        0x0c => Key::CtrlL,
        0x15 => Key::CtrlU,
        0x0b => Key::CtrlK,
        0x17 => Key::CtrlW,
        0x01 => Key::CtrlA,
        0x05 => Key::CtrlE,
        0x12 => Key::CtrlR,
        0x09 => Key::Tab,
        byte if byte >= 0x20 && byte < 0x7f => Key::Char(byte as char),
        byte if byte >= 0x80 => read_utf8(io, byte),
        _ => Key::Unknown,
    }
}

fn read_escape(io: &mut Io<'_>) -> Key {
    let mut byte = [0u8; 1];
    if io.stdin.read(&mut byte) == 0 {
        return Key::Unknown;
    }

    match byte[0] {
        b'[' => {
            let mut params = Vec::new();
            loop {
                if io.stdin.read(&mut byte) == 0 {
                    return Key::Unknown;
                }
                let value = byte[0];
                if (0x40..=0x7e).contains(&value) {
                    return match value {
                        b'A' => Key::Up,
                        b'B' => Key::Down,
                        b'C' => Key::Right,
                        b'D' => Key::Left,
                        b'H' => Key::Home,
                        b'F' => Key::End,
                        b'~' => match params.first().copied() {
                            Some(b'1') | Some(b'7') => Key::Home,
                            Some(b'4') | Some(b'8') => Key::End,
                            Some(b'3') => Key::Delete,
                            _ => Key::Unknown,
                        },
                        _ => Key::Unknown,
                    };
                }
                params.push(value);
            }
        }
        b'O' => {
            if io.stdin.read(&mut byte) == 0 {
                return Key::Unknown;
            }
            match byte[0] {
                b'H' => Key::Home,
                b'F' => Key::End,
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                _ => Key::Unknown,
            }
        }
        0x7f => Key::Backspace,
        _ => Key::Unknown,
    }
}

fn read_utf8(io: &mut Io<'_>, first: u8) -> Key {
    let length = if first & 0b1110_0000 == 0b1100_0000 {
        2
    } else if first & 0b1111_0000 == 0b1110_0000 {
        3
    } else if first & 0b1111_1000 == 0b1111_0000 {
        4
    } else {
        return Key::Unknown;
    };

    let mut bytes = [0u8; 4];
    bytes[0] = first;
    for index in 1..length {
        if io.stdin.read(&mut bytes[index..index + 1]) == 0 {
            return Key::Unknown;
        }
    }
    core::str::from_utf8(&bytes[..length])
        .ok()
        .and_then(|text| text.chars().next())
        .map(Key::Char)
        .unwrap_or(Key::Unknown)
}
