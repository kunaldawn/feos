//! Turns keyboard input into key presses and [`feos_gui::Input`] events.
//!
//! The base of this crate is [`ByteDecoder`], which decodes the bytes a terminal
//! sends: arrows as `ESC [ A` or `ESC O A`, function keys, Page Up/Down, Insert,
//! Delete, Ctrl/Alt/Shift chords and UTF-8 text. A lone `Escape` can only be told
//! from a sequence after a short timeout, which is the caller's to enforce. The
//! shell's line editor uses it directly.
//!
//! With the default `gui` feature, two sources also feed the GUI:
//!
//! - [`SerialKeys`] wraps the decoder for the serial console, which remains a
//!   keyboard even on boards without an input device.
//! - [`EvdevKeys`] decodes Linux evdev key transitions from a virtio keyboard,
//!   so QEMU's graphical window can type into the desktop and terminal.
//!
//! [`SerialKeys`] owns that decoder: a caller polls it once per frame, passes
//! the resulting [`Input`] to its UI, and clears the input's events when the
//! frame is done.
//!
//! ```no_run
//! # #[cfg(feature = "gui")]
//! # {
//! use feos_gui::Input;
//! use feos_keys::SerialKeys;
//!
//! let mut keys = SerialKeys::new();
//! let mut input = Input::new();
//! let leave = keys.poll(0, &mut input);
//! # }
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(feature = "gui")]
use feos_console::read_byte;
#[cfg(feature = "gui")]
use feos_gui::{Event, Input, Key, Modifiers};
#[cfg(feature = "gui")]
use feos_hal::KeyboardEvent;

/// Maximum time spent waiting for the rest of an escape sequence, in ms.
pub const ESCAPE_TIMEOUT_MS: u64 = 30;

/// Ctrl-\ on a serial terminal: leave a full-screen session.
pub const EXIT_BYTE: u8 = 0x1c;

/// Ctrl-D, accepted as a convenience because some setups reserve Ctrl-\.
pub const EXIT_BYTE_ALT: u8 = 0x04;

/// A key, with no regard for how it was typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    /// A printable character, or a letter when Ctrl is held.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Escape,
    /// A function key, F1 being 1.
    Function(u8),
}

/// A key with its modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyPress {
    /// The key.
    pub code: KeyCode,
    /// Shift was held.
    pub shift: bool,
    /// Ctrl was held.
    pub ctrl: bool,
    /// Alt was held.
    pub alt: bool,
}

impl KeyPress {
    /// A key with no modifiers.
    pub const fn plain(code: KeyCode) -> Self {
        Self {
            code,
            shift: false,
            ctrl: false,
            alt: false,
        }
    }

