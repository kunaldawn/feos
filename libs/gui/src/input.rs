//! Input events and the pointer/keyboard state accumulated between frames.

use core::ops::{BitAnd, BitOr, BitOrAssign};

use feos_gfx::{Point, Size};

/// Maximum number of events buffered for a single frame.
pub const MAX_EVENTS: usize = 64;

/// Queue slots only pointer events may use.
///
/// A burst of typing must not crowd out a button release, which would leave
/// every widget under the pointer behaving as if the button were still held.
pub const POINTER_RESERVE: usize = 16;

/// Keyboard modifier keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1 << 0);
    pub const CTRL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    pub const SUPER: Self = Self(1 << 3);

    /// Returns `true` when every bit in `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns `true` when no modifiers are held.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Adds modifier bits.
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Removes modifier bits.
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Returns the raw bits.
    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Modifiers {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

/// A non-text keyboard key.
///
/// Printable characters arrive as [`Event::Text`]. `Character` exists for
/// keyboard shortcuts a platform driver may want to report directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Tab,
    Enter,
    Escape,
    Space,
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
    Character(char),
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

/// A mouse button.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

/// One input event.
///
/// Events are intentionally small and `Copy`, so a driver can push them into a
/// fixed-size ring without allocating.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Event {
    #[default]
    None,
    PointerMoved(Point),
    PointerDown {
        pos: Point,
        button: MouseButton,
    },
    PointerUp {
        pos: Point,
        button: MouseButton,
    },
    PointerWheel(i32),
    KeyDown(Key, Modifiers),
    KeyUp(Key, Modifiers),
    Text(char),
    ModifiersChanged(Modifiers),
    Focus(bool),
    Resize(Size),
}

/// Input state for the current frame.
///
/// The platform feeds events with [`push`](Input::push), then calls
/// [`clear_events`](Input::clear_events) at the end of the frame. Pointer
/// position and button state persist across frames; the event queue does not.
pub struct Input {
    events: [Event; MAX_EVENTS],
    count: usize,
    pointer: Point,
    pointer_down: bool,
    secondary_down: bool,
    modifiers: Modifiers,
    focused: bool,
    size: Size,
}

impl Input {
    /// Creates an empty input state.
    pub const fn new() -> Self {
        Self {
            events: [Event::None; MAX_EVENTS],
            count: 0,
            pointer: Point::new(0, 0),
            pointer_down: false,
            secondary_down: false,
            modifiers: Modifiers::NONE,
            focused: true,
            size: Size::new(0, 0),
        }
    }

    /// Number of keyboard events that still fit in this frame's queue.
    ///
    /// An input source that can hold events back, such as a keyboard ring,
    /// should stop draining at zero and leave the rest for the next frame.
    pub const fn keyboard_capacity(&self) -> usize {
        (MAX_EVENTS - POINTER_RESERVE).saturating_sub(self.count)
    }

    /// Enqueues one event. Returns `false` when the event was not queued.
    ///
    /// Pointer and modifier state is applied even when the queue is full, so
    /// [`pointer_down`](Self::pointer_down) stays truthful and
    /// [`Ui`](crate::Ui) can still see a press or release that lost its event.
    /// Keyboard events cannot take the last [`POINTER_RESERVE`] slots, and
    /// consecutive pointer moves share one slot.
    pub fn push(&mut self, event: Event) -> bool {
        match event {
            Event::PointerMoved(pos) => self.pointer = pos,
            Event::PointerDown { pos, button } => {
                self.pointer = pos;
                match button {
                    MouseButton::Left => self.pointer_down = true,
                    MouseButton::Right | MouseButton::Middle => self.secondary_down = true,
                }
            }
            Event::PointerUp { pos, button } => {
                self.pointer = pos;
                match button {
                    MouseButton::Left => self.pointer_down = false,
                    MouseButton::Right | MouseButton::Middle => self.secondary_down = false,
                }
            }
            Event::ModifiersChanged(modifiers) => self.modifiers = modifiers,
            Event::KeyDown(_, modifiers) | Event::KeyUp(_, modifiers) => self.modifiers = modifiers,
            Event::Focus(focused) => self.focused = focused,
            Event::Resize(size) => self.size = size,
            Event::None | Event::PointerWheel(_) | Event::Text(_) => {}
        }

        if let Event::PointerMoved(_) = event {
            if self.count > 0 && matches!(self.events[self.count - 1], Event::PointerMoved(_)) {
                self.events[self.count - 1] = event;
                return true;
            }
        }
        let keyboard = matches!(
            event,
            Event::KeyDown(..) | Event::KeyUp(..) | Event::Text(_)
        );
        let limit = if keyboard {
            MAX_EVENTS - POINTER_RESERVE
        } else {
            MAX_EVENTS
        };
        if self.count >= limit {
            return false;
        }

        self.events[self.count] = event;
        self.count += 1;
        true
    }

    /// Clears the event queue while preserving pointer and key state.
    pub fn clear_events(&mut self) {
        self.count = 0;
    }

