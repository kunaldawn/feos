//! The immediate-mode UI context.

use feos_gfx::{Canvas, Point, Rect};

use crate::geometry::{Padding, RectExt};
use crate::id::{mix, Id};
use crate::input::{Event, Input, Key, Modifiers, MouseButton};
use crate::layout::{Axis, LayoutStack};
use crate::painter::Painter;
use crate::response::{Response, Sense};
use crate::state::InteractionState;
use crate::theme::Theme;

/// A draggable, resizable top-level window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowState {
    /// Window rectangle in canvas coordinates.
    pub rect: Rect,
    /// Whether the window is displayed.
    pub open: bool,
    /// Whether the window is collapsed to its title bar.
    pub minimized: bool,
}

impl WindowState {
    /// Creates an open window.
    pub const fn new(rect: Rect) -> Self {
        Self {
            rect,
            open: true,
            minimized: false,
        }
    }

    /// Creates a window that starts closed.
    pub const fn closed(rect: Rect) -> Self {
        Self {
            rect,
            open: false,
            minimized: false,
        }
    }
}

/// Result of drawing a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowResponse {
    /// Interaction with the title bar.
    pub response: Response,
    /// The user clicked the close button.
    pub closed: bool,
    /// The content area actually used.
    pub content: Rect,
}

