//! HID reports: keyboards, mice and tablets.
//!
//! A keyboard speaks the boot protocol, whose report is the same for all of them. A
//! pointer's report is described by its report descriptor, which [`parse_report`]
//! reads, because a tablet has no boot protocol to fall back on.

use alloc::collections::VecDeque;

use feos_hal::{KeyboardEvent, PointerEvent};

/// The number of key slots in a boot keyboard report.
const BOOT_KEYS: usize = 6;
/// Key transitions waiting to be read, at most.
const KEY_QUEUE: usize = 128;

/// Linux key codes for HID keyboard usages 0x04 to 0x73, where a table is the only
/// way to say it. Zero means there's no such key here.
const USAGE_TO_KEY: [u8; 0x74] = [
    0, 0, 0, 0, // none, roll-over, POST fail, undefined
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, // a-z
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, // 1-0
    28, 1, 14, 15, 57, // enter, escape, backspace, tab, space
    12, 13, 26, 27, 43, 43, 39, 40, 41, 51, 52, 53, // - = [ ] \ non-US# ; ' ` , . /
    58, // caps lock
    59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, // F1-F12
    99, 70, 119, // print screen, scroll lock, pause
    110, 102, 104, 111, 107, 109, // insert, home, page up, delete, end, page down
    106, 105, 108, 103, // right, left, down, up
    69, // num lock
    98, 55, 74, 78, 96, // keypad / * - + enter
    79, 80, 81, 75, 76, 77, 71, 72, 73, 82, 83, // keypad 1-0 .
    86, 127, 116, 117, // non-US \, compose, power, keypad =
    183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, // F13-F24
];

/// Modifier bits, in report order: left control, shift, alt, meta, then right.
const MODIFIER_KEYS: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];

/// The Linux code for a HID usage on the keyboard page.
pub fn usage_to_key(usage: u8) -> Option<u16> {
    let code = *USAGE_TO_KEY.get(usize::from(usage))?;
    (code != 0).then_some(u16::from(code))
}

/// Turns a boot keyboard's reports, each the whole state of the keyboard, into
/// presses and releases.
#[derive(Default)]
pub struct KeyboardState {
    modifiers: u8,
    keys: [u8; BOOT_KEYS],
    queue: VecDeque<KeyboardEvent>,
}

impl KeyboardState {
    fn push(&mut self, code: u16, pressed: bool) {
        if self.queue.len() < KEY_QUEUE {
            self.queue.push_back(KeyboardEvent { code, pressed });
        }
    }

    /// Takes one report. Too short, or a roll-over error, changes nothing.
    pub fn on_report(&mut self, report: &[u8]) {
        if report.len() < 3 {
            return;
        }
        // Usage 1 in every slot means too many keys are down, and says nothing else.
        if report[2..].iter().take(BOOT_KEYS).all(|&usage| usage == 1) {
            return;
        }
        let modifiers = report[0];
        for (bit, &code) in MODIFIER_KEYS.iter().enumerate() {
            let mask = 1u8 << bit;
            if (self.modifiers ^ modifiers) & mask != 0 {
                self.push(code, modifiers & mask != 0);
            }
        }
        self.modifiers = modifiers;

        let mut now = [0u8; BOOT_KEYS];
        let given = &report[2..report.len().min(2 + BOOT_KEYS)];
        now[..given.len()].copy_from_slice(given);
        let before = self.keys;
        for &usage in before.iter().filter(|&&usage| usage != 0) {
            if !now.contains(&usage) {
                if let Some(code) = usage_to_key(usage) {
                    self.push(code, false);
                }
            }
        }
        for &usage in now.iter().filter(|&&usage| usage != 0) {
            if !before.contains(&usage) {
                if let Some(code) = usage_to_key(usage) {
                    self.push(code, true);
                }
            }
        }
        self.keys = now;
    }

    /// The next transition.
    pub fn next(&mut self) -> Option<KeyboardEvent> {
        self.queue.pop_front()
    }
}

