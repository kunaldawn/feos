//! Widget responses and interaction sensing.

use feos_gfx::{Point, Rect};

use crate::id::Id;

/// Describes what kind of interaction a widget wants to receive.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Sense(u8);

impl Sense {
    /// The widget can be hovered (and therefore needs hover feedback).
    pub const HOVER: Self = Self(1 << 0);
    /// A completed click is reported.
    pub const CLICK: Self = Self(1 << 1);
    /// Press, move and release are reported as a drag.
    pub const DRAG: Self = Self(1 << 2);
    /// The widget participates in keyboard focus and Tab navigation.
    pub const FOCUSABLE: Self = Self(1 << 3);
    /// Mouse-wheel events over the widget are reported.
    pub const SCROLL: Self = Self(1 << 4);

    /// Click plus focus, the default for buttons, tabs and checkboxes.
    pub const fn click() -> Self {
        Self(Self::CLICK.0 | Self::FOCUSABLE.0 | Self::HOVER.0)
    }

    /// Drag plus focus, the default for sliders and window title bars.
    pub const fn drag() -> Self {
        Self(Self::DRAG.0 | Self::FOCUSABLE.0 | Self::HOVER.0)
    }

    /// Keyboard-only focus target, used by text fields.
    pub const fn focus() -> Self {
        Self(Self::FOCUSABLE.0 | Self::HOVER.0 | Self::CLICK.0)
    }

    /// Returns `true` when every bit in `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Adds capabilities.
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The result of laying out and interacting with one widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Response {
    /// Widget identifier.
    pub id: Id,
    /// Rectangle occupied by the widget this frame.
    pub rect: Rect,
    /// Pointer is over the widget.
    pub hovered: bool,
    /// Widget owns the current press or drag.
    pub active: bool,
    /// Left button is held on the widget.
    pub pressed: bool,
    /// A click completed on this frame.
    pub clicked: bool,
    /// A right click completed on this frame.
    pub secondary_clicked: bool,
    /// The widget is being dragged this frame.
    pub dragged: bool,
    /// Pointer movement since the previous frame, valid while dragging.
    pub drag_delta: Point,
    /// Mouse-wheel delta over the widget this frame.
    pub scroll: i32,
    /// The widget owns keyboard focus.
    pub focused: bool,
    /// Focus moved away from this widget this frame.
    pub lost_focus: bool,
    /// The widget's value changed this frame.
    pub changed: bool,
    /// Enter was pressed while the widget was focused.
    pub submitted: bool,
}

impl Response {
    pub(crate) const fn empty(id: Id, rect: Rect) -> Self {
        Self {
            id,
            rect,
            hovered: false,
            active: false,
            pressed: false,
            clicked: false,
            secondary_clicked: false,
            dragged: false,
            drag_delta: Point::new(0, 0),
            scroll: 0,
            focused: false,
            lost_focus: false,
            changed: false,
            submitted: false,
        }
    }
}
