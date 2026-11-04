//! Persistent interaction and scroll state.

use feos_gfx::{Point, Rect};

use crate::id::Id;

/// Maximum number of floating surfaces the hit test tracks per frame.
pub const MAX_OVERLAYS: usize = 8;

/// Maximum number of widgets that can participate in tab navigation.
pub const MAX_FOCUS_ORDER: usize = 64;

#[derive(Clone, Copy, Debug)]
struct ScrollSlot {
    id: Id,
    offset: i32,
    content: i32,
}

impl ScrollSlot {
    const EMPTY: Self = Self {
        id: Id::NONE,
        offset: 0,
        content: 0,
    };
}

/// State that must survive between frames: hover, focus, the active widget,
/// the tab order and scroll offsets.
///
/// `N` is the number of scrollable regions tracked simultaneously. State is
/// stored inline, so the type never allocates. A scroll region beyond capacity
/// renders unscrolled.
pub struct InteractionState<const N: usize = 32> {
    hovered: Id,
    active: Id,
    focus: Id,
    focus_at_frame_start: Id,
    focus_order: [Id; MAX_FOCUS_ORDER],
    focus_order_len: usize,
    previous_order: [Id; MAX_FOCUS_ORDER],
    previous_order_len: usize,
    scroll: [ScrollSlot; N],
    last_pointer: Point,
    pointer_valid: bool,
    /// Left button state at the end of the previous frame.
    last_down: bool,
    /// Overlay rectangles registered during the current frame.
    overlays: [Rect; MAX_OVERLAYS],
    overlay_count: usize,
    /// Overlay rectangles from the previous frame, used for hit testing.
    previous_overlays: [Rect; MAX_OVERLAYS],
    previous_overlay_count: usize,
    frame: u64,
}

impl<const N: usize> InteractionState<N> {
    /// Creates an empty state.
    pub const fn new() -> Self {
        Self {
            hovered: Id::NONE,
            active: Id::NONE,
            focus: Id::NONE,
            focus_at_frame_start: Id::NONE,
            focus_order: [Id::NONE; MAX_FOCUS_ORDER],
            focus_order_len: 0,
            previous_order: [Id::NONE; MAX_FOCUS_ORDER],
            previous_order_len: 0,
            scroll: [ScrollSlot::EMPTY; N],
            last_pointer: Point::new(0, 0),
            pointer_valid: false,
            last_down: false,
            overlays: [Rect::new(0, 0, 0, 0); MAX_OVERLAYS],
            overlay_count: 0,
            previous_overlays: [Rect::new(0, 0, 0, 0); MAX_OVERLAYS],
            previous_overlay_count: 0,
            frame: 0,
        }
    }

    /// Starts a new frame: hover is cleared, the previous focus order becomes
    /// available for Tab handling and the current order starts empty.
    pub fn begin_frame(&mut self) {
        self.previous_order = self.focus_order;
        self.previous_order_len = self.focus_order_len;
        self.focus_order_len = 0;
        self.focus_at_frame_start = self.focus;
        self.hovered = Id::NONE;
        self.previous_overlays = self.overlays;
        self.previous_overlay_count = self.overlay_count;
        self.overlay_count = 0;
        self.frame = self.frame.wrapping_add(1);
    }

    /// Number of frames seen so far.
    pub const fn frame(&self) -> u64 {
        self.frame
    }

    /// The widget currently under the pointer.
    pub const fn hovered(&self) -> Id {
        self.hovered
    }

    /// The widget that owns the current press/drag.
    pub const fn active(&self) -> Id {
        self.active
    }

    pub(crate) fn set_active(&mut self, id: Id) {
        self.active = id;
    }

    pub(crate) fn set_hovered(&mut self, id: Id) {
        self.hovered = id;
    }

    /// The keyboard focus owner.
    pub const fn focus(&self) -> Id {
        self.focus
    }

    pub(crate) fn set_focus(&mut self, id: Id) {
        self.focus = id;
    }

    pub(crate) fn clear_focus(&mut self) {
        self.focus = Id::NONE;
    }

    /// The focus owner at the start of this frame, used to report focus loss.
    pub(crate) const fn focus_at_frame_start(&self) -> Id {
        self.focus_at_frame_start
    }

    /// Registers a widget for Tab navigation.
    pub(crate) fn register_focusable(&mut self, id: Id) {
        if self.focus_order_len < MAX_FOCUS_ORDER
            && !self.focus_order[..self.focus_order_len].contains(&id)
        {
            self.focus_order[self.focus_order_len] = id;
            self.focus_order_len += 1;
        }
    }

    /// Moves focus to the next widget in the previous frame's tab order.
    pub fn focus_next(&mut self) {
        if self.previous_order_len == 0 {
            return;
        }

        let next = match self.previous_order[..self.previous_order_len]
            .iter()
            .position(|id| *id == self.focus)
        {
            Some(index) => (index + 1) % self.previous_order_len,
            None => 0,
        };
        self.focus = self.previous_order[next];
    }

    /// Moves focus to the previous widget in the previous frame's tab order.
    pub fn focus_previous(&mut self) {
        if self.previous_order_len == 0 {
            return;
        }

        let next = match self.previous_order[..self.previous_order_len]
            .iter()
            .position(|id| *id == self.focus)
        {
            Some(0) | None => self.previous_order_len - 1,
            Some(index) => index - 1,
        };
        self.focus = self.previous_order[next];
    }

