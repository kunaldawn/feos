//! Turns serial-console bytes into [`feos_gui::Input`] events.
//!
//! The QEMU board has no keyboard device, so the serial terminal is the
//! keyboard for the desktop and full-screen GUI apps. Terminals send arrow
//! keys as `ESC [ A` (or `ESC O A`), which means a lone `Escape` can only be
//! recognised after a short timeout.
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
}

impl SerialKeys {
    /// Creates a decoder in the idle state.
    pub const fn new() -> Self {
        Self {
            state: DecodeState::Ground,
            escape_at: 0,
            exit: false,
        }
    }

    fn key(input: &mut Input, key: Key) {
        input.push(Event::KeyDown(key, Modifiers::NONE));
    }

    fn text(input: &mut Input, ch: char) {
        input.push(Event::Text(ch));
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
                Self::key(input, Key::Escape);
            }
            self.state = DecodeState::Ground;
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
                b'\r' | b'\n' => Self::key(input, Key::Enter),
                b'\t' => Self::key(input, Key::Tab),
                0x7f | 0x08 => Self::key(input, Key::Backspace),
                b' ' => {
                    Self::key(input, Key::Space);
                    Self::text(input, ' ');
                }
                byte if byte.is_ascii_graphic() => {
                    let ch = byte as char;
                    Self::key(input, Key::Character(ch));
                    Self::text(input, ch);
                }
                _ => {}
            },
            DecodeState::Escape => {
                if byte == b'[' || byte == b'O' {
                    self.state = DecodeState::ControlSequence;
                    self.escape_at = now;
                } else {
                    // Not a sequence: this was a plain Escape, then another key.
                    self.state = DecodeState::Ground;
                    Self::key(input, Key::Escape);
                    self.feed(byte, now, input);
                }
            }
            DecodeState::ControlSequence => {
                if (0x40..=0x7e).contains(&byte) {
                    self.state = DecodeState::Ground;
                    let key = match byte {
                        b'A' => Some(Key::Up),
                        b'B' => Some(Key::Down),
                        b'C' => Some(Key::Right),
                        b'D' => Some(Key::Left),
                        b'H' => Some(Key::Home),
                        b'F' => Some(Key::End),
                        _ => None,
                    };
                    if let Some(key) = key {
                        Self::key(input, key);
                    }
                } else if byte == 0x1b {
                    // A new sequence starts; drop the incomplete one.
                    self.state = DecodeState::Escape;
                    self.escape_at = now;
                }
                // Other bytes are numeric parameters (for example `1;5`).
            }
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

    #[test]
    fn terminal_arrow_sequences_become_navigation_keys() {
        let mut keys = SerialKeys::new();
        let mut input = Input::new();
        for byte in b"\x1b[A" {
            keys.feed(*byte, 0, &mut input);
        }
        assert!(input.keys().any(|(key, _)| key == Key::Up));

        let mut input = Input::new();
        for byte in b"\x1bOB" {
            keys.feed(*byte, 0, &mut input);
        }
        assert!(input.keys().any(|(key, _)| key == Key::Down));
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
}