/// The per-frame UI context.
///
/// A `Ui` borrows a canvas, a theme, the input state and the caller-owned
/// [`InteractionState`]. Construct one at the start of a frame and drop it at
/// the end; dropping releases any stale press state.
pub struct Ui<'a, 'b, 's, const N: usize = 32> {
    /// Drawing surface.
    pub painter: Painter<'a, 'b>,
    /// Active theme.
    pub theme: &'s Theme,
    /// Input for this frame.
    pub input: &'s Input,
    pub(crate) state: &'s mut InteractionState<N>,
    layout: LayoutStack,
    id_seed: u64,
    id_counter: u64,
    pointer: Point,
    pointer_down: bool,
    pointer_pressed: bool,
    pointer_released: bool,
    secondary_pressed: bool,
    secondary_released: bool,
    pointer_delta: Point,
    wheel: i32,
    tab_pressed: bool,
    escape_pressed: bool,
    enter_pressed: bool,
    space_pressed: bool,
    /// A surface above the one being drawn covers the pointer.
    pointer_occluded: bool,
    /// Stacking level of the overlay being drawn (0 is the page).
    overlay_level: usize,
    /// The pointer a widget under it asked for this frame.
    cursor: crate::cursor::CursorShape,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// Starts a frame.
    pub fn new(
        canvas: &'a mut Canvas<'b>,
        theme: &'s Theme,
        input: &'s Input,
        state: &'s mut InteractionState<N>,
    ) -> Self {
        let canvas_rect = Rect::new(0, 0, canvas.width() as u32, canvas.height() as u32);
        let pointer = input.pointer();
        let pointer_delta = state
            .last_pointer()
            .map(|previous| Point::new(pointer.x - previous.x, pointer.y - previous.y))
            .unwrap_or_else(|| Point::new(0, 0));

        state.begin_frame();
        state.set_last_pointer(pointer);

        let mut ui = Self {
            painter: Painter::new(canvas),
            theme,
            input,
            state,
            layout: LayoutStack::new(canvas_rect, theme.metrics.gap),
            id_seed: 0x0066_656f_7367_7569,
            id_counter: 0,
            pointer,
            pointer_down: input.pointer_down(),
            pointer_pressed: false,
            pointer_released: false,
            secondary_pressed: false,
            secondary_released: false,
            pointer_delta,
            wheel: 0,
            tab_pressed: false,
            escape_pressed: false,
            enter_pressed: false,
            space_pressed: false,
            pointer_occluded: false,
            overlay_level: 0,
            cursor: crate::cursor::CursorShape::Arrow,
        };
        ui.scan_events();

        // A press or release can lose its event when a frame's queue is full,
        // but `Input` still applies its state. Recover the edge from the state
        // so a widget cannot stay stuck in the pressed state.
        let was_down = ui.state.last_down();
        if ui.pointer_down && !was_down {
            ui.pointer_pressed = true;
        } else if !ui.pointer_down && was_down {
            ui.pointer_released = true;
        }
        ui.state.set_last_down(ui.pointer_down);

        if ui.tab_pressed {
            if ui.shift() {
                ui.state.focus_previous();
            } else {
                ui.state.focus_next();
            }
        }
        if ui.escape_pressed {
            ui.state.clear_focus();
        }
        if !input.focused() {
            ui.state.clear_focus();
        }

        ui
    }

    /// Asks for `shape` as the pointer this frame. Widgets call it while
    /// hovered: text fields ask for the text beam and links for the hand.
    pub fn set_cursor(&mut self, shape: crate::cursor::CursorShape) {
        self.cursor = shape;
    }

    /// The pointer asked for this frame, the arrow if nothing asked. A windowed
    /// app hands it to the desktop through `WindowContext::cursor`.
    pub fn cursor(&self) -> crate::cursor::CursorShape {
        self.cursor
    }

    fn scan_events(&mut self) {
        for event in self.input.events() {
            match event {
                Event::PointerDown {
                    button: MouseButton::Left,
                    ..
                } => self.pointer_pressed = true,
                Event::PointerUp {
                    button: MouseButton::Left,
                    ..
                } => self.pointer_released = true,
                Event::PointerDown {
                    button: MouseButton::Right | MouseButton::Middle,
                    ..
                } => self.secondary_pressed = true,
                Event::PointerUp {
                    button: MouseButton::Right | MouseButton::Middle,
                    ..
                } => self.secondary_released = true,
                Event::PointerWheel(delta) => self.wheel = self.wheel.saturating_add(delta),
                Event::KeyDown(Key::Tab, _) => self.tab_pressed = true,
                Event::KeyDown(Key::Escape, _) => self.escape_pressed = true,
                Event::KeyDown(Key::Enter, _) => self.enter_pressed = true,
                Event::KeyDown(Key::Space, _) => self.space_pressed = true,
                _ => {}
            }
        }
    }

    /// The canvas rectangle.
    pub fn canvas_rect(&self) -> Rect {
        self.painter.canvas_rect()
    }

    /// The canvas size.
    pub fn size(&self) -> (u32, u32) {
        self.painter.size()
    }

    /// The current clip rectangle.
    pub fn clip(&self) -> Rect {
        self.painter.clip()
    }

    /// The pointer position.
    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// Pointer movement since the previous frame.
    pub const fn pointer_delta(&self) -> Point {
        self.pointer_delta
    }

    /// Returns `true` while the left button is held.
    pub const fn pointer_down(&self) -> bool {
        self.pointer_down
    }

    /// Returns `true` when the left button went down this frame.
    pub const fn pointer_pressed(&self) -> bool {
        self.pointer_pressed
    }

    /// Returns `true` when the left button went up this frame.
    pub const fn pointer_released(&self) -> bool {
        self.pointer_released
    }

    /// Returns `true` when the right or middle button went down this frame.
    pub const fn secondary_pressed(&self) -> bool {
        self.secondary_pressed
    }

    /// Returns `true` when the right or middle button went up this frame.
    pub const fn secondary_released(&self) -> bool {
        self.secondary_released
    }

    /// Mouse-wheel delta for this frame.
    pub const fn wheel(&self) -> i32 {
        self.wheel
    }

    /// Current modifier state.
    pub const fn modifiers(&self) -> Modifiers {
        self.input.modifiers()
    }

    /// Returns `true` while Shift is held.
    pub const fn shift(&self) -> bool {
        self.input.modifiers().contains(Modifiers::SHIFT)
    }

    /// Returns `true` while Ctrl is held.
    pub const fn ctrl(&self) -> bool {
        self.input.modifiers().contains(Modifiers::CTRL)
    }

    /// Returns `true` while Alt is held.
    pub const fn alt(&self) -> bool {
        self.input.modifiers().contains(Modifiers::ALT)
    }

    /// Returns `true` when a key went down this frame.
    pub fn key_pressed(&self, key: Key) -> bool {
        self.input.keys().any(|(pressed, _)| pressed == key)
    }

    /// Returns `true` when Enter went down this frame.
    pub const fn enter_pressed(&self) -> bool {
        self.enter_pressed
    }

    /// Returns `true` when Escape went down this frame.
    pub const fn escape_pressed(&self) -> bool {
        self.escape_pressed
    }

    /// Typed characters for this frame.
    pub fn text(&self) -> impl Iterator<Item = char> + '_ {
        self.input.text()
    }

    /// The persistent interaction state.
    pub fn state(&self) -> &InteractionState<N> {
        self.state
    }

    /// Mutable persistent interaction state.
    pub fn state_mut(&mut self) -> &mut InteractionState<N> {
        self.state
    }

    /// The current keyboard focus owner.
    pub fn focus(&self) -> Id {
        self.state.focus()
    }

    /// Gives keyboard focus to `id`.
    pub fn request_focus(&mut self, id: Id) {
        self.state.set_focus(id);
    }

    /// Clears keyboard focus.
    pub fn clear_focus(&mut self) {
        self.state.clear_focus();
    }

    /// Produces the next widget identifier in the current ID scope.
    pub fn next_id(&mut self) -> Id {
        let counter = self.id_counter;
        self.id_counter = self.id_counter.wrapping_add(1);
        Id::new(mix(self.id_seed ^ mix(counter)) | 1)
    }

    /// Runs `f` inside an identifier scope.
    ///
    /// Widgets created inside the closure keep stable IDs even when widgets
    /// before or after the scope change.
    pub fn with_id<R>(&mut self, label: &str, f: impl FnOnce(&mut Self) -> R) -> R {
        let previous = self.id_seed;
        self.id_seed = mix(previous ^ Id::from_name(label).raw());
        let result = f(self);
        self.id_seed = previous;
        result
    }

    /// Lays out the next widget cell and returns its rectangle.
    ///
    /// In a column the width is the full frame width; in a row the width is
    /// `content_width`; in a grid the cell size wins.
    pub fn allocate(&mut self, content_width: i32, content_height: i32) -> Rect {
        self.layout.next(content_width, content_height)
    }

    /// Lays out the next widget cell with an explicit width even in a column.
    pub fn allocate_sized(&mut self, width: i32, height: i32) -> Rect {
        self.layout.next_sized(width, height)
    }

    /// The current layout axis.
    pub fn axis(&self) -> Axis {
        self.layout.axis()
    }

    /// Remaining width and height in the current layout frame.
    pub fn remaining(&self) -> (i32, i32) {
        self.layout.remaining()
    }

    /// Runs `f` inside a vertical layout.
    ///
    /// Returns the area the children consumed.
    pub fn column<R>(&mut self, rect: Rect, gap: i32, f: impl FnOnce(&mut Self) -> R) -> Rect {
        let (used, _) = self.column_with(rect, gap, f);
        used
    }

    /// Runs `f` inside a vertical layout and returns both the consumed area
    /// and the closure's result.
    pub fn column_with<R>(
        &mut self,
        rect: Rect,
        gap: i32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> (Rect, R) {
        self.layout.push_column(rect, gap);
        let value = f(self);
        let used = self.layout.pop();
        (used, value)
    }

    /// Runs `f` inside a horizontal layout.
    ///
    /// Returns the area the children consumed.
    pub fn row<R>(&mut self, rect: Rect, gap: i32, f: impl FnOnce(&mut Self) -> R) -> Rect {
        let (used, _) = self.row_with(rect, gap, f);
        used
    }

    /// Runs `f` inside a horizontal layout and returns both the consumed area
    /// and the closure's result.
    pub fn row_with<R>(
        &mut self,
        rect: Rect,
        gap: i32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> (Rect, R) {
        self.layout.push_row(rect, gap);
        let value = f(self);
        let used = self.layout.pop();
        (used, value)
    }

    /// Runs `f` inside a fixed-cell grid.
    ///
    /// Returns the area the grid consumed.
    pub fn grid<R>(
        &mut self,
        rect: Rect,
        columns: u32,
        cell_height: i32,
        gap: i32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Rect {
        let (used, _) = self.grid_with(rect, columns, cell_height, gap, f);
        used
    }

    /// Runs `f` inside a fixed-cell grid and returns both the consumed area
    /// and the closure's result.
    pub fn grid_with<R>(
        &mut self,
        rect: Rect,
        columns: u32,
        cell_height: i32,
        gap: i32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> (Rect, R) {
        self.layout.push_grid(rect, columns, cell_height, gap);
        let value = f(self);
        let used = self.layout.pop();
        (used, value)
    }

    /// Runs `f` inside `rect` after applying padding.
    ///
    /// The padded area becomes a new vertical layout, so widgets created by
    /// `f` flow from the padded top-left corner.
    pub fn padded<R>(&mut self, rect: Rect, padding: Padding, f: impl FnOnce(&mut Self) -> R) -> R {
        let inner = padding.apply(rect);
        self.layout.push_column(inner, self.theme.metrics.gap);
        let result = f(self);
        self.layout.pop();
        result
    }

    /// Centers a `width` x `height` area inside `rect`.
    pub fn centered(&self, rect: Rect, width: i32, height: i32) -> Rect {
        Rect::new(0, 0, width.max(0) as u32, height.max(0) as u32).center_in(rect)
    }

    /// Returns `true` when the pointer is over `rect`, inside the clip and not
    /// covered by a surface above the one being drawn.
    pub fn pointer_over(&self, rect: Rect) -> bool {
        !self.pointer_occluded
            && !self.state.overlay_covers(self.overlay_level, self.pointer)
            && self.painter.clip_contains(self.pointer)
            && rect.contains(self.pointer)
    }

    /// Declares that a surface drawn later in this frame covers the pointer,
    /// so widgets drawn until the next call must not respond to it.
    ///
    /// The toolkit keeps no z-order, because whoever owns the surfaces already
    /// knows it. A window manager calls this before each surface.
    pub fn set_pointer_occluded(&mut self, occluded: bool) {
        self.pointer_occluded = occluded;
    }

    /// Returns `true` while the pointer is marked as covered.
    pub const fn pointer_occluded(&self) -> bool {
        self.pointer_occluded
    }

    /// Marks everything drawn from here on as a floating surface over `rect`,
    /// for the rest of the frame.
    ///
    /// This is [`overlay`](Self::overlay) for code that cannot use a closure:
    /// a popup drawn at the end of a widget, for example. Floating surfaces
    /// are already drawn late, so what follows belongs to them or sits above.
    pub fn float(&mut self, rect: Rect) {
        if let Some(level) = self.state.register_overlay(rect) {
            self.overlay_level = level;
        }
    }

    /// Draws a floating surface over `rect`.
    ///
    /// A popup is drawn late in the frame so it paints on top, which is too
    /// late to stop the widgets beneath it from reacting in the same frame.
    /// Registering the rectangle here makes them ignore the pointer from the
    /// next frame on, one frame of latency that only matters on the frame the
    /// popup opens. Widgets inside `f` still respond; overlays opened later
    /// cover this one in turn.
    pub fn overlay<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Self) -> R) -> R {
        let Some(level) = self.state.register_overlay(rect) else {
            return f(self);
        };
        let previous = self.overlay_level;
        self.overlay_level = level;
        let result = f(self);
        self.overlay_level = previous;
        result
    }

    /// Lays out a widget and processes pointer/keyboard interaction.
    ///
    /// Widgets use this internally; custom widgets can too.
    pub fn interact(&mut self, id: Id, rect: Rect, sense: Sense) -> Response {
        let mut response = Response::empty(id, rect);
        let hovered = self.pointer_over(rect);
        response.hovered = hovered;

        if hovered {
            self.state.set_hovered(id);
        }

        if sense.contains(Sense::FOCUSABLE) {
            self.state.register_focusable(id);
        }
        let focused_before = self.state.focus_at_frame_start();

        let can_press = sense.contains(Sense::CLICK) || sense.contains(Sense::DRAG);
        if hovered && self.pointer_pressed && can_press {
            self.state.set_active(id);
            if sense.contains(Sense::FOCUSABLE) {
                self.state.set_focus(id);
            }
        }

        let is_active = self.state.active() == id;
        response.active = is_active;
        response.pressed = is_active && self.pointer_down;
        response.focused = self.state.focus() == id;

        if is_active && self.pointer_released {
            if hovered && sense.contains(Sense::CLICK) {
                response.clicked = true;
            }
            self.state.set_active(Id::NONE);
        }

        if hovered && self.secondary_released && sense.contains(Sense::CLICK) {
            response.secondary_clicked = true;
        }

        if is_active && sense.contains(Sense::DRAG) {
            // The pointer delta on the press frame includes the movement that
            // brought the pointer onto the widget. Reporting it as a drag would
            // make windows jump when a drag starts, so only frames after the
            // press count as drag movement.
            response.dragged = self.pointer_down
                && !self.pointer_pressed
                && (self.pointer_delta.x != 0 || self.pointer_delta.y != 0);
            response.drag_delta = self.pointer_delta;
        }

        if hovered && self.wheel != 0 && sense.contains(Sense::SCROLL) {
            response.scroll = self.wheel;
        }

        if response.focused
            && sense.contains(Sense::CLICK)
            && (self.enter_pressed || self.space_pressed)
        {
            response.clicked = true;
            response.changed = true;
        }
        if response.focused && self.enter_pressed {
            response.submitted = true;
        }

        if focused_before == id && self.state.focus() != id {
            response.lost_focus = true;
        }

        response
    }
}

impl<const N: usize> Drop for Ui<'_, '_, '_, N> {
    fn drop(&mut self) {
        // If a widget disappeared while it owned a press, do not leave the UI
        // stuck in a pressed state after the release event.
        if self.pointer_released {
            self.state.set_active(Id::NONE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(
        state: &mut InteractionState<4>,
        input: &Input,
        mut draw: impl FnMut(&mut Ui<'_, '_, '_, 4>),
    ) {
        let mut pixels = [0u32; 64 * 64];
        let mut canvas = Canvas::new(&mut pixels, 64, 64, 64).unwrap();
        let theme = Theme::default();
        let mut ui = Ui::new(&mut canvas, &theme, input, state);
        draw(&mut ui);
    }

    #[test]
    fn an_occluded_widget_ignores_the_pointer() {
        let rect = Rect::new(0, 0, 32, 32);
        let id = Id::from_name("below");
        let mut state = InteractionState::<4>::new();
        let mut input = Input::new();
        input.push(Event::PointerDown {
            pos: Point::new(4, 4),
            button: MouseButton::Left,
        });
        frame(&mut state, &input, |ui| {
            ui.set_pointer_occluded(true);
            let hit = ui.interact(id, rect, Sense::click());
            assert!(!hit.hovered && !hit.pressed);
            ui.set_pointer_occluded(false);
            let hit = ui.interact(Id::from_name("above"), rect, Sense::click());
            assert!(hit.hovered && hit.pressed);
        });
        assert_ne!(state.active(), id);
    }

    #[test]
    fn an_overlay_blocks_widgets_beneath_it_from_the_next_frame() {
        let rect = Rect::new(0, 0, 32, 32);
        let mut state = InteractionState::<4>::new();
        let mut input = Input::new();
        input.set_pointer(Point::new(4, 4));
        let draw = |ui: &mut Ui<'_, '_, '_, 4>, expect_below: bool| {
            let below = ui.interact(Id::from_name("below"), rect, Sense::click());
            assert_eq!(below.hovered, expect_below);
            let inside = ui.overlay(rect, |ui| {
                ui.interact(Id::from_name("inside"), rect, Sense::click())
            });
            assert!(inside.hovered);
        };
        frame(&mut state, &input, |ui| draw(ui, true));
        frame(&mut state, &input, |ui| draw(ui, false));
    }

    #[test]
    fn a_release_lost_to_a_full_queue_still_ends_the_press() {
        let rect = Rect::new(0, 0, 32, 32);
        let id = Id::from_name("button");
        let mut state = InteractionState::<4>::new();
        let mut input = Input::new();
        input.push(Event::PointerDown {
            pos: Point::new(4, 4),
            button: MouseButton::Left,
        });
        frame(&mut state, &input, |ui| {
            ui.interact(id, rect, Sense::click());
        });
        assert_eq!(state.active(), id);

        input.clear_events();
        for _ in 0..crate::input::MAX_EVENTS {
            input.push(Event::PointerWheel(1));
        }
        assert!(!input.push(Event::PointerUp {
            pos: Point::new(4, 4),
            button: MouseButton::Left,
        }));
        frame(&mut state, &input, |ui| {
            let hit = ui.interact(id, rect, Sense::click());
            assert!(hit.clicked);
        });
        assert_eq!(state.active(), Id::NONE);
    }
}