/// Where one value is in a report, in bits from the start of the report's data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Field {
    /// Start, in bits.
    pub offset: u32,
    /// Width, in bits.
    pub size: u32,
    /// The largest value it claims.
    pub maximum: i32,
    /// True for absolute, false for a change.
    pub absolute: bool,
}

/// How a pointer's report is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ReportLayout {
    /// The report's ID, which comes first in each report when there is one.
    pub report_id: Option<u8>,
    /// Bit where the buttons start: left, right, middle.
    pub buttons: Option<u32>,
    /// X.
    pub x: Option<Field>,
    /// Y.
    pub y: Option<Field>,
    /// The wheel.
    pub wheel: Option<Field>,
}

impl ReportLayout {
    /// The boot protocol's mouse report: three bytes, and often a wheel in a fourth.
    pub const BOOT_MOUSE: Self = Self {
        report_id: None,
        buttons: Some(0),
        x: Some(Field { offset: 8, size: 8, maximum: 127, absolute: false }),
        y: Some(Field { offset: 16, size: 8, maximum: 127, absolute: false }),
        wheel: Some(Field { offset: 24, size: 8, maximum: 127, absolute: false }),
    };

    /// True when it's a layout a pointer could have.
    pub const fn is_pointer(&self) -> bool {
        self.x.is_some() && self.y.is_some()
    }
}

/// Reads a report descriptor as far as a mouse or tablet needs. Only the first report
/// that has X and Y counts. `None` when it runs off the end or nests too deep.
pub fn parse_report(descriptor: &[u8]) -> Option<ReportLayout> {
    const PAGE_DESKTOP: u32 = 1;
    const PAGE_BUTTON: u32 = 9;
    const USAGE_X: u32 = 0x30;
    const USAGE_Y: u32 = 0x31;
    const USAGE_WHEEL: u32 = 0x38;
    const MAX_USAGES: usize = 8;

    let mut layout = ReportLayout::default();
    let mut bit = 0u32;
    let mut page = 0u32;
    let mut report_size = 0u32;
    let mut report_count = 0u32;
    let mut maximum = 0i32;
    let mut usages = [0u32; MAX_USAGES];
    let mut usage_count = 0usize;
    let mut usage_min = 0u32;
    let mut report_id = None;
    let mut at = 0;
    let mut steps = 0;
    while at < descriptor.len() {
        steps += 1;
        if steps > 4096 {
            return None;
        }
        let prefix = descriptor[at];
        if prefix == 0xfe {
            // A long item: nothing here uses one.
            let length = usize::from(*descriptor.get(at + 1)?);
            at += 3 + length;
            continue;
        }
        let size = match prefix & 3 {
            3 => 4,
            other => usize::from(other),
        };
        let data = descriptor.get(at + 1..at + 1 + size)?;
        let mut unsigned = 0u32;
        for (index, &byte) in data.iter().enumerate() {
            unsigned |= u32::from(byte) << (8 * index);
        }
        let signed = match size {
            1 => i32::from(unsigned as u8 as i8),
            2 => i32::from(unsigned as u16 as i16),
            _ => unsigned as i32,
        };
        at += 1 + size;
        match prefix & 0xfc {
            // Global items.
            0x04 => page = unsigned,
            0x24 => maximum = signed,
            0x74 => report_size = unsigned,
            0x94 => report_count = unsigned,
            0x84 => {
                // A new report: one with a pointer in it has been read in full.
                if layout.is_pointer() {
                    return Some(layout);
                }
                layout = ReportLayout::default();
                bit = 0;
                report_id = Some(unsigned as u8);
                layout.report_id = report_id;
            }
            // Local items.
            0x08 => {
                if usage_count < MAX_USAGES {
                    usages[usage_count] = unsigned;
                    usage_count += 1;
                }
            }
            0x18 => usage_min = unsigned,
            // Input.
            0x80 => {
                let total = report_size.checked_mul(report_count)?;
                let constant = unsigned & 1 != 0;
                if !constant {
                    let absolute = unsigned & 4 == 0;
                    if page == PAGE_BUTTON {
                        if layout.buttons.is_none() && usage_min <= 1 {
                            layout.buttons = Some(bit);
                        }
                    } else if page == PAGE_DESKTOP {
                        for index in 0..report_count.min(MAX_USAGES as u32) {
                            let usage = usages.get(index as usize).copied().unwrap_or(0);
                            let field = Field {
                                offset: bit.checked_add(index.checked_mul(report_size)?)?,
                                size: report_size,
                                maximum,
                                absolute,
                            };
                            match usage {
                                USAGE_X => layout.x = Some(field),
                                USAGE_Y => layout.y = Some(field),
                                USAGE_WHEEL => layout.wheel = Some(field),
                                _ => {}
                            }
                        }
                    }
                }
                bit = bit.checked_add(total)?;
                usage_count = 0;
                usage_min = 0;
            }
            // Output, Feature and collections clear what's local, and move no bits here.
            0x90 | 0xb0 | 0xa0 | 0xc0 => {
                usage_count = 0;
                usage_min = 0;
            }
            _ => {}
        }
    }
    layout.report_id = report_id;
    layout.is_pointer().then_some(layout)
}

