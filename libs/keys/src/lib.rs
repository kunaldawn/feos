//! Turns serial-console bytes into [`feos_gui::Input`] events.
//!
//! The QEMU board has no keyboard device, so the serial terminal is the
//! keyboard for the desktop and full-screen GUI apps. Terminals send arrow
//! keys as `ESC [ A` (or `ESC O A`), which means a lone `Escape` can only be
//! recognised after a short timeout. Function keys, Page Up/Down, Insert,
//! Delete and Ctrl/Alt/Shift modified arrows are decoded from the standard
//! xterm control sequences as well.
//!
//! [`SerialKeys`] owns that decoder: a caller polls it once per frame, passes
//! the resulting [`Input`] to its UI, and clears the input's events when the
//! frame is done.
//!
//! ```no_run
//! use feos_gui::Input;
//! use feos_keys::SerialKeys;
//!
//! let mut keys = SerialKeys::new();
//! let mut input = Input::new();
//! let leave = keys.poll(0, &mut input);
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use feos_console::read_byte;
use feos_gui::{Event, Input, Key, Modifiers};

/// Maximum time spent waiting for the rest of an escape sequence, in ms.
pub const ESCAPE_TIMEOUT_MS: u64 = 30;

/// Ctrl-\ on a serial terminal: leave a full-screen session.
pub const EXIT_BYTE: u8 = 0x1c;

/// Ctrl-D, accepted as a convenience because some setups reserve Ctrl-\.
pub const EXIT_BYTE_ALT: u8 = 0x04;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DecodeState {
    Ground,
    Escape,
    ControlSequence,
}

/// Decodes serial bytes into GUI input events.
pub struct SerialKeys {
    state: DecodeState,
    escape_at: u64,
    exit: bool,
    csi_params: [u16; 6],
    csi_count: usize,
    csi_current: u16,
    csi_has_current: bool,
}

impl SerialKeys {
    /// Creates a decoder in the idle state.
    pub const fn new() -> Self {
        Self {
            state: DecodeState::Ground,
            escape_at: 0,
            exit: false,
            csi_params: [0; 6],
            csi_count: 0,
            csi_current: 0,
            csi_has_current: false,
        }
    }

    fn key(input: &mut Input, key: Key, modifiers: Modifiers) {
        input.push(Event::KeyDown(key, modifiers));
    }

    fn text(input: &mut Input, ch: char) {
        input.push(Event::Text(ch));
    }

    fn csi_reset(&mut self) {
        self.csi_params = [0; 6];
        self.csi_count = 0;
        self.csi_current = 0;
        self.csi_has_current = false;
    }

    fn csi_value(&self, index: usize) -> u16 {
        if index < self.csi_count {
            self.csi_params[index]
        } else if index == self.csi_count && self.csi_has_current {
            self.csi_current
        } else {
            0
        }
    }

    /// Decodes the xterm modifier parameter (`2` is Shift, `5` is Ctrl, ...).
    fn csi_modifiers(value: u16) -> Modifiers {
        if value <= 1 {
            return Modifiers::NONE;
        }
        let bits = value - 1;
        let mut modifiers = Modifiers::NONE;
        if bits & 1 != 0 {
            modifiers |= Modifiers::SHIFT;
        }
        if bits & 2 != 0 {
            modifiers |= Modifiers::ALT;
        }
        if bits & 4 != 0 {
            modifiers |= Modifiers::CTRL;
        }
        modifiers
    }

    fn push_control_char(input: &mut Input, byte: u8) {
        // Ctrl-A is 0x01, Ctrl-Z is 0x1A. Tab, Enter and Backspace already
        // have their own meaning above, and Ctrl-\ / Ctrl-D leave the session.
        let key = match byte {
            0x00 => Key::Space,
            0x01..=0x1a => Key::Character((b'a' + byte - 1) as char),
            _ => return,
        };
        Self::key(input, key, Modifiers::CTRL);
    }

    /// Drains the console and queues this frame's events.
    ///
    /// Returns `true` when the user asked to leave the session with Ctrl-\ or
    /// Ctrl-D. The flag is sticky once set.
    pub fn poll(&mut self, now: u64, input: &mut Input) -> bool {
        while let Some(byte) = read_byte() {
            self.feed(byte, now, input);
        }

        if self.state != DecodeState::Ground
            && now.wrapping_sub(self.escape_at) >= ESCAPE_TIMEOUT_MS
        {
            if self.state == DecodeState::Escape {
                Self::key(input, Key::Escape, Modifiers::NONE);
            }
            self.state = DecodeState::Ground;
            self.csi_reset();
        }

        self.exit
    }

