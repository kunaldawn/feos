//! A BIOS-compatible keyboard queue.
//!
//! The desktop, the serial decoder and host tests all produce the same
//! [`KeyEvent`]: an ASCII byte and a PC scan code. `INT 16h` and the DOS
//! console functions read from this queue, so the guest sees a normal 84-key
//! keyboard.

use alloc::collections::VecDeque;

/// One key press as the BIOS reports it (`AH` = scan code, `AL` = ASCII).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyEvent {
    pub ascii: u8,
    pub scan: u8,
}

impl KeyEvent {
    pub const fn new(ascii: u8, scan: u8) -> Self {
        Self { ascii, scan }
    }

    pub const fn word(self) -> u16 {
        ((self.scan as u16) << 8) | self.ascii as u16
    }
}

/// The keyboard controller state and its 16-entry type-ahead buffer.
pub struct Keyboard {
    buffer: VecDeque<KeyEvent>,
    shift: bool,
    ctrl: bool,
    alt: bool,
    caps: bool,
    num: bool,
    scroll: bool,
    insert: bool,
    last_scan: u8,
    /// A key pushed by `INT 16h` function 05h.
    pushed: Option<KeyEvent>,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyboard {
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            shift: false,
            ctrl: false,
            alt: false,
            caps: false,
            num: false,
            scroll: false,
            insert: false,
            last_scan: 0,
            pushed: None,
        }
    }

    /// Empties the type-ahead buffer.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.pushed = None;
    }

    pub fn is_empty(&self) -> bool {
        self.pushed.is_none() && self.buffer.is_empty()
    }

    pub fn len(&self) -> usize {
        self.buffer.len() + usize::from(self.pushed.is_some())
    }

    /// Queues one key. The buffer holds at most 16 keys, like the BIOS.
    pub fn push(&mut self, ascii: u8, scan: u8) {
        self.last_scan = scan;
        if self.buffer.len() >= 16 {
            self.buffer.pop_front();
        }
        self.buffer.push_back(KeyEvent::new(ascii, scan));
    }

    /// Queues a printable character using the US scan-code table.
    pub fn push_ascii(&mut self, ch: u8, scan: u8) {
        self.push(ch, scan);
    }

    /// Queues a character and derives its scan code.
    pub fn push_char(&mut self, ch: char) {
        if let Some(ascii) = ascii_for_char(ch) {
            let scan = scan_for_char(ch);
            self.push(ascii, scan);
        }
    }

    /// Queues a non-printing key such as Enter, Escape or an arrow.
    pub fn push_key(&mut self, key: Key) {
        let (ascii, scan) = key.event();
        self.push(ascii, scan);
    }

    pub fn pop(&mut self) -> Option<KeyEvent> {
        if let Some(event) = self.pushed.take() {
            return Some(event);
        }
        self.buffer.pop_front()
    }

    pub fn peek(&self) -> Option<KeyEvent> {
        self.pushed.or_else(|| self.buffer.front().copied())
    }

    /// `INT 16h` function 05h: push a key back for the next read.
    pub fn push_back(&mut self, event: KeyEvent) {
        self.pushed = Some(event);
    }

    pub fn set_modifiers(
        &mut self,
        shift: bool,
        ctrl: bool,
        alt: bool,
        caps: bool,
        num: bool,
        scroll: bool,
        insert: bool,
    ) {
        self.shift = shift;
        self.ctrl = ctrl;
        self.alt = alt;
        self.caps = caps;
        self.num = num;
        self.scroll = scroll;
        self.insert = insert;
    }

    /// The BIOS keyboard flags returned by `INT 16h` function 02h.
    pub fn flags(&self) -> u8 {
        let mut flags = 0;
        if self.shift {
            flags |= 0x03;
        }
        if self.ctrl {
            flags |= 0x04;
        }
        if self.alt {
            flags |= 0x08;
        }
        if self.scroll {
            flags |= 0x10;
        }
        if self.num {
            flags |= 0x20;
        }
        if self.caps {
            flags |= 0x40;
        }
        if self.insert {
            flags |= 0x80;
        }
        flags
    }

    pub fn last_scan(&self) -> u8 {
        self.last_scan
    }
}