/// Reads a field from a report. Nothing past the end, so a short report reads as zero.
fn read_field(report: &[u8], field: &Field) -> i32 {
    if field.size == 0 || field.size > 32 {
        return 0;
    }
    let mut value = 0u32;
    for index in 0..field.size {
        let position = field.offset.saturating_add(index) as usize;
        let byte = report.get(position / 8).copied().unwrap_or(0);
        if byte >> (position % 8) & 1 != 0 {
            value |= 1 << index;
        }
    }
    if !field.absolute && field.size < 32 && value & (1 << (field.size - 1)) != 0 {
        // A change is signed.
        return (value | (u32::MAX << field.size)) as i32;
    }
    value as i32
}

/// Turns a pointer's reports into cursor positions and button changes.
pub struct PointerState {
    layout: ReportLayout,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    buttons: u8,
}

impl PointerState {
    /// A pointer on a screen of this size, starting from the middle.
    pub fn new(layout: ReportLayout, width: u16, height: u16) -> Self {
        Self {
            layout,
            width: i32::from(width.max(1)),
            height: i32::from(height.max(1)),
            x: i32::from(width / 2),
            y: i32::from(height / 2),
            buttons: 0,
        }
    }

    /// The cursor.
    pub fn position(&self) -> (u16, u16) {
        (self.x as u16, self.y as u16)
    }

    fn axis(field: &Field, report: &[u8], current: i32, extent: i32) -> i32 {
        let value = read_field(report, field);
        let target = if field.absolute {
            let maximum = i64::from(field.maximum.max(1));
            (i64::from(value.max(0)).min(maximum) * i64::from(extent - 1) / maximum) as i32
        } else {
            current.saturating_add(value)
        };
        target.clamp(0, extent - 1)
    }

    /// Takes one report, and returns what it changed. `None` for one that's for some
    /// other report, or changes nothing.
    pub fn on_report(&mut self, report: &[u8]) -> Option<PointerEvent> {
        let data = match self.layout.report_id {
            Some(id) => {
                if report.first() != Some(&id) {
                    return None;
                }
                &report[1..]
            }
            None => report,
        };
        let (old_x, old_y) = (self.x, self.y);
        if let Some(field) = &self.layout.x {
            self.x = Self::axis(field, data, self.x, self.width);
        }
        if let Some(field) = &self.layout.y {
            self.y = Self::axis(field, data, self.y, self.height);
        }
        let mut event = PointerEvent {
            x: self.x as u16,
            y: self.y as u16,
            ..PointerEvent::default()
        };
        if let Some(offset) = self.layout.buttons {
            let mut buttons = 0u8;
            for index in 0..3u32 {
                let position = offset.saturating_add(index) as usize;
                let byte = data.get(position / 8).copied().unwrap_or(0);
                if byte >> (position % 8) & 1 != 0 {
                    buttons |= 1 << index;
                }
            }
            let changed = self.buttons ^ buttons;
            let transition = |mask: u8| (changed & mask != 0).then_some(buttons & mask != 0);
            event.left = transition(1);
            event.right = transition(2);
            event.middle = transition(4);
            self.buttons = buttons;
        }
        if let Some(field) = &self.layout.wheel {
            event.wheel = read_field(data, field);
        }
        ((self.x, self.y) != (old_x, old_y) || !event.is_position_only()).then_some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As QEMU's usb-tablet reports it.
    const TABLET: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00,
        0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30,
        0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02,
        0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
        0xc0, 0xc0,
    ];

