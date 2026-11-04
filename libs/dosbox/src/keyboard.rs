//! The keyboard: host events, the 8042 controller and the BIOS ring buffer.
//!
//! The desktop, the serial decoder and host tests produce [`KeyEvent`]s. At the
//! prompt, the shell pops them directly. While a program runs they become what a
//! real keyboard produces: make and break scan codes, one at a time through port
//! `60h` with `IRQ 1`. The BIOS `INT 9` handler turns those into entries in the
//! BDA ring, which is what `INT 16h` and the DOS console calls read. A game that
//! hooks `INT 9` or reads port `60h` sees the same bytes.

use alloc::collections::VecDeque;

use crate::memory::{Memory, BDA_BASE};

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

/// One entry in the host queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HostEvent {
    /// A key went down: an ASCII byte for the BIOS plus its scan code.
    Down(KeyEvent),
    /// A key went up.
    Up(u8),
}

/// Scan codes that real keyboards send with an `E0` prefix.
fn is_extended(scan: u8) -> bool {
    matches!(
        scan,
        0x47 | 0x48 | 0x49 | 0x4B | 0x4D | 0x4F | 0x50 | 0x51 | 0x52 | 0x53
    )
}

/// How long a tap made with [`Keyboard::push`] stays down, in 8254 ticks.
const TAP_TICKS: u64 = 60_000;
/// The gap between scan codes through the controller, in 8254 ticks.
const SCAN_GAP_TICKS: u64 = 1_200;
/// The largest number of host events kept waiting.
const HOST_QUEUE: usize = 64;

