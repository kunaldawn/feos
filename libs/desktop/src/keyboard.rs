//! Keyboard layouts for the virtio keyboard.
//!
//! The evdev decoder reports what a US keyboard would have typed. Each printed
//! character identifies one physical key and shift state, so another layout is
//! a table from that character to the one the user's keycaps show. Characters
//! the bitmap font lacks are not in the tables: those keys keep their US
//! meaning instead of typing `?`.

use feos_gui::{Event, Key};

/// A keyboard layout, selected in the configuration or the settings dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum KeyboardLayout {
    /// US QWERTY, what the decoder produces.
    #[default]
    Us,
    /// UK QWERTY.
    Uk,
    /// German QWERTZ.
    De,
    /// French AZERTY.
    Fr,
    /// Dvorak.
    Dvorak,
}

const DVORAK_FROM: &str = "qwertyuiop[]sdfghjkl;'zxcvbnm,./QWERTYUIOP{}SDFGHJKL:\"ZXCVBNM<>?-_=+";
const DVORAK_TO: &str = "',.pyfgcrl/=oeuidhtns-;qjkxbmwvz\"<>PYFGCRL?+OEUIDHTNS_:QJKXBMWVZ[{]}";

const FR_FROM: &str = "qwazQWAZm;M:,<./?";
const FR_TO: &str = "azqwAZQW;m:M,<./?";

const DE_FROM: &str = "yzYZ@#^&*()_+-=<>?/";
const DE_TO: &str = "zyZY\"#&/()=?`-'<>_-";

const UK_FROM: &str = "@\"";
const UK_TO: &str = "\"@";

impl KeyboardLayout {
    /// Every layout, in the order the settings dialog lists them.
    pub const ALL: [Self; 5] = [Self::Us, Self::Uk, Self::De, Self::Fr, Self::Dvorak];

    /// Parses a layout name from the configuration.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|layout| value.eq_ignore_ascii_case(layout.name()))
    }

    /// The name used in the configuration and the settings dialog.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Us => "us",
            Self::Uk => "uk",
            Self::De => "de",
            Self::Fr => "fr",
            Self::Dvorak => "dvorak",
        }
    }

    /// The character a key produces in this layout, given what it produces on
    /// a US keyboard.
    pub fn map_char(self, ch: char) -> char {
        let (from, to) = match self {
            Self::Us => return ch,
            Self::Uk => (UK_FROM, UK_TO),
            Self::De => (DE_FROM, DE_TO),
            Self::Fr => (FR_FROM, FR_TO),
            Self::Dvorak => (DVORAK_FROM, DVORAK_TO),
        };
        from.chars()
            .position(|known| known == ch)
            .and_then(|index| to.chars().nth(index))
            .unwrap_or(ch)
    }

    /// Applies this layout to one decoded event.
    pub fn map_event(self, event: Event) -> Event {
        match event {
            Event::Text(ch) => Event::Text(self.map_char(ch)),
            Event::KeyDown(Key::Character(ch), modifiers) => {
                Event::KeyDown(Key::Character(self.map_char(ch)), modifiers)
            }
            Event::KeyUp(Key::Character(ch), modifiers) => {
                Event::KeyUp(Key::Character(self.map_char(ch)), modifiers)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_gui::Modifiers;

    #[test]
    fn tables_pair_up_character_for_character() {
        for (from, to) in [
            (DVORAK_FROM, DVORAK_TO),
            (FR_FROM, FR_TO),
            (DE_FROM, DE_TO),
            (UK_FROM, UK_TO),
        ] {
            assert_eq!(from.chars().count(), to.chars().count(), "{from}");
        }
    }

    #[test]
    fn us_is_the_identity() {
        for ch in ' '..='~' {
            assert_eq!(KeyboardLayout::Us.map_char(ch), ch);
        }
    }

    #[test]
    fn layouts_move_the_keys_they_are_known_for() {
        assert_eq!(KeyboardLayout::De.map_char('y'), 'z');
        assert_eq!(KeyboardLayout::De.map_char('Z'), 'Y');
        assert_eq!(KeyboardLayout::Fr.map_char('q'), 'a');
        assert_eq!(KeyboardLayout::Fr.map_char('m'), ';');
        assert_eq!(KeyboardLayout::Dvorak.map_char('s'), 'o');
        assert_eq!(KeyboardLayout::Dvorak.map_char('d'), 'e');
        assert_eq!(KeyboardLayout::Uk.map_char('@'), '"');
    }

    #[test]
    fn dvorak_never_loses_a_character() {
        // A permutation must not make any printable character untypable.
        let mut seen = [false; 128];
        for ch in ' '..='~' {
            seen[KeyboardLayout::Dvorak.map_char(ch) as usize] = true;
        }
        for ch in ' '..='~' {
            assert!(seen[ch as usize], "{ch} is untypable");
        }
    }

    #[test]
    fn events_are_mapped_but_special_keys_pass_through() {
        let layout = KeyboardLayout::De;
        assert_eq!(layout.map_event(Event::Text('y')), Event::Text('z'));
        assert_eq!(
            layout.map_event(Event::KeyDown(Key::Character('y'), Modifiers::CTRL)),
            Event::KeyDown(Key::Character('z'), Modifiers::CTRL)
        );
        assert_eq!(
            layout.map_event(Event::KeyDown(Key::Enter, Modifiers::NONE)),
            Event::KeyDown(Key::Enter, Modifiers::NONE)
        );
    }

    #[test]
    fn names_round_trip() {
        for layout in KeyboardLayout::ALL {
            assert_eq!(KeyboardLayout::parse(layout.name()), Some(layout));
        }
        assert_eq!(KeyboardLayout::parse("klingon"), None);
    }
}