    /// As QEMU's usb-mouse reports it.
    const MOUSE: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00,
        0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30,
        0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, 0xc0, 0xc0,
    ];

    #[test]
    fn a_tablet_descriptor_reads_as_absolute() {
        let layout = parse_report(TABLET).unwrap();
        assert_eq!(layout.buttons, Some(0));
        assert_eq!(layout.x, Some(Field { offset: 8, size: 16, maximum: 0x7fff, absolute: true }));
        assert_eq!(layout.y, Some(Field { offset: 24, size: 16, maximum: 0x7fff, absolute: true }));
        assert_eq!(layout.wheel.unwrap().offset, 40);
        assert!(!layout.wheel.unwrap().absolute);
    }

    #[test]
    fn a_mouse_descriptor_reads_as_relative() {
        let layout = parse_report(MOUSE).unwrap();
        assert_eq!(layout.x, Some(Field { offset: 8, size: 8, maximum: 127, absolute: false }));
        assert_eq!(layout.wheel.unwrap().offset, 24);
    }

    #[test]
    fn a_keyboard_descriptor_is_not_a_pointer() {
        let keyboard = [0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x95, 0x08, 0x75, 0x01, 0x81, 0x02, 0xc0];
        assert_eq!(parse_report(&keyboard), None);
    }

    #[test]
    fn a_report_id_comes_first_in_the_report() {
        let mut descriptor = vec![0x85, 0x05];
        descriptor.extend_from_slice(MOUSE);
        let layout = parse_report(&descriptor).unwrap();
        assert_eq!(layout.report_id, Some(5));
        let mut pointer = PointerState::new(layout, 100, 100);
        assert_eq!(pointer.on_report(&[6, 0, 5, 5, 0]), None, "another report");
        let event = pointer.on_report(&[5, 0, 5, 5, 0]).unwrap();
        assert_eq!((event.x, event.y), (55, 55));
    }

    #[test]
    fn truncated_descriptors_are_refused() {
        for length in 0..TABLET.len() - 2 {
            let _ = parse_report(&TABLET[..length]);
        }
        assert_eq!(parse_report(&[0x26, 0xff]), None);
    }

    #[test]
    fn a_tablet_maps_to_the_screen() {
        let mut pointer = PointerState::new(parse_report(TABLET).unwrap(), 1024, 768);
        let event = pointer.on_report(&[0, 0x00, 0x40, 0x00, 0x20, 0]).unwrap();
        assert_eq!((event.x, event.y), (511, 191));
        let event = pointer.on_report(&[0, 0xff, 0x7f, 0xff, 0x7f, 0]).unwrap();
        assert_eq!((event.x, event.y), (1023, 767));
        assert_eq!(pointer.on_report(&[0, 0xff, 0x7f, 0xff, 0x7f, 0]), None, "no change");
    }

    #[test]
    fn a_mouse_moves_and_stays_on_the_screen() {
        let mut pointer = PointerState::new(parse_report(MOUSE).unwrap(), 100, 100);
        assert_eq!(pointer.position(), (50, 50));
        let event = pointer.on_report(&[0, 10, 0xf6, 0]).unwrap();
        assert_eq!((event.x, event.y), (60, 40));
        for _ in 0..20 {
            pointer.on_report(&[0, 0x7f, 0x7f, 0]);
        }
        assert_eq!(pointer.position(), (99, 99));
        for _ in 0..20 {
            pointer.on_report(&[0, 0x81, 0x81, 0]);
        }
        assert_eq!(pointer.position(), (0, 0));
    }

    #[test]
    fn buttons_and_the_wheel_are_reported_when_they_change() {
        let mut pointer = PointerState::new(ReportLayout::BOOT_MOUSE, 100, 100);
        let event = pointer.on_report(&[1, 0, 0]).unwrap();
        assert_eq!((event.left, event.right), (Some(true), None));
        assert_eq!(pointer.on_report(&[1, 0, 0]), None);
        let event = pointer.on_report(&[2, 0, 0]).unwrap();
        assert_eq!((event.left, event.right), (Some(false), Some(true)));
        let event = pointer.on_report(&[2, 0, 0, 0xff]).unwrap();
        assert_eq!(event.wheel, -1);
    }

    #[test]
    fn a_short_report_reads_as_zero() {
        let mut pointer = PointerState::new(parse_report(TABLET).unwrap(), 100, 100);
        let event = pointer.on_report(&[1]).unwrap();
        assert_eq!((event.left, event.x, event.y), (Some(true), 0, 0));
        assert!(pointer.on_report(&[]).is_some(), "the button came up");
    }

    #[test]
    fn keys_turn_into_presses_and_releases() {
        let mut keyboard = KeyboardState::default();
        // Press a.
        keyboard.on_report(&[0, 0, 4, 0, 0, 0, 0, 0]);
        assert_eq!(keyboard.next(), Some(KeyboardEvent { code: 30, pressed: true }));
        assert_eq!(keyboard.next(), None);
        // Hold a, press left shift and b.
        keyboard.on_report(&[2, 0, 4, 5, 0, 0, 0, 0]);
        assert_eq!(keyboard.next(), Some(KeyboardEvent { code: 42, pressed: true }));
        assert_eq!(keyboard.next(), Some(KeyboardEvent { code: 48, pressed: true }));
        // Let everything go.
        keyboard.on_report(&[0; 8]);
        assert_eq!(keyboard.next(), Some(KeyboardEvent { code: 42, pressed: false }));
        assert_eq!(keyboard.next(), Some(KeyboardEvent { code: 30, pressed: false }));
        assert_eq!(keyboard.next(), Some(KeyboardEvent { code: 48, pressed: false }));
    }

    #[test]
    fn a_roll_over_report_changes_nothing() {
        let mut keyboard = KeyboardState::default();
        keyboard.on_report(&[0, 0, 4, 0, 0, 0, 0, 0]);
        keyboard.next();
        keyboard.on_report(&[0, 0, 1, 1, 1, 1, 1, 1]);
        assert_eq!(keyboard.next(), None);
    }

    #[test]
    fn the_key_table_agrees_with_landmarks() {
        assert_eq!(USAGE_TO_KEY.len(), 0x74);
        // a, 1, enter, F1, right, up, keypad 1, keypad 0.
        for (usage, code) in [(0x04, 30), (0x1e, 2), (0x28, 28), (0x3a, 59), (0x4f, 106), (0x52, 103), (0x59, 79), (0x62, 82)] {
            assert_eq!(usage_to_key(usage), Some(code), "usage {usage:#x}");
        }
        assert_eq!(usage_to_key(0), None);
        assert_eq!(usage_to_key(0xff), None);
    }

    #[test]
    fn no_input_panics() {
        let mut state = 0x2468_ace1_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..100_000usize {
            let mut descriptor = TABLET.to_vec();
            for _ in 0..1 + round % 6 {
                let at = next() as usize % descriptor.len();
                descriptor[at] = next() as u8;
            }
            let layout = parse_report(&descriptor).unwrap_or(ReportLayout::BOOT_MOUSE);
            let mut pointer = PointerState::new(layout, 800, 600);
            let report: Vec<u8> = (0..next() % 12).map(|_| next() as u8).collect();
            pointer.on_report(&report);
            let mut keyboard = KeyboardState::default();
            keyboard.on_report(&report);
        }
    }
}