    /// Feeds one byte. Exposed for host tests and custom byte sources.
    pub fn feed(&mut self, byte: u8, now: u64, input: &mut Input) {
        match self.state {
            DecodeState::Ground => match byte {
                EXIT_BYTE | EXIT_BYTE_ALT => self.exit = true,
                0x1b => {
                    self.state = DecodeState::Escape;
                    self.escape_at = now;
                }
                b'\r' | b'\n' => Self::key(input, Key::Enter, Modifiers::NONE),
                b'\t' => Self::key(input, Key::Tab, Modifiers::NONE),
                0x7f | 0x08 => Self::key(input, Key::Backspace, Modifiers::NONE),
                b' ' => {
                    Self::key(input, Key::Space, Modifiers::NONE);
                    Self::text(input, ' ');
                }
                0x00..=0x1a => Self::push_control_char(input, byte),
                byte if byte.is_ascii_graphic() => {
                    let ch = byte as char;
                    Self::key(input, Key::Character(ch), Modifiers::NONE);
                    Self::text(input, ch);
                }
                _ => {}
            },
            DecodeState::Escape => {
                if byte == b'[' || byte == b'O' {
                    self.state = DecodeState::ControlSequence;
                    self.escape_at = now;
                    self.csi_reset();
                } else {
                    // Not a sequence: this was a plain Escape, then another
                    // key. Treat the second byte as Alt+key, because that is
                    // how an xterm reports Alt+Enter, Alt+Tab and Alt+letter.
                    self.state = DecodeState::Ground;
                    match byte {
                        b'\r' | b'\n' => Self::key(input, Key::Enter, Modifiers::ALT),
                        b'\t' => Self::key(input, Key::Tab, Modifiers::ALT),
                        0x7f | 0x08 => Self::key(input, Key::Backspace, Modifiers::ALT),
                        b' ' => Self::key(input, Key::Space, Modifiers::ALT),
                        byte if byte.is_ascii_graphic() => {
                            Self::key(input, Key::Character(byte as char), Modifiers::ALT)
                        }
                        _ => {
                            Self::key(input, Key::Escape, Modifiers::NONE);
                            self.feed(byte, now, input);
                        }
                    }
                }
            }
            DecodeState::ControlSequence => match byte {
                b'0'..=b'9' => {
                    self.csi_current = self
                        .csi_current
                        .saturating_mul(10)
                        .saturating_add((byte - b'0') as u16);
                    self.csi_has_current = true;
                }
                b';' => {
                    if self.csi_count < self.csi_params.len() {
                        self.csi_params[self.csi_count] = self.csi_current;
                        self.csi_count += 1;
                    }
                    self.csi_current = 0;
                    self.csi_has_current = false;
                }
                0x40..=0x7e => {
                    self.state = DecodeState::Ground;
                    self.decode_csi(byte, input);
                    self.csi_reset();
                }
                0x1b => {
                    self.state = DecodeState::Escape;
                    self.escape_at = now;
                    self.csi_reset();
                }
                _ => {}
            },
        }
    }

    fn decode_csi(&mut self, final_byte: u8, input: &mut Input) {
        let first = self.csi_value(0);
        let second = self.csi_value(1);
        let modifiers = Self::csi_modifiers(second);

        let key = match final_byte {
            b'A' => Some(Key::Up),
            b'B' => Some(Key::Down),
            b'C' => Some(Key::Right),
            b'D' => Some(Key::Left),
            b'H' => Some(Key::Home),
            b'F' => Some(Key::End),
            b'Z' => {
                Self::key(input, Key::Tab, Modifiers::SHIFT);
                return;
            }
            b'~' => match first {
                1 | 7 => Some(Key::Home),
                2 => Some(Key::Insert),
                3 => Some(Key::Delete),
                4 | 8 => Some(Key::End),
                5 => Some(Key::PageUp),
                6 => Some(Key::PageDown),
                11 => Some(Key::F1),
                12 => Some(Key::F2),
                13 => Some(Key::F3),
                14 => Some(Key::F4),
                15 => Some(Key::F5),
                17 => Some(Key::F6),
                18 => Some(Key::F7),
                19 => Some(Key::F8),
                20 => Some(Key::F9),
                21 => Some(Key::F10),
                23 => Some(Key::F11),
                24 => Some(Key::F12),
                _ => None,
            },
            _ => None,
        };

        if let Some(key) = key {
            Self::key(input, key, modifiers);
        }
    }
}

impl Default for SerialKeys {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
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
}
