//! What a full-screen program needs from a terminal.
//!
//! A program takes the keyboard with [`RawTerminal::enter`], reads [`Key`]s and
//! draws with VT sequences on standard output. The alternate screen means that
//! leaving puts back whatever the terminal showed before. Keys come from the
//! terminal even when standard input is a pipe, so `ps | less` can page.

use alloc::vec::Vec;

use crate::commands::Ctx;
use crate::env::Env;
use crate::io::{Input, Output};

/// One keypress.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Key {
    Char(char),
    /// A letter with Ctrl: `Ctrl(b's')`.
    Ctrl(u8),
    Enter,
    Tab,
    Backspace,
    Delete,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// The input has gone.
    Eof,
    /// Something this decoder doesn't know.
    Unknown,
}

/// Decodes bytes into keys. A separate step from reading, for tests.
pub(super) fn decode_key(bytes: &[u8]) -> (Key, usize) {
    let Some(&first) = bytes.first() else {
        return (Key::Eof, 0);
    };
    match first {
        0x1b => decode_escape(bytes),
        b'\r' | b'\n' => (Key::Enter, 1),
        b'\t' => (Key::Tab, 1),
        0x7f | 0x08 => (Key::Backspace, 1),
        0x01..=0x1a => (Key::Ctrl(first + b'a' - 1), 1),
        0x00..=0x1f => (Key::Unknown, 1),
        0x20..=0x7e => (Key::Char(first as char), 1),
        _ => {
            let length = match first {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return (Key::Unknown, 1),
            };
            match bytes.get(..length).and_then(|part| core::str::from_utf8(part).ok()) {
                Some(text) => (text.chars().next().map_or(Key::Unknown, Key::Char), length),
                None => (Key::Unknown, 1),
            }
        }
    }
}

fn decode_escape(bytes: &[u8]) -> (Key, usize) {
    match bytes.get(1) {
        None => (Key::Escape, 1),
        Some(b'[') => {
            let mut end = 2;
            while bytes.get(end).is_some_and(|byte| byte.is_ascii_digit() || *byte == b';') {
                end += 1;
            }
            let Some(&last) = bytes.get(end) else {
                return (Key::Unknown, bytes.len());
            };
            let number = bytes[2..end]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .fold(0u32, |value, byte| {
                    value.saturating_mul(10).saturating_add(u32::from(byte - b'0'))
                });
            let key = match (last, number) {
                (b'A', _) => Key::Up,
                (b'B', _) => Key::Down,
                (b'C', _) => Key::Right,
                (b'D', _) => Key::Left,
                (b'H', _) | (b'~', 1 | 7) => Key::Home,
                (b'F', _) | (b'~', 4 | 8) => Key::End,
                (b'~', 3) => Key::Delete,
                (b'~', 5) => Key::PageUp,
                (b'~', 6) => Key::PageDown,
                _ => Key::Unknown,
            };
            (key, end + 1)
        }
        Some(b'O') => {
            let key = match bytes.get(2) {
                Some(b'A') => Key::Up,
                Some(b'B') => Key::Down,
                Some(b'C') => Key::Right,
                Some(b'D') => Key::Left,
                Some(b'H') => Key::Home,
                Some(b'F') => Key::End,
                _ => Key::Unknown,
            };
            (key, 3.min(bytes.len()))
        }
        // Escape on its own, then whatever followed.
        Some(_) => (Key::Escape, 1),
    }
}

/// Reads one key from the terminal.
pub(super) fn read_key(input: &mut dyn Input, pending: &mut Vec<u8>) -> Key {
    if pending.is_empty() {
        let mut buffer = [0u8; 64];
        let count = input.read_tty(&mut buffer);
        if count == 0 {
            return Key::Eof;
        }
        pending.extend_from_slice(&buffer[..count]);
        // An escape sequence or a multi-byte character may still be arriving.
        let needs_more = pending[0] == 0x1b || pending[0] >= 0xc0;
        if needs_more && pending.len() < 8 {
            let count = input.read_tty_pending(&mut buffer);
            pending.extend_from_slice(&buffer[..count]);
        }
    }
    let (key, used) = decode_key(pending);
    pending.drain(..used.max(1).min(pending.len()));
    key
}

/// The terminal's size, from `COLUMNS` and `LINES`, or the classic 80x24.
pub(super) fn screen_size(env: &Env) -> (usize, usize) {
    let number = |name, default: usize| {
        env.get(name)
            .and_then(|text| text.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default)
    };
    (number("COLUMNS", 80).clamp(10, 500), number("LINES", 24).clamp(3, 200))
}

/// Holds the terminal in raw mode on the alternate screen.
pub(super) struct RawTerminal;

impl RawTerminal {
    pub(super) fn enter(ctx: &mut Ctx<'_>) -> Self {
        ctx.stdin.set_raw(true);
        ctx.stdout.write(b"\x1b[?1049h\x1b[2J\x1b[H");
        Self
    }

    pub(super) fn leave(self, ctx: &mut Ctx<'_>) {
        ctx.stdout.write(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        ctx.stdin.set_raw(false);
    }
}

/// Appends a sequence that moves the cursor to a zero-based position.
pub(super) fn move_to(out: &mut Vec<u8>, row: usize, column: usize) {
    use core::fmt::Write;
    let mut text = alloc::string::String::new();
    let _ = write!(text, "\x1b[{};{}H", row + 1, column + 1);
    out.extend_from_slice(text.as_bytes());
}

/// True when both ends are a terminal, which a full-screen program needs.
pub(super) fn on_terminal(input: &dyn Input, output: &dyn Output) -> bool {
    input.tty_is_terminal() && output.is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_decode_from_what_terminals_send() {
        assert_eq!(decode_key(b"a"), (Key::Char('a'), 1));
        assert_eq!(decode_key(b"\x13"), (Key::Ctrl(b's'), 1));
        assert_eq!(decode_key(b"\r"), (Key::Enter, 1));
        assert_eq!(decode_key(b"\x7f"), (Key::Backspace, 1));
        assert_eq!(decode_key(b"\x1b[A"), (Key::Up, 3));
        assert_eq!(decode_key(b"\x1bOB"), (Key::Down, 3));
        assert_eq!(decode_key(b"\x1b[5~"), (Key::PageUp, 4));
        assert_eq!(decode_key(b"\x1b[1;5C"), (Key::Right, 6));
        assert_eq!(decode_key(b"\x1b"), (Key::Escape, 1));
        assert_eq!(decode_key("é".as_bytes()), (Key::Char('é'), 2));
    }

    #[test]
    fn broken_input_never_panics() {
        for bytes in [&b"\x1b["[..], b"\x1b[99", b"\xc3", b"\xff", b"\x1bO", b"\x1b[;;~"] {
            let (_, used) = decode_key(bytes);
            assert!(used <= bytes.len());
        }
    }
}