/// The keyboard and its controller.
pub struct Keyboard {
    host: VecDeque<HostEvent>,
    /// Scan bytes on their way to port `60h`, with the ASCII of a make code.
    scans: VecDeque<(u8, Option<u8>, u64)>,
    shift: bool,
    ctrl: bool,
    alt: bool,
    caps: bool,
    num: bool,
    scroll: bool,
    insert: bool,
    /// The byte port `60h` returns.
    pub output: u8,
    /// The ASCII byte of the make code now in `output`.
    pub output_ascii: Option<u8>,
    /// Whether the output buffer is full, which is status bit 0 of port `64h`.
    pub output_full: bool,
    /// Don't send anything before this tick.
    next_tick: u64,
    /// Whether the last code in `output` was the `E0` prefix.
    extended_prefix: bool,
    /// The keyboard interface is enabled (commands `AD`/`AE`).
    pub enabled: bool,
    /// The last controller command waiting for a data byte on port `60h`.
    pub pending_command: Option<u8>,
    /// The controller's command byte.
    pub command_byte: u8,
    /// A reply queued for port `60h`.
    pub reply: Option<u8>,
    /// The output port, where bit 1 is the A20 gate.
    pub output_port: u8,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyboard {
    pub fn new() -> Self {
        Self {
            host: VecDeque::new(),
            scans: VecDeque::new(),
            shift: false,
            ctrl: false,
            alt: false,
            caps: false,
            num: false,
            scroll: false,
            insert: false,
            output: 0,
            output_ascii: None,
            output_full: false,
            next_tick: 0,
            extended_prefix: false,
            enabled: true,
            pending_command: None,
            command_byte: 0x45,
            reply: None,
            output_port: 0xCD,
        }
    }

    /// Empties the host queue and anything on the way to port `60h`.
    pub fn clear(&mut self) {
        self.host.clear();
        self.scans.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.host.is_empty()
    }

    pub fn len(&self) -> usize {
        self.host.len()
    }

    fn queue(&mut self, event: HostEvent) {
        if self.host.len() >= HOST_QUEUE {
            self.host.pop_front();
        }
        self.host.push_back(event);
    }

    /// Queues one tap: a press followed shortly by a release.
    pub fn push(&mut self, ascii: u8, scan: u8) {
        let scan = if scan == 0 {
            scan_for_char(ascii as char)
        } else {
            scan
        };
        self.queue(HostEvent::Down(KeyEvent::new(ascii, scan)));
        self.queue(HostEvent::Up(scan));
    }

    /// Queues a press without a release, for hosts that report key-up events.
    pub fn key_down(&mut self, ascii: u8, scan: u8) {
        let scan = if scan == 0 {
            scan_for_char(ascii as char)
        } else {
            scan
        };
        self.queue(HostEvent::Down(KeyEvent::new(ascii, scan)));
    }

    /// Queues a release.
    pub fn key_up(&mut self, scan: u8) {
        self.queue(HostEvent::Up(scan));
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

    /// The next key press, for the prompt. Releases are dropped here.
    pub fn pop(&mut self) -> Option<KeyEvent> {
        loop {
            match self.host.pop_front()? {
                HostEvent::Down(event) => return Some(event),
                HostEvent::Up(_) => {}
            }
        }
    }

    /// Records the host's modifier state. Changes to shift, ctrl and alt also
    /// produce the scan codes a game reading port `60h` expects to see.
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
        for (old, new, scan) in [
            (self.shift, shift, 0x2A),
            (self.ctrl, ctrl, 0x1D),
            (self.alt, alt, 0x38),
        ] {
            if old != new {
                if new {
                    self.queue(HostEvent::Down(KeyEvent::new(0, scan)));
                } else {
                    self.queue(HostEvent::Up(scan));
                }
            }
        }
        self.shift = shift;
        self.ctrl = ctrl;
        self.alt = alt;
        self.caps = caps;
        self.num = num;
        self.scroll = scroll;
        self.insert = insert;
    }

    /// The BIOS keyboard flags as the host last reported them.
    pub fn flags(&self) -> u8 {
        let mut flags = 0;
        if self.shift {
            flags |= 0x02;
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
        self.output
    }

    /// Turns host events into timed scan bytes.
    fn expand_host(&mut self, tick: u64) {
        if !self.scans.is_empty() {
            return;
        }
        let Some(event) = self.host.pop_front() else {
            return;
        };
        let at = tick.max(self.next_tick);
        match event {
            HostEvent::Down(event) => {
                if is_extended(event.scan) {
                    self.scans.push_back((0xE0, None, at));
                }
                self.scans
                    .push_back((event.scan, Some(event.ascii), at + SCAN_GAP_TICKS));
                // A release following immediately is a tap: hold it for a while
                // so a game polling port 60h sees the key down.
                if let Some(HostEvent::Up(scan)) = self.host.front().copied() {
                    if scan == event.scan {
                        self.host.pop_front();
                        let release = at + SCAN_GAP_TICKS + TAP_TICKS;
                        if is_extended(scan) {
                            self.scans.push_back((0xE0, None, release));
                        }
                        self.scans
                            .push_back((scan | 0x80, None, release + SCAN_GAP_TICKS));
                    }
                }
            }
            HostEvent::Up(scan) => {
                if is_extended(scan) {
                    self.scans.push_back((0xE0, None, at));
                }
                self.scans
                    .push_back((scan | 0x80, None, at + SCAN_GAP_TICKS));
            }
        }
    }

    /// Moves the next byte into the output buffer when it's due. Returns true
    /// when `IRQ 1` should be raised. Used while a program runs.
    pub fn pump(&mut self, tick: u64) -> bool {
        if self.output_full || !self.enabled {
            return false;
        }
        if let Some(reply) = self.reply.take() {
            self.output = reply;
            self.output_ascii = None;
            self.output_full = true;
            return true;
        }
        self.expand_host(tick);
        match self.scans.front().copied() {
            Some((scan, ascii, at)) if at <= tick => {
                self.scans.pop_front();
                self.output = scan;
                self.output_ascii = ascii;
                self.output_full = true;
                self.next_tick = tick + SCAN_GAP_TICKS;
                true
            }
            _ => false,
        }
    }

    /// The tick something next has to happen at, if there's anything waiting.
    pub fn next_tick(&self) -> Option<u64> {
        if self.output_full || !self.enabled {
            return None;
        }
        if self.reply.is_some() {
            return Some(0);
        }
        if let Some((_, _, at)) = self.scans.front() {
            return Some((*at).max(self.next_tick));
        }
        if !self.host.is_empty() {
            return Some(self.next_tick);
        }
        None
    }

    /// Whether anything is on its way to the guest.
    pub fn has_pending(&self) -> bool {
        self.output_full || !self.scans.is_empty() || !self.host.is_empty()
    }

    /// A read of port `60h`.
    pub fn read_data(&mut self) -> u8 {
        self.output_full = false;
        self.output
    }

    /// A read of port `64h`.
    pub fn read_status(&self) -> u8 {
        0x14 | u8::from(self.output_full)
    }

    /// A write to port `64h`: a controller command.
    pub fn write_command(&mut self, value: u8) {
        self.pending_command = None;
        match value {
            0x20 => self.reply = Some(self.command_byte),
            0x60 | 0xD1 => self.pending_command = Some(value),
            0xA8 | 0xA7 => {}
            0xA9 | 0xAB => self.reply = Some(0),
            0xAA => self.reply = Some(0x55),
            0xAD => self.enabled = false,
            0xAE => self.enabled = true,
            0xC0 => self.reply = Some(0xFF),
            0xD0 => self.reply = Some(self.output_port),
            _ => {}
        }
    }

    /// A write to port `60h`: data for the controller, or a keyboard command.
    /// Returns the new A20 state if the output port changed it.
    pub fn write_data(&mut self, value: u8) -> Option<bool> {
        match self.pending_command.take() {
            Some(0x60) => {
                self.command_byte = value;
                None
            }
            Some(0xD1) => {
                self.output_port = value;
                Some(value & 2 != 0)
            }
            _ => {
                // Everything the keyboard itself is told gets an ACK, or the
                // reset self-test's pair.
                self.reply = Some(0xFA);
                None
            }
        }
    }

    /// The `INT 9` handler. Processes the byte in the output buffer into the
    /// BDA shift state and ring.
    pub fn bios_int9(&mut self, mem: &mut Memory) {
        let scan = self.read_data();
        if scan == 0xE0 || scan == 0xE1 {
            self.extended_prefix = true;
            mem.write8(BDA_BASE + 0x96, mem.read8(BDA_BASE + 0x96) | 0x02);
            return;
        }
        let extended = core::mem::take(&mut self.extended_prefix);
        mem.write8(BDA_BASE + 0x96, mem.read8(BDA_BASE + 0x96) & !0x02);
        let released = scan & 0x80 != 0;
        let code = scan & 0x7F;
        let mut flags = mem.read8(BDA_BASE + 0x17);
        let mut flags2 = mem.read8(BDA_BASE + 0x18);
        let toggle_bit = match code {
            0x2A => Some((0x02, false)),
            0x36 => Some((0x01, false)),
            0x1D => Some((0x04, false)),
            0x38 => Some((0x08, false)),
            0x46 => Some((0x10, true)),
            0x45 => Some((0x20, true)),
            0x3A => Some((0x40, true)),
            0x52 if !extended => None,
            _ => None,
        };
        if let Some((bit, toggle)) = toggle_bit {
            if toggle {
                if !released && flags2 & bit == 0 {
                    flags ^= bit;
                }
                if released {
                    flags2 &= !bit;
                } else {
                    flags2 |= bit;
                }
            } else if released {
                flags &= !bit;
            } else {
                flags |= bit;
            }
            mem.write8(BDA_BASE + 0x17, flags);
            mem.write8(BDA_BASE + 0x18, flags2);
            return;
        }
        if released {
            return;
        }
        let host_ascii = self.output_ascii.take();
        let word = translate_scan(code, flags, extended, host_ascii);
        if word != 0 {
            bda_push(mem, word);
        }
    }
}

/// Pushes a key word into the BDA ring. Returns false when it's full.
pub fn bda_push(mem: &mut Memory, word: u16) -> bool {
    let start = mem.read16(BDA_BASE + 0x80);
    let end = mem.read16(BDA_BASE + 0x82);
    let (start, end) = if end > start && end - start >= 4 {
        (start, end)
    } else {
        (0x1E, 0x3E)
    };
    let tail = mem.read16(BDA_BASE + 0x1C);
    let mut next = tail.wrapping_add(2);
    if next >= end {
        next = start;
    }
    if next == mem.read16(BDA_BASE + 0x1A) {
        return false;
    }
    mem.write16(BDA_BASE + (tail as u32 & 0xFF), word);
    mem.write16(BDA_BASE + 0x1C, next);
    true
}

/// Reads the next key word from the BDA ring without removing it.
pub fn bda_peek(mem: &Memory) -> Option<u16> {
    let head = mem.read16(BDA_BASE + 0x1A);
    if head == mem.read16(BDA_BASE + 0x1C) {
        return None;
    }
    Some(mem.read16(BDA_BASE + (head as u32 & 0xFF)))
}

/// Removes the next key word from the BDA ring.
pub fn bda_pop(mem: &mut Memory) -> Option<u16> {
    let word = bda_peek(mem)?;
    let start = mem.read16(BDA_BASE + 0x80);
    let end = mem.read16(BDA_BASE + 0x82);
    let (start, end) = if end > start && end - start >= 4 {
        (start, end)
    } else {
        (0x1E, 0x3E)
    };
    let mut next = mem.read16(BDA_BASE + 0x1A).wrapping_add(2);
    if next >= end {
        next = start;
    }
    mem.write16(BDA_BASE + 0x1A, next);
    Some(word)
}

/// Empties the BDA ring.
pub fn bda_flush(mem: &mut Memory) {
    let tail = mem.read16(BDA_BASE + 0x1C);
    mem.write16(BDA_BASE + 0x1A, tail);
}

/// The key word the BIOS stores for a make code.
fn translate_scan(code: u8, flags: u8, extended: bool, host_ascii: Option<u8>) -> u16 {
    let word = |ascii: u8, scan: u8| ((scan as u16) << 8) | ascii as u16;
    let shift = flags & 0x03 != 0;
    let ctrl = flags & 0x04 != 0;
    let alt = flags & 0x08 != 0;
    if let Some(ascii) = host_ascii {
        if ascii != 0 || !matches!(code, 0x1D | 0x2A | 0x36 | 0x38) {
            if ascii != 0 && alt {
                return word(0, code);
            }
            if ascii != 0 {
                return word(ascii, code);
            }
            if extended {
                return word(0xE0, code);
            }
            return word(0, code);
        }
    }
    if extended {
        return word(0xE0, code);
    }
    const UNSHIFTED: &[u8; 58] =
        b"\0\x1B1234567890-=\x08\tqwertyuiop[]\r\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
    const SHIFTED: &[u8; 58] =
        b"\0\x1B!@#$%^&*()_+\x08\tQWERTYUIOP{}\r\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";
    match code as usize {
        index @ 1..=57 => {
            let mut ascii = if shift {
                SHIFTED[index]
            } else {
                UNSHIFTED[index]
            };
            if ascii == 0 {
                return 0;
            }
            if flags & 0x40 != 0 && ascii.is_ascii_alphabetic() {
                ascii ^= 0x20;
            }
            if alt {
                return word(0, code);
            }
            if ctrl {
                if ascii.is_ascii_alphabetic() {
                    return word(ascii & 0x1F, code);
                }
                return word(0, code);
            }
            word(ascii, code)
        }
        0x3B..=0x44 | 0x47..=0x53 | 0x57 | 0x58 => word(0, code),
        _ => 0,
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
        '1' | '!' => 0x02,
        '2' | '@' => 0x03,
        '3' | '#' => 0x04,
        '4' | '$' => 0x05,
        '5' | '%' => 0x06,
        '6' | '^' => 0x07,
        '7' | '&' => 0x08,
        '8' => 0x09,
        '9' | '(' => 0x0A,
        '0' | ')' => 0x0B,
        ' ' => 0x39,
        '\n' | '\r' => 0x1C,
        '\t' => 0x0F,
        '\u{8}' => 0x0E,
        '\u{1b}' => 0x01,
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

/// Sets up the BDA keyboard fields at boot.
pub fn bda_init(mem: &mut Memory) {
    mem.write16(BDA_BASE + 0x1A, 0x1E);
    mem.write16(BDA_BASE + 0x1C, 0x1E);
    mem.write16(BDA_BASE + 0x80, 0x1E);
    mem.write16(BDA_BASE + 0x82, 0x3E);
    mem.write8(BDA_BASE + 0x96, 0x10);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(keyboard: &mut Keyboard, mem: &mut Memory) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec::Vec::new();
        let mut tick = 0;
        for _ in 0..16 {
            match keyboard.next_tick() {
                Some(next) => tick = tick.max(next),
                None => break,
            }
            if keyboard.pump(tick) {
                out.push(keyboard.output);
                keyboard.bios_int9(mem);
            }
            tick += 1;
        }
        out
    }

    #[test]
    fn prompt_queue_reports_scan_and_ascii() {
        let mut keyboard = Keyboard::new();
        keyboard.push_ascii(b'a', scan_for_char('a'));
        let event = keyboard.pop().expect("key");
        assert_eq!(event.ascii, b'a');
        assert_eq!(event.scan, 0x1E);
        assert!(keyboard.pop().is_none());
    }

    #[test]
    fn tap_produces_make_and_break_and_fills_the_ring() {
        let mut mem = Memory::new();
        bda_init(&mut mem);
        let mut keyboard = Keyboard::new();
        keyboard.push(b'a', 0x1E);
        assert_eq!(drain(&mut keyboard, &mut mem), [0x1E, 0x9E]);
        assert_eq!(bda_pop(&mut mem), Some(0x1E61));
        assert_eq!(bda_pop(&mut mem), None);
    }

    #[test]
    fn arrows_use_the_extended_prefix_and_keep_their_scan_code() {
        let mut mem = Memory::new();
        bda_init(&mut mem);
        let mut keyboard = Keyboard::new();
        keyboard.push_key(Key::Up);
        assert_eq!(drain(&mut keyboard, &mut mem), [0xE0, 0x48, 0xE0, 0xC8]);
        // The ring keeps the grey-key marker, which INT 16h's old calls hide.
        assert_eq!(bda_pop(&mut mem), Some(0x48E0));
    }

    #[test]
    fn modifiers_reach_port_60_and_the_bda_flags() {
        let mut mem = Memory::new();
        bda_init(&mut mem);
        let mut keyboard = Keyboard::new();
        keyboard.set_modifiers(true, false, false, false, false, false, false);
        assert_eq!(drain(&mut keyboard, &mut mem), [0x2A]);
        assert_eq!(mem.read8(BDA_BASE + 0x17) & 0x03, 0x02);
        keyboard.set_modifiers(false, false, false, false, false, false, false);
        assert_eq!(drain(&mut keyboard, &mut mem), [0xAA]);
        assert_eq!(mem.read8(BDA_BASE + 0x17) & 0x03, 0);
        assert_eq!(keyboard.flags() & 0x03, 0);
    }

    #[test]
    fn ring_refuses_when_full() {
        let mut mem = Memory::new();
        bda_init(&mut mem);
        let mut pushed = 0;
        while bda_push(&mut mem, 0x1E61) {
            pushed += 1;
            assert!(pushed < 32);
        }
        assert_eq!(pushed, 15);
    }

    #[test]
    fn untagged_scan_codes_translate_through_the_shift_state() {
        assert_eq!(translate_scan(0x1E, 0, false, None), 0x1E61);
        assert_eq!(translate_scan(0x1E, 0x02, false, None), 0x1E41);
        assert_eq!(translate_scan(0x1E, 0x04, false, None), 0x1E01);
        assert_eq!(translate_scan(0x1E, 0x08, false, None), 0x1E00);
        assert_eq!(translate_scan(0x3B, 0, false, None), 0x3B00);
    }
}