    /// Ctrl plus a character.
    pub const fn ctrl(ch: char) -> Self {
        Self {
            code: KeyCode::Char(ch),
            shift: false,
            ctrl: true,
            alt: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DecodeState {
    Ground,
    Escape,
    ControlSequence,
    /// `ESC O` takes one letter.
    SingleShift,
    Utf8,
}

/// Decodes terminal bytes into key presses.
#[derive(Clone, Copy)]
pub struct ByteDecoder {
    state: DecodeState,
    params: [u16; 6],
    count: usize,
    current: u16,
    has_current: bool,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

impl ByteDecoder {
    /// Creates a decoder in the idle state.
    pub const fn new() -> Self {
        Self {
            state: DecodeState::Ground,
            params: [0; 6],
            count: 0,
            current: 0,
            has_current: false,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        }
    }

    /// True when the next byte continues a sequence already started.
    pub const fn is_pending(&self) -> bool {
        !matches!(self.state, DecodeState::Ground)
    }

    /// True right after a bare `ESC`, the one state where silence means a key.
    const fn awaiting_escape(&self) -> bool {
        matches!(self.state, DecodeState::Escape)
    }

    /// True in the idle state.
    pub const fn is_idle(&self) -> bool {
        matches!(self.state, DecodeState::Ground)
    }

    /// Ends a sequence that will not be continued: a lone `ESC` becomes Escape,
    /// and a partial sequence is dropped.
    pub fn flush(&mut self, emit: &mut dyn FnMut(KeyPress)) {
        if self.awaiting_escape() {
            emit(KeyPress::plain(KeyCode::Escape));
        }
        self.state = DecodeState::Ground;
        self.reset_params();
        self.utf8_need = 0;
        self.utf8_len = 0;
    }

    fn reset_params(&mut self) {
        self.params = [0; 6];
        self.count = 0;
        self.current = 0;
        self.has_current = false;
    }

    fn param(&self, index: usize) -> u16 {
        if index < self.count {
            self.params[index]
        } else if index == self.count && self.has_current {
            self.current
        } else {
            0
        }
    }

    /// Feeds one byte, calling `emit` for each finished key.
    pub fn feed(&mut self, byte: u8, emit: &mut dyn FnMut(KeyPress)) {
        match self.state {
            DecodeState::Ground => self.ground(byte, emit),
            DecodeState::Escape => match byte {
                b'[' => {
                    self.state = DecodeState::ControlSequence;
                    self.reset_params();
                }
                b'O' => {
                    self.state = DecodeState::SingleShift;
                    self.reset_params();
                }
                // Not a sequence: an xterm reports Alt+key as ESC then the key.
                b'\r' | b'\n' => self.alt(KeyCode::Enter, emit),
                b'\t' => self.alt(KeyCode::Tab, emit),
                0x7f | 0x08 => self.alt(KeyCode::Backspace, emit),
                0x20..=0x7e => self.alt(KeyCode::Char(byte as char), emit),
                _ => {
                    self.state = DecodeState::Ground;
                    emit(KeyPress::plain(KeyCode::Escape));
                    self.ground(byte, emit);
                }
            },
            DecodeState::SingleShift => {
                self.state = DecodeState::Ground;
                if let Some(code) = final_key(byte) {
                    emit(KeyPress::plain(code));
                }
            }
            DecodeState::ControlSequence => match byte {
                b'0'..=b'9' => {
                    self.current = self
                        .current
                        .saturating_mul(10)
                        .saturating_add(u16::from(byte - b'0'));
                    self.has_current = true;
                }
                b';' => {
                    if self.count < self.params.len() {
                        self.params[self.count] = self.current;
                        self.count += 1;
                    }
                    self.current = 0;
                    self.has_current = false;
                }
                0x40..=0x7e => {
                    self.state = DecodeState::Ground;
                    self.finish_sequence(byte, emit);
                    self.reset_params();
                }
                0x1b => {
                    self.state = DecodeState::Escape;
                    self.reset_params();
                }
                _ => {}
            },
            DecodeState::Utf8 => {
                if byte & 0xc0 != 0x80 {
                    self.state = DecodeState::Ground;
                    self.utf8_need = 0;
                    self.utf8_len = 0;
                    self.ground(byte, emit);
                    return;
                }
                self.utf8[self.utf8_len] = byte;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_need {
                    self.state = DecodeState::Ground;
                    let decoded = core::str::from_utf8(&self.utf8[..self.utf8_len])
                        .ok()
                        .and_then(|text| text.chars().next());
                    self.utf8_need = 0;
                    self.utf8_len = 0;
                    if let Some(ch) = decoded {
                        emit(KeyPress::plain(KeyCode::Char(ch)));
                    }
                }
            }
        }
    }

    fn alt(&mut self, code: KeyCode, emit: &mut dyn FnMut(KeyPress)) {
        self.state = DecodeState::Ground;
        emit(KeyPress {
            alt: true,
            ..KeyPress::plain(code)
        });
    }

    fn ground(&mut self, byte: u8, emit: &mut dyn FnMut(KeyPress)) {
        let control = |ch: char| Some(KeyPress::ctrl(ch));
        let press = match byte {
            0x1b => {
                self.state = DecodeState::Escape;
                None
            }
            b'\r' | b'\n' => Some(KeyPress::plain(KeyCode::Enter)),
            b'\t' => Some(KeyPress::plain(KeyCode::Tab)),
            0x7f | 0x08 => Some(KeyPress::plain(KeyCode::Backspace)),
            0x00 => control(' '),
            0x01..=0x1a => control((b'a' + byte - 1) as char),
            0x1c => control('\\'),
            0x1d => control('='),
            0x1e => control('^'),
            0x1f => control('-'),
            0x20..=0x7e => Some(KeyPress::plain(KeyCode::Char(byte as char))),
            0xc2..=0xdf => self.begin_utf8(byte, 2),
            0xe0..=0xef => self.begin_utf8(byte, 3),
            0xf0..=0xf4 => self.begin_utf8(byte, 4),
            _ => None,
        };
        if let Some(press) = press {
            emit(press);
        }
    }

    fn begin_utf8(&mut self, byte: u8, need: usize) -> Option<KeyPress> {
        self.utf8[0] = byte;
        self.utf8_len = 1;
        self.utf8_need = need;
        self.state = DecodeState::Utf8;
        None
    }

    fn finish_sequence(&mut self, final_byte: u8, emit: &mut dyn FnMut(KeyPress)) {
        let first = self.param(0);
        let bits = self.param(1).saturating_sub(1);
        let code = match final_byte {
            b'Z' => {
                emit(KeyPress {
                    shift: true,
                    ..KeyPress::plain(KeyCode::Tab)
                });
                return;
            }
            b'~' => match first {
                1 | 7 => Some(KeyCode::Home),
                2 => Some(KeyCode::Insert),
                3 => Some(KeyCode::Delete),
                4 | 8 => Some(KeyCode::End),
                5 => Some(KeyCode::PageUp),
                6 => Some(KeyCode::PageDown),
                11..=15 => Some(KeyCode::Function((first - 10) as u8)),
                17..=21 => Some(KeyCode::Function((first - 11) as u8)),
                23 | 24 => Some(KeyCode::Function((first - 12) as u8)),
                _ => None,
            },
            other => final_key(other),
        };
        if let Some(code) = code {
            emit(KeyPress {
                code,
                shift: bits & 1 != 0,
                alt: bits & 2 != 0,
                ctrl: bits & 4 != 0,
            });
        }
    }
}

impl Default for ByteDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// The keys whose sequences end in one letter, after `ESC [` or `ESC O`.
fn final_key(byte: u8) -> Option<KeyCode> {
    Some(match byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::Function(1),
        b'Q' => KeyCode::Function(2),
        b'R' => KeyCode::Function(3),
        b'S' => KeyCode::Function(4),
        _ => return None,
    })
}

/// Bytes held between frames. A frame's event queue fills long before a paste
/// ends, and a UART's FIFO cannot hold the difference.
#[cfg(feature = "gui")]
const PENDING_BYTES: usize = 4096;

/// Events one byte can queue: a key, its text and its release, and the same of
/// an Escape before them.
#[cfg(feature = "gui")]
const EVENTS_PER_BYTE: usize = 6;

/// Decodes serial bytes into GUI input events.
#[cfg(feature = "gui")]
pub struct SerialKeys {
    decoder: ByteDecoder,
    escape_at: u64,
    exit: bool,
    ctrl_d_exits: bool,
    pending: [u8; PENDING_BYTES],
    head: usize,
    count: usize,
}

#[cfg(feature = "gui")]
impl SerialKeys {
    /// Creates a decoder in the idle state. Ctrl-D and Ctrl-\ leave the session.
    pub const fn new() -> Self {
        Self {
            decoder: ByteDecoder::new(),
            escape_at: 0,
            exit: false,
            ctrl_d_exits: true,
            pending: [0; PENDING_BYTES],
            head: 0,
            count: 0,
        }
    }

    /// Delivers Ctrl-D as a key instead of leaving the session, for a session
    /// that hosts windows wanting it. Ctrl-\ still leaves.
    pub const fn with_ctrl_d_passthrough(mut self) -> Self {
        self.ctrl_d_exits = false;
        self
    }

    fn push(input: &mut Input, press: KeyPress) {
        let mut modifiers = Modifiers::NONE;
        if press.shift {
            modifiers |= Modifiers::SHIFT;
        }
        if press.ctrl {
            modifiers |= Modifiers::CTRL;
        }
        if press.alt {
            modifiers |= Modifiers::ALT;
        }
        let key = match press.code {
            KeyCode::Char(' ') => Key::Space,
            KeyCode::Char(ch) => Key::Character(ch),
            KeyCode::Enter => Key::Enter,
            KeyCode::Tab => Key::Tab,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Delete => Key::Delete,
            KeyCode::Insert => Key::Insert,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Escape => Key::Escape,
            KeyCode::Function(number) => match function_key(number) {
                Some(key) => key,
                None => return,
            },
        };
        input.push(Event::KeyDown(key, modifiers));
        if let KeyCode::Char(ch) = press.code {
            if !press.ctrl && !press.alt {
                input.push(Event::Text(ch));
            }
        }
        // A serial line only reports presses, so each one is also its release.
        input.push(Event::KeyUp(key, modifiers));
    }

    /// Adds a byte to the queue, dropping it when the queue is full.
    fn enqueue(&mut self, byte: u8) {
        if self.count < PENDING_BYTES {
            let tail = (self.head + self.count) % PENDING_BYTES;
            self.pending[tail] = byte;
            self.count += 1;
        }
    }

    /// Decodes queued bytes until the frame's event queue is close to full. The
    /// rest stays queued for the next frame.
    fn drain_pending(&mut self, now: u64, input: &mut Input) {
        while self.count > 0 && input.keyboard_capacity() >= EVENTS_PER_BYTE {
            let byte = self.pending[self.head];
            self.head = (self.head + 1) % PENDING_BYTES;
            self.count -= 1;
            self.feed(byte, now, input);
        }
    }

    /// Drains the console and queues this frame's events.
    ///
    /// Returns `true` when the user asked to leave the session with Ctrl-\ or
    /// Ctrl-D. The flag is sticky once set.
    pub fn poll(&mut self, now: u64, input: &mut Input) -> bool {
        while let Some(byte) = read_byte() {
            self.enqueue(byte);
        }
        self.drain_pending(now, input);

        if self.count == 0
            && self.decoder.is_pending()
            && now.wrapping_sub(self.escape_at) >= ESCAPE_TIMEOUT_MS
        {
            self.decoder
                .flush(&mut |press| Self::push(input, press));
        }

        self.exit
    }

    /// Feeds one byte. Exposed for host tests and custom byte sources.
    pub fn feed(&mut self, byte: u8, now: u64, input: &mut Input) {
        let leaves = byte == EXIT_BYTE || (byte == EXIT_BYTE_ALT && self.ctrl_d_exits);
        if leaves && (self.decoder.is_idle() || self.decoder.awaiting_escape()) {
            self.decoder.flush(&mut |press| Self::push(input, press));
            self.exit = true;
            return;
        }
        self.escape_at = now;
        self.decoder.feed(byte, &mut |press| Self::push(input, press));
    }
}

#[cfg(feature = "gui")]
impl Default for SerialKeys {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "gui")]
fn function_key(number: u8) -> Option<Key> {
    Some(match number {
        1 => Key::F1,
        2 => Key::F2,
        3 => Key::F3,
        4 => Key::F4,
        5 => Key::F5,
        6 => Key::F6,
        7 => Key::F7,
        8 => Key::F8,
        9 => Key::F9,
        10 => Key::F10,
        11 => Key::F11,
        12 => Key::F12,
        _ => return None,
    })
}

/// Converts a GUI key event for code that works in [`KeyPress`]es.
#[cfg(feature = "gui")]
pub fn key_press_from_gui(key: Key, modifiers: Modifiers) -> Option<KeyPress> {
    let code = match key {
        Key::Character(ch) => KeyCode::Char(ch),
        Key::Space => KeyCode::Char(' '),
        Key::Enter => KeyCode::Enter,
        Key::Tab => KeyCode::Tab,
        Key::Backspace => KeyCode::Backspace,
        Key::Delete => KeyCode::Delete,
        Key::Insert => KeyCode::Insert,
        Key::Left => KeyCode::Left,
        Key::Right => KeyCode::Right,
        Key::Up => KeyCode::Up,
        Key::Down => KeyCode::Down,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,
        Key::Escape => KeyCode::Escape,
        _ => return None,
    };
    Some(KeyPress {
        code,
        shift: modifiers.contains(Modifiers::SHIFT),
        ctrl: modifiers.contains(Modifiers::CTRL),
        alt: modifiers.contains(Modifiers::ALT),
    })
}

/// Decodes Linux evdev key transitions from a virtio keyboard into GUI events.
///
/// The layout is US/QWERTY. Modifier state is tracked across presses and
/// releases, Caps Lock toggles on press, and Ctrl+letter produces a key event
/// without a matching text event so the terminal can implement Ctrl-C, Ctrl-D
/// and friends.
///
/// Each side of a modifier is tracked apart, so releasing one of two held Shift
/// keys leaves Shift down.
#[derive(Clone, Copy, Debug, Default)]
#[cfg(feature = "gui")]
pub struct EvdevKeys {
    shift: u8,
    ctrl: u8,
    alt: u8,
    caps: bool,
}

#[cfg(feature = "gui")]
impl EvdevKeys {
    /// Creates a decoder with no modifiers held.
    pub const fn new() -> Self {
        Self {
            shift: 0,
            ctrl: 0,
            alt: 0,
            caps: false,
        }
    }

    fn key(input: &mut Input, key: Key, modifiers: Modifiers) {
        input.push(Event::KeyDown(key, modifiers));
    }

    fn text(input: &mut Input, ch: char) {
        input.push(Event::Text(ch));
    }

    fn modifiers(&self) -> Modifiers {
        let mut modifiers = Modifiers::NONE;
        if self.shift != 0 {
            modifiers |= Modifiers::SHIFT;
        }
        if self.ctrl != 0 {
            modifiers |= Modifiers::CTRL;
        }
        if self.alt != 0 {
            modifiers |= Modifiers::ALT;
        }
        modifiers
    }

    /// Feeds one key transition.
    pub fn feed(&mut self, event: KeyboardEvent, input: &mut Input) {
        match event.code {
            42 | 54 => {
                set_side(&mut self.shift, event.code == 54, event.pressed);
                input.push(Event::ModifiersChanged(self.modifiers()));
                return;
            }
            29 | 97 => {
                set_side(&mut self.ctrl, event.code == 97, event.pressed);
                input.push(Event::ModifiersChanged(self.modifiers()));
                return;
            }
            56 | 100 => {
                set_side(&mut self.alt, event.code == 100, event.pressed);
                input.push(Event::ModifiersChanged(self.modifiers()));
                return;
            }
            58 => {
                if event.pressed {
                    self.caps = !self.caps;
                }
                return;
            }
            _ => {}
        }

        let modifiers = self.modifiers();

        if !event.pressed {
            // A release names the key, whatever Shift says now: a key pressed
            // with Shift can be released without it.
            let key = special_key(event.code)
                .or_else(|| character(event.code).map(|(plain, _)| Key::Character(plain)));
            if let Some(key) = key {
                input.push(Event::KeyUp(key, modifiers));
            }
            return;
        }

        if let Some(key) = special_key(event.code) {
            if key == Key::Space && self.ctrl == 0 && self.alt == 0 {
                Self::text(input, ' ');
            }
            Self::key(input, key, modifiers);
            return;
        }

        if let Some((plain, shifted)) = character(event.code) {
            if self.ctrl != 0 {
                Self::key(input, Key::Character(plain), modifiers);
                return;
            }

            let upper = if plain.is_ascii_alphabetic() {
                self.caps ^ (self.shift != 0)
            } else {
                self.shift != 0
            };
            let ch = if upper { shifted } else { plain };
            Self::key(input, Key::Character(ch), modifiers);
            Self::text(input, ch);
        }
    }
}

/// Records one side of a modifier: bit 0 is the left key, bit 1 the right.
#[cfg(feature = "gui")]
fn set_side(state: &mut u8, right: bool, pressed: bool) {
    let bit = if right { 2 } else { 1 };
    if pressed {
        *state |= bit;
    } else {
        *state &= !bit;
    }
}

#[cfg(feature = "gui")]
fn special_key(code: u16) -> Option<Key> {
    match code {
        1 => Some(Key::Escape),
        14 => Some(Key::Backspace),
        15 => Some(Key::Tab),
        28 | 96 => Some(Key::Enter),
        57 => Some(Key::Space),
        59 => Some(Key::F1),
        60 => Some(Key::F2),
        61 => Some(Key::F3),
        62 => Some(Key::F4),
        63 => Some(Key::F5),
        64 => Some(Key::F6),
        65 => Some(Key::F7),
        66 => Some(Key::F8),
        67 => Some(Key::F9),
        68 => Some(Key::F10),
        87 => Some(Key::F11),
        88 => Some(Key::F12),
        102 => Some(Key::Home),
        103 => Some(Key::Up),
        104 => Some(Key::PageUp),
        105 => Some(Key::Left),
        106 => Some(Key::Right),
        107 => Some(Key::End),
        108 => Some(Key::Down),
        109 => Some(Key::PageDown),
        110 => Some(Key::Insert),
        111 => Some(Key::Delete),
        _ => None,
    }
}

#[cfg(feature = "gui")]
fn character(code: u16) -> Option<(char, char)> {
    let pair = match code {
        2 => ('1', '!'),
        3 => ('2', '@'),
        4 => ('3', '#'),
        5 => ('4', '$'),
        6 => ('5', '%'),
        7 => ('6', '^'),
        8 => ('7', '&'),
        9 => ('8', '*'),
        10 => ('9', '('),
        11 => ('0', ')'),
        12 => ('-', '_'),
        13 => ('=', '+'),
        16 => ('q', 'Q'),
        17 => ('w', 'W'),
        18 => ('e', 'E'),
        19 => ('r', 'R'),
        20 => ('t', 'T'),
        21 => ('y', 'Y'),
        22 => ('u', 'U'),
        23 => ('i', 'I'),
        24 => ('o', 'O'),
        25 => ('p', 'P'),
        26 => ('[', '{'),
        27 => (']', '}'),
        30 => ('a', 'A'),
        31 => ('s', 'S'),
        32 => ('d', 'D'),
        33 => ('f', 'F'),
        34 => ('g', 'G'),
        35 => ('h', 'H'),
        36 => ('j', 'J'),
        37 => ('k', 'K'),
        38 => ('l', 'L'),
        39 => (';', ':'),
        40 => ('\'', '"'),
        41 => ('`', '~'),
        43 => ('\\', '|'),
        44 => ('z', 'Z'),
        45 => ('x', 'X'),
        46 => ('c', 'C'),
        47 => ('v', 'V'),
        48 => ('b', 'B'),
        49 => ('n', 'N'),
        50 => ('m', 'M'),
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        55 => ('*', '*'),
        71 => ('7', '7'),
        72 => ('8', '8'),
        73 => ('9', '9'),
        74 => ('-', '-'),
        75 => ('4', '4'),
        76 => ('5', '5'),
        77 => ('6', '6'),
        78 => ('+', '+'),
        79 => ('1', '1'),
        80 => ('2', '2'),
        81 => ('3', '3'),
        82 => ('0', '0'),
        83 => ('.', '.'),
        98 => ('/', '/'),
        _ => return None,
    };
    Some(pair)
}

#[cfg(all(test, feature = "gui"))]
mod tests {
    use super::*;

    fn feed_all(keys: &mut SerialKeys, bytes: &[u8]) -> Input {
        let mut input = Input::new();
        for byte in bytes {
            keys.feed(*byte, 0, &mut input);
        }
        input
    }

    #[test]
    fn terminal_arrow_sequences_become_navigation_keys() {
        let mut keys = SerialKeys::new();
        assert!(feed_all(&mut keys, b"\x1b[A")
            .keys()
            .any(|(key, _)| key == Key::Up));

        let mut keys = SerialKeys::new();
        assert!(feed_all(&mut keys, b"\x1bOB")
            .keys()
            .any(|(key, _)| key == Key::Down));
    }

    #[test]
    fn enter_and_ctrl_backslash_are_decoded() {
        let mut keys = SerialKeys::new();
        let mut input = Input::new();
        keys.feed(b'\r', 0, &mut input);
        assert!(input.keys().any(|(key, _)| key == Key::Enter));
        assert!(!keys.exit);

        keys.feed(EXIT_BYTE, 0, &mut input);
        assert!(keys.exit);
    }

    #[test]
    fn modified_arrows_and_function_keys_are_decoded() {
        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"\x1b[1;5C");
        assert!(input
            .keys()
            .any(|(key, modifiers)| key == Key::Right && modifiers.contains(Modifiers::CTRL)));

        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"\x1b[5~");
        assert!(input.keys().any(|(key, _)| key == Key::PageUp));

        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"\x1b[15~");
        assert!(input.keys().any(|(key, _)| key == Key::F5));

        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"\x1b[3~");
        assert!(input.keys().any(|(key, _)| key == Key::Delete));
    }

    #[test]
    fn escape_prefixed_keys_become_alt_chords() {
        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"\x1b\r");
        assert!(input
            .keys()
            .any(|(key, modifiers)| key == Key::Enter && modifiers.contains(Modifiers::ALT)));

        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"\x1bf");
        assert!(input.keys().any(
            |(key, modifiers)| key == Key::Character('f') && modifiers.contains(Modifiers::ALT)
        ));
    }

    #[test]
    fn control_letters_arrive_without_text_events() {
        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"\x01");
        assert!(input
            .keys()
            .any(|(key, modifiers)| key == Key::Character('a')
                && modifiers.contains(Modifiers::CTRL)));
        assert_eq!(input.text().count(), 0);
    }

    #[test]
    fn serial_control_equals_and_minus_are_zoom_keys() {
        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"\x1d\x1f");
        assert!(input
            .keys()
            .any(|(key, modifiers)| key == Key::Character('=')
                && modifiers.contains(Modifiers::CTRL)));
        assert!(input
            .keys()
            .any(|(key, modifiers)| key == Key::Character('-')
                && modifiers.contains(Modifiers::CTRL)));
    }

    #[test]
    fn evdev_decodes_letters_shift_and_caps() {
        let mut keys = EvdevKeys::new();
        let mut input = Input::new();
        keys.feed(
            KeyboardEvent {
                code: 30,
                pressed: true,
            },
            &mut input,
        );
        assert!(input.text().any(|ch| ch == 'a'));

        input.clear_events();
        keys.feed(
            KeyboardEvent {
                code: 42,
                pressed: true,
            },
            &mut input,
        );
        keys.feed(
            KeyboardEvent {
                code: 30,
                pressed: true,
            },
            &mut input,
        );
        assert!(input.text().any(|ch| ch == 'A'));

        input.clear_events();
        keys.feed(
            KeyboardEvent {
                code: 42,
                pressed: false,
            },
            &mut input,
        );
        keys.feed(
            KeyboardEvent {
                code: 58,
                pressed: true,
            },
            &mut input,
        );
        keys.feed(
            KeyboardEvent {
                code: 30,
                pressed: true,
            },
            &mut input,
        );
        assert!(input.text().any(|ch| ch == 'A'));
    }

    #[test]
    fn evdev_ctrl_letters_have_no_text() {
        let mut keys = EvdevKeys::new();
        let mut input = Input::new();
        keys.feed(
            KeyboardEvent {
                code: 29,
                pressed: true,
            },
            &mut input,
        );
        keys.feed(
            KeyboardEvent {
                code: 46,
                pressed: true,
            },
            &mut input,
        );
        assert!(input
            .keys()
            .any(|(key, modifiers)| key == Key::Character('c')
                && modifiers.contains(Modifiers::CTRL)));
        assert_eq!(input.text().count(), 0);
    }

    #[test]
    fn f1_to_f4_arrive_in_their_short_forms_with_modifiers() {
        let mut keys = SerialKeys::new();
        let mut input = Input::new();
        for byte in *b"\x1b[1;3S" {
            keys.feed(byte, 0, &mut input);
        }
        assert_eq!(input.keys().next(), Some((Key::F4, Modifiers::ALT)));
    }

    #[test]
    fn xterm_function_keys_arrive_in_both_encodings() {
        let mut keys = SerialKeys::new();
        assert!(feed_all(&mut keys, b"\x1bOP").keys().any(|(key, _)| key == Key::F1));
        assert!(feed_all(&mut keys, b"\x1bOS").keys().any(|(key, _)| key == Key::F4));
        assert!(feed_all(&mut keys, b"\x1b[1;2Q").keys().any(
            |(key, modifiers)| key == Key::F2 && modifiers.contains(Modifiers::SHIFT)
        ));
        assert!(feed_all(&mut keys, b"\x1b[11~").keys().any(|(key, _)| key == Key::F1));
    }

    #[test]
    fn releasing_one_of_two_held_modifiers_keeps_it_down() {
        let mut keys = EvdevKeys::new();
        let mut input = Input::new();
        let event = |code, pressed| KeyboardEvent { code, pressed };
        keys.feed(event(42, true), &mut input);
        keys.feed(event(54, true), &mut input);
        keys.feed(event(42, false), &mut input);
        keys.feed(event(30, true), &mut input);
        assert!(input.text().any(|ch| ch == 'A'));
        input.clear_events();
        keys.feed(event(54, false), &mut input);
        keys.feed(event(30, true), &mut input);
        assert!(input.text().any(|ch| ch == 'a'));
    }

    #[test]
    fn serial_text_decodes_utf8() {
        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, "é".as_bytes());
        assert!(input.text().any(|ch| ch == 'é'));
    }

    #[test]
    fn ctrl_d_can_reach_the_focused_app() {
        let mut keys = SerialKeys::new();
        feed_all(&mut keys, b"\x04");
        assert!(keys.exit);

        let mut keys = SerialKeys::new().with_ctrl_d_passthrough();
        let input = feed_all(&mut keys, b"\x04");
        assert!(!keys.exit);
        assert!(input
            .keys()
            .any(|(key, modifiers)| key == Key::Character('d')
                && modifiers.contains(Modifiers::CTRL)));
    }

    #[test]
    fn input_beyond_a_full_frame_waits_for_the_next_one() {
        let mut keys = SerialKeys::new();
        for byte in 0..200u8 {
            keys.enqueue(b'a' + byte % 26);
        }
        let mut seen = 0usize;
        for frame in 0..40 {
            let mut input = Input::new();
            keys.drain_pending(frame, &mut input);
            let count = input.text().count();
            assert!(count > 0 || seen == 200);
            seen += count;
        }
        assert_eq!(seen, 200);
    }

    #[test]
    fn gui_keys_convert_for_the_line_editor() {
        let press = key_press_from_gui(Key::Character('r'), Modifiers::CTRL).unwrap();
        assert_eq!(press, KeyPress::ctrl('r'));
        let press = key_press_from_gui(Key::Up, Modifiers::NONE).unwrap();
        assert_eq!(press.code, KeyCode::Up);
        assert_eq!(
            key_press_from_gui(Key::Space, Modifiers::NONE).unwrap().code,
            KeyCode::Char(' ')
        );
    }

    #[test]
    fn evdev_releases_become_key_up_events() {
        let mut keys = EvdevKeys::new();
        let mut input = Input::new();
        let press = |code, pressed| KeyboardEvent { code, pressed };
        // Shift+A, with Shift let go first.
        keys.feed(press(42, true), &mut input);
        keys.feed(press(30, true), &mut input);
        keys.feed(press(42, false), &mut input);
        keys.feed(press(30, false), &mut input);
        keys.feed(press(103, true), &mut input);
        keys.feed(press(103, false), &mut input);
        let ups: std::vec::Vec<Key> = input
            .events()
            .filter_map(|event| match event {
                Event::KeyUp(key, _) => Some(key),
                _ => None,
            })
            .collect();
        assert_eq!(ups, [Key::Character('a'), Key::Up]);
        // A release isn't a press: nothing reads it as a key typed.
        assert_eq!(input.keys().filter(|(key, _)| *key == Key::Up).count(), 1);
    }

    #[test]
    fn serial_keys_release_as_soon_as_they_press() {
        let mut keys = SerialKeys::new();
        let input = feed_all(&mut keys, b"a");
        let events: std::vec::Vec<Event> = input.events().collect();
        assert!(matches!(events.first(), Some(Event::KeyDown(Key::Character('a'), _))));
        assert!(matches!(events.last(), Some(Event::KeyUp(Key::Character('a'), _))));
    }

    #[test]
    fn evdev_reports_modifier_changes() {
        let mut keys = EvdevKeys::new();
        let mut input = Input::new();
        keys.feed(KeyboardEvent { code: 42, pressed: true }, &mut input);
        keys.feed(KeyboardEvent { code: 42, pressed: false }, &mut input);
        let states: std::vec::Vec<bool> = input
            .events()
            .filter_map(|event| match event {
                Event::ModifiersChanged(modifiers) => Some(modifiers.contains(Modifiers::SHIFT)),
                _ => None,
            })
            .collect();
        assert_eq!(states, [true, false]);
    }
}