    /// Clears hover, focus, press and scroll state.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Registers a floating surface for the next frame's hit test and returns
    /// its stacking level, or `None` when the table is full.
    pub(crate) fn register_overlay(&mut self, rect: Rect) -> Option<usize> {
        if self.overlay_count == MAX_OVERLAYS {
            return None;
        }
        self.overlays[self.overlay_count] = rect;
        self.overlay_count += 1;
        Some(self.overlay_count)
    }

    /// Returns `true` when an overlay at `level` or above covered `point`
    /// last frame. Level 0 is the page; an overlay's own contents run at the
    /// level [`register_overlay`](Self::register_overlay) returned.
    pub(crate) fn overlay_covers(&self, level: usize, point: Point) -> bool {
        self.previous_overlays[..self.previous_overlay_count]
            .iter()
            .enumerate()
            .any(|(index, rect)| index >= level && rect.contains(point))
    }

    pub(crate) const fn last_down(&self) -> bool {
        self.last_down
    }

    pub(crate) fn set_last_down(&mut self, down: bool) {
        self.last_down = down;
    }

    pub(crate) fn last_pointer(&self) -> Option<Point> {
        if self.pointer_valid {
            Some(self.last_pointer)
        } else {
            None
        }
    }

    pub(crate) fn set_last_pointer(&mut self, pointer: Point) {
        self.last_pointer = pointer;
        self.pointer_valid = true;
    }

    /// Current scroll offset for `id` (0 when the region has no state).
    pub fn scroll_offset(&self, id: Id) -> i32 {
        self.scroll
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| slot.offset)
            .unwrap_or(0)
    }

    /// Last measured content height for `id`.
    pub fn scroll_content(&self, id: Id) -> i32 {
        self.scroll
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| slot.content)
            .unwrap_or(0)
    }

    pub(crate) fn set_scroll(&mut self, id: Id, offset: i32, content: i32) {
        if let Some(slot) = self.scroll.iter_mut().find(|slot| slot.id == id) {
            slot.offset = offset;
            slot.content = content;
            return;
        }

        if let Some(slot) = self.scroll.iter_mut().find(|slot| slot.id.is_none()) {
            *slot = ScrollSlot {
                id,
                offset,
                content,
            };
            return;
        }

        if N > 0 {
            // The table is full: replace a deterministic slot so one noisy
            // region cannot starve every other region indefinitely.
            let index = (id.raw() as usize) % N;
            self.scroll[index] = ScrollSlot {
                id,
                offset,
                content,
            };
        }
    }
}

impl<const N: usize> Default for InteractionState<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Caller-owned scroll position for APIs that need explicit scroll state.
///
/// The built-in [`scroll_area`](crate::Ui::scroll_area) keeps its offset in
/// [`InteractionState`]; this type is available for custom widgets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollState {
    pub offset: i32,
    pub content_height: i32,
    pub viewport_height: i32,
}

impl ScrollState {
    pub const fn new() -> Self {
        Self {
            offset: 0,
            content_height: 0,
            viewport_height: 0,
        }
    }

    /// Clamps the offset to the valid range.
    pub fn clamp(&mut self) {
        let max = (self.content_height - self.viewport_height).max(0);
        self.offset = self.offset.clamp(0, max);
    }

    /// Scrolls by `delta`, then clamps.
    pub fn scroll_by(&mut self, delta: i32) {
        self.offset = self.offset.saturating_add(delta);
        self.clamp();
    }

    /// The visible rectangle of the content, given a viewport origin.
    pub fn content_rect(&self, viewport: Rect) -> Rect {
        Rect::new(
            viewport.x,
            viewport.y.saturating_sub(self.offset),
            viewport.width,
            self.content_height.max(0) as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_order_survives_a_frame() {
        let mut state = InteractionState::<4>::new();
        state.begin_frame();
        let a = Id::new(1);
        let b = Id::new(2);
        state.register_focusable(a);
        state.register_focusable(b);
        state.begin_frame();
        state.focus_next();
        assert_eq!(state.focus(), a);
        state.focus_next();
        assert_eq!(state.focus(), b);
        state.focus_next();
        assert_eq!(state.focus(), a);
        state.focus_previous();
        assert_eq!(state.focus(), b);
    }

    #[test]
    fn scroll_offsets_are_keyed_by_id() {
        let mut state = InteractionState::<2>::new();
        let a = Id::new(10);
        let b = Id::new(11);
        state.set_scroll(a, 4, 100);
        state.set_scroll(b, 8, 200);
        assert_eq!(state.scroll_offset(a), 4);
        assert_eq!(state.scroll_offset(b), 8);
        state.set_scroll(a, 12, 100);
        assert_eq!(state.scroll_offset(a), 12);
    }

    #[test]
    fn scroll_state_clamps() {
        let mut scroll = ScrollState::new();
        scroll.content_height = 100;
        scroll.viewport_height = 40;
        scroll.scroll_by(200);
        assert_eq!(scroll.offset, 60);
        scroll.scroll_by(-100);
        assert_eq!(scroll.offset, 0);
    }
}