    /// Clears everything, including pointer, modifiers and focus.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Number of queued events.
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Returns `true` when no events are queued.
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Iterates over this frame's events.
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events[..self.count].iter().copied()
    }

    /// Iterates over typed characters in this frame's events.
    pub fn text(&self) -> impl Iterator<Item = char> + '_ {
        self.events().filter_map(|event| match event {
            Event::Text(ch) => Some(ch),
            _ => None,
        })
    }

    /// Iterates over key-down events in this frame.
    pub fn keys(&self) -> impl Iterator<Item = (Key, Modifiers)> + '_ {
        self.events().filter_map(|event| match event {
            Event::KeyDown(key, modifiers) => Some((key, modifiers)),
            _ => None,
        })
    }

    /// Returns a copy of this input with keyboard events removed.
    ///
    /// Pointer, wheel, focus and size events are preserved, as is the
    /// persistent pointer and modifier state. Surfaces that implement their
    /// own keyboard navigation can pass the result to a widget toolkit without
    /// it also activating a focused widget for the same key press.
    pub fn without_keyboard(&self) -> Self {
        let mut filtered = Self {
            events: [Event::None; MAX_EVENTS],
            count: 0,
            pointer: self.pointer,
            pointer_down: self.pointer_down,
            secondary_down: self.secondary_down,
            modifiers: self.modifiers,
            focused: self.focused,
            size: self.size,
        };

        for event in &self.events[..self.count] {
            if matches!(
                event,
                Event::KeyDown(..) | Event::KeyUp(..) | Event::Text(_)
            ) {
                continue;
            }
            filtered.events[filtered.count] = *event;
            filtered.count += 1;
        }

        filtered
    }

    /// Returns a copy of this input with the events `keep` rejects removed.
    pub fn filtered(&self, mut keep: impl FnMut(&Event) -> bool) -> Self {
        let mut filtered = Self {
            events: [Event::None; MAX_EVENTS],
            count: 0,
            pointer: self.pointer,
            pointer_down: self.pointer_down,
            secondary_down: self.secondary_down,
            modifiers: self.modifiers,
            focused: self.focused,
            size: self.size,
        };
        for event in &self.events[..self.count] {
            if keep(event) {
                filtered.events[filtered.count] = *event;
                filtered.count += 1;
            }
        }
        filtered
    }

    /// Current pointer position.
    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// Sets the pointer position without producing an event.
    pub fn set_pointer(&mut self, position: Point) {
        self.pointer = position;
    }

    /// Returns `true` while the left button is held.
    pub const fn pointer_down(&self) -> bool {
        self.pointer_down
    }

    /// Returns `true` while a right or middle button is held.
    pub const fn secondary_down(&self) -> bool {
        self.secondary_down
    }

    /// Current modifier state.
    pub const fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// Returns `true` when the surface has keyboard focus.
    pub const fn focused(&self) -> bool {
        self.focused
    }

    /// Last reported surface size.
    pub const fn size(&self) -> Size {
        self.size
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_state_persists_after_events_are_cleared() {
        let mut input = Input::new();
        input.push(Event::PointerMoved(Point::new(3, 4)));
        input.push(Event::PointerDown {
            pos: Point::new(5, 6),
            button: MouseButton::Left,
        });
        input.clear_events();

        assert_eq!(input.pointer(), Point::new(5, 6));
        assert!(input.pointer_down());
        assert!(input.is_empty());

        input.push(Event::PointerUp {
            pos: Point::new(7, 8),
            button: MouseButton::Left,
        });
        assert!(!input.pointer_down());
        assert_eq!(input.pointer(), Point::new(7, 8));
    }

    #[test]
    fn without_keyboard_keeps_pointer_state() {
        let mut input = Input::new();
        input.push(Event::KeyDown(Key::Tab, Modifiers::NONE));
        input.push(Event::Text('x'));
        input.push(Event::PointerMoved(Point::new(3, 4)));
        input.push(Event::PointerDown {
            pos: Point::new(5, 6),
            button: MouseButton::Left,
        });

        let filtered = input.without_keyboard();
        assert!(filtered.events().all(|event| !matches!(
            event,
            Event::KeyDown(..) | Event::KeyUp(..) | Event::Text(_)
        )));
        assert_eq!(filtered.events().count(), 2);
        assert_eq!(filtered.pointer(), Point::new(5, 6));
        assert!(filtered.pointer_down());
    }

    #[test]
    fn modifiers_combine() {
        let modifiers = Modifiers::CTRL | Modifiers::SHIFT;
        assert!(modifiers.contains(Modifiers::CTRL));
        assert!(modifiers.contains(Modifiers::SHIFT));
        assert!(!modifiers.contains(Modifiers::ALT));
        assert_eq!((modifiers & Modifiers::SHIFT), Modifiers::SHIFT);
    }

    #[test]
    fn typing_cannot_crowd_out_a_button_release() {
        let mut input = Input::new();
        input.push(Event::PointerDown {
            pos: Point::new(1, 1),
            button: MouseButton::Left,
        });
        let mut queued = 0;
        for _ in 0..MAX_EVENTS * 2 {
            if input.push(Event::Text('x')) {
                queued += 1;
            }
        }
        assert_eq!(queued, MAX_EVENTS - POINTER_RESERVE - 1);
        assert_eq!(input.keyboard_capacity(), 0);
        assert!(input.push(Event::PointerUp {
            pos: Point::new(1, 1),
            button: MouseButton::Left,
        }));
        assert!(!input.pointer_down());
    }

    #[test]
    fn pointer_state_survives_a_full_queue() {
        let mut input = Input::new();
        for index in 0..MAX_EVENTS {
            input.push(Event::PointerWheel(index as i32));
        }
        assert!(!input.push(Event::PointerDown {
            pos: Point::new(9, 9),
            button: MouseButton::Left,
        }));
        assert!(input.pointer_down());
        assert_eq!(input.pointer(), Point::new(9, 9));
    }

    #[test]
    fn consecutive_moves_share_one_slot() {
        let mut input = Input::new();
        for x in 0..200 {
            assert!(input.push(Event::PointerMoved(Point::new(x, 0))));
        }
        assert_eq!(input.len(), 1);
        assert_eq!(input.pointer(), Point::new(199, 0));
    }
}