/// A non-printing key, with its BIOS ASCII/scan-code pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Enter,
    Escape,
    Backspace,
    Tab,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl Key {
    pub const fn event(self) -> (u8, u8) {
        match self {
            Key::Enter => (0x0D, 0x1C),
            Key::Escape => (0x1B, 0x01),
            Key::Backspace => (0x08, 0x0E),
            Key::Tab => (0x09, 0x0F),
            Key::Space => (0x20, 0x39),
            Key::Up => (0x00, 0x48),
            Key::Down => (0x00, 0x50),
            Key::Left => (0x00, 0x4B),
            Key::Right => (0x00, 0x4D),
            Key::Home => (0x00, 0x47),
            Key::End => (0x00, 0x4F),
            Key::PageUp => (0x00, 0x49),
            Key::PageDown => (0x00, 0x51),
            Key::Insert => (0x00, 0x52),
            Key::Delete => (0x00, 0x53),
            Key::F1 => (0x00, 0x3B),
            Key::F2 => (0x00, 0x3C),
            Key::F3 => (0x00, 0x3D),
            Key::F4 => (0x00, 0x3E),
            Key::F5 => (0x00, 0x3F),
            Key::F6 => (0x00, 0x40),
            Key::F7 => (0x00, 0x41),
            Key::F8 => (0x00, 0x42),
            Key::F9 => (0x00, 0x43),
            Key::F10 => (0x00, 0x44),
            Key::F11 => (0x00, 0x57),
            Key::F12 => (0x00, 0x58),
        }
    }
}

/// The US-layout scan code for a printable character.
pub fn scan_for_char(ch: char) -> u8 {
    match ch.to_ascii_uppercase() {
        'A' => 0x1E,
        'B' => 0x30,
        'C' => 0x2E,
        'D' => 0x20,
        'E' => 0x12,
        'F' => 0x21,
        'G' => 0x22,
        'H' => 0x23,
        'I' => 0x17,
        'J' => 0x24,
        'K' => 0x25,
        'L' => 0x26,
        'M' => 0x32,
        'N' => 0x31,
        'O' => 0x18,
        'P' => 0x19,
        'Q' => 0x10,
        'R' => 0x13,
        'S' => 0x1F,
        'T' => 0x14,
        'U' => 0x16,
        'V' => 0x2F,
        'W' => 0x11,
        'X' => 0x2D,
        'Y' => 0x15,
        'Z' => 0x2C,
        '1' => 0x02,
        '2' => 0x03,
        '3' => 0x04,
        '4' => 0x05,
        '5' => 0x06,
        '6' => 0x07,
        '7' => 0x08,
        '8' => 0x09,
        '9' => 0x0A,
        '0' => 0x0B,
        ' ' => 0x39,
        '\n' | '\r' => 0x1C,
        '\t' => 0x0F,
        '\u{8}' => 0x0E,
        '-' | '_' => 0x0C,
        '=' | '+' => 0x0D,
        '[' | '{' => 0x1A,
        ']' | '}' => 0x1B,
        '\\' | '|' => 0x2B,
        ';' | ':' => 0x27,
        '\'' | '"' => 0x28,
        '`' | '~' => 0x29,
        ',' | '<' => 0x33,
        '.' | '>' => 0x34,
        '/' | '?' => 0x35,
        '*' => 0x37,
        _ => 0,
    }
}

/// The ASCII byte a character sends to the guest.
pub fn ascii_for_char(ch: char) -> Option<u8> {
    let code = ch as u32;
    if (0x20..=0x7E).contains(&code) {
        Some(code as u8)
    } else {
        match ch {
            '\n' | '\r' => Some(0x0D),
            '\t' => Some(0x09),
            '\u{8}' => Some(0x08),
            '\u{1b}' => Some(0x1B),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_reports_scan_and_ascii() {
        let mut keyboard = Keyboard::new();
        keyboard.push_ascii(b'a', scan_for_char('a'));
        let event = keyboard.pop().expect("key");
        assert_eq!(event.ascii, b'a');
        assert_eq!(event.scan, 0x1E);
        assert!(keyboard.is_empty());
    }

    #[test]
    fn shift_flags_match_bios_layout() {
        let mut keyboard = Keyboard::new();
        keyboard.set_modifiers(true, false, false, false, false, false, false);
        assert_eq!(keyboard.flags() & 0x03, 0x03);
    }
}
