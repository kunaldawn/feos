//! Layout components: accordions, split panes, drawers and popovers.
//!
//! These components arrange other widgets. Like the rest of the toolkit they
//! take caller-owned state and their closures run immediately, so nested
//! content stays ordinary feos-gui code.

use feos_gfx::Rect;

use crate::geometry::{Align, Padding, RectExt, VerticalAlign};
use crate::layout::Axis;
use crate::response::Response;
use crate::response::Sense;
use crate::ui::Ui;

use super::icons::{paint_icon, Icon};

/// Persistent expansion state for [`Ui::accordion_section`].
///
/// `N` is the maximum number of sections tracked. Higher indices render closed
/// instead of panicking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccordionState<const N: usize = 16> {
    open: [bool; N],
    /// When `false`, opening one section closes the others.
    pub multiple: bool,
}

impl<const N: usize> AccordionState<N> {
    /// Creates a state where several sections may be open at once.
    pub const fn new() -> Self {
        Self {
            open: [false; N],
            multiple: true,
        }
    }

    /// Creates a state where only one section is open at a time.
    pub const fn single() -> Self {
        Self {
            open: [false; N],
            multiple: false,
        }
    }

    /// Returns `true` when section `index` is open.
    pub fn is_open(&self, index: usize) -> bool {
        self.open.get(index).copied().unwrap_or(false)
    }

    /// Opens or closes section `index`.
    pub fn set_open(&mut self, index: usize, open: bool) {
        if open && !self.multiple {
            self.open = [false; N];
        }
        if let Some(slot) = self.open.get_mut(index) {
            *slot = open;
        }
    }

    /// Toggles section `index`, respecting single-open mode.
    pub fn toggle(&mut self, index: usize) {
        let open = self.is_open(index);
        if !open && !self.multiple {
            self.open = [false; N];
        }
        if let Some(slot) = self.open.get_mut(index) {
            *slot = !open;
        }
    }

    /// The raw expansion flags.
    pub fn as_slice(&self) -> &[bool] {
        &self.open
    }
}

impl<const N: usize> Default for AccordionState<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Persistent state for [`Ui::split_pane`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitState {
    /// Position of the divider, from `0.0` to `1.0`.
    pub fraction: f32,
    /// Smallest share given to the first pane.
    pub min: f32,
    /// Largest share given to the first pane.
    pub max: f32,
}

impl SplitState {
    /// An even split.
    pub const fn new() -> Self {
        Self {
            fraction: 0.5,
            min: 0.1,
            max: 0.9,
        }
    }

    /// A split at a specific fraction, clamped to the default limits.
    pub const fn at(fraction: f32) -> Self {
        Self {
            fraction,
            min: 0.1,
            max: 0.9,
        }
    }

    fn clamped(&self) -> f32 {
        let fraction = if self.fraction.is_finite() {
            self.fraction
        } else {
            0.5
        };
        let low = self.min.clamp(0.0, 1.0);
        let high = self.max.clamp(0.0, 1.0);
        fraction.clamp(low.min(high), high.max(low))
    }
}

impl Default for SplitState {
    fn default() -> Self {
        Self::new()
    }
}

/// The rectangles and divider interaction produced by [`Ui::split_pane`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitResponse {
    /// The divider's drag response.
    pub response: crate::Response,
    /// The first (left or top) pane.
    pub first: Rect,
    /// The second (right or bottom) pane.
    pub second: Rect,
    /// The draggable divider.
    pub divider: Rect,
}

/// The edge a [`Ui::drawer`] slides in from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DrawerSide {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

/// Persistent state for [`Ui::drawer`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawerState {
    /// Whether the drawer is visible.
    pub open: bool,
}

impl DrawerState {
    pub const fn new() -> Self {
        Self { open: false }
    }
}

/// Persistent state for [`Ui::popover`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PopoverState {
    /// Whether the floating panel is visible.
    pub open: bool,
}

impl PopoverState {
    pub const fn new() -> Self {
        Self { open: false }
    }
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// One accordion section.
    ///
    /// The header is always drawn. When the section is open, `f` runs inside
    /// the body. `body_height` is the space reserved for the body, which keeps
    /// layout deterministic without measuring the closure twice.
    ///
    /// Returns the header response and, when the body was drawn this frame,
    /// the closure's value.
    pub fn accordion_section<const M: usize, R>(
        &mut self,
        state: &mut AccordionState<M>,
        index: usize,
        title: &str,
        body_height: i32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> (crate::Response, Option<R>) {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let header_height = metrics.control_height;
        let padding = metrics.padding_x;
        let open_before = state.is_open(index);
        let body_height = body_height.max(0);
        let total = header_height
            + if open_before {
                body_height + padding * 2
            } else {
                0
            };
        let rect = self.allocate(280, total);
        let (header, _) = rect.split_top(header_height);
        let id = self.next_id();

        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );

        let mut response = self.interact(id, header, Sense::click());
        if response.hovered {
            self.painter
                .fill_rounded_rect(header.inset(1), metrics.radius, palette.surface_hover);
        }
        if response.focused {
            self.painter
                .stroke_rounded_rect(header.inset(1), metrics.radius, 2, palette.focus);
        }
        self.painter.draw_text_ellipsis(
            Rect::new(
                header.x + padding,
                header.y,
                header.width.saturating_sub((padding * 2 + 24) as u32),
                header.height,
            ),
            title,
            palette.text,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        paint_icon(
            &mut self.painter,
            Rect::new(
                header.right_i32() - 24,
                header.y + (header_height - 14) / 2,
                14,
                14,
            ),
            if open_before {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            },
            palette.text_muted,
        );
        if response.clicked {
            state.toggle(index);
            response.changed = true;
        }

        let open_now = state.is_open(index);
        let mut value: Option<R> = None;
        if open_now && open_before {
            self.painter.fill_rect(
                Rect::new(rect.x, header.bottom_i32() - 1, rect.width, 1),
                palette.border,
            );
            let body = Rect::new(
                rect.x + padding,
                header.bottom_i32() + padding,
                rect.width.saturating_sub((padding * 2) as u32),
                body_height.max(0) as u32,
            );
            let clip = self.painter.push_clip(body);
            let result = self.column_with(body, metrics.gap, f).1;
            self.painter.restore_clip(clip);
            value = Some(result);
        } else if open_before {
            // The section is closing; leave the reserved area blank until the
            // next frame, when the layout no longer includes it.
        }

        (response, value)
    }

    /// An accordion section that returns only the header response.
    pub fn accordion<const M: usize>(
        &mut self,
        state: &mut AccordionState<M>,
        index: usize,
        title: &str,
        body_height: i32,
        f: impl FnOnce(&mut Self),
    ) -> crate::Response {
        self.accordion_section(state, index, title, body_height, f)
            .0
    }

    /// Two panes separated by a draggable divider.
    ///
    /// [`Axis::Horizontal`] places the panes side by side; [`Axis::Vertical`]
    /// stacks them. Each closure runs in a column filling its pane.
    pub fn split_pane<R1, R2>(
        &mut self,
        rect: Rect,
        axis: Axis,
        state: &mut SplitState,
        first: impl FnOnce(&mut Self) -> R1,
        second: impl FnOnce(&mut Self) -> R2,
    ) -> SplitResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let thickness = metrics.gap.max(6);
        let fraction = state.clamped();

        let (first_rect, divider, second_rect) = match axis {
            Axis::Horizontal => {
                let usable = (rect.width as i32 - thickness).max(0);
                let first_size = (usable as f32 * fraction) as i32;
                (
                    Rect::new(rect.x, rect.y, first_size.max(0) as u32, rect.height),
                    Rect::new(rect.x + first_size, rect.y, thickness as u32, rect.height),
                    Rect::new(
                        rect.x + first_size + thickness,
                        rect.y,
                        (usable - first_size).max(0) as u32,
                        rect.height,
                    ),
                )
            }
            Axis::Vertical => {
                let usable = (rect.height as i32 - thickness).max(0);
                let first_size = (usable as f32 * fraction) as i32;
                (
                    Rect::new(rect.x, rect.y, rect.width, first_size.max(0) as u32),
                    Rect::new(rect.x, rect.y + first_size, rect.width, thickness as u32),
                    Rect::new(
                        rect.x,
                        rect.y + first_size + thickness,
                        rect.width,
                        (usable - first_size).max(0) as u32,
                    ),
                )
            }
        };

        let id = self.next_id();
        let response = self.interact(id, divider, Sense::drag());
        if response.active && self.pointer_down() {
            let pointer = self.pointer();
            let position = match axis {
                Axis::Horizontal => pointer.x - rect.x,
                Axis::Vertical => pointer.y - rect.y,
            };
            let total = match axis {
                Axis::Horizontal => rect.width as i32,
                Axis::Vertical => rect.height as i32,
            };
            if total > 0 {
                state.fraction = (position as f32 / total as f32).clamp(0.0, 1.0);
            }
        }

        self.painter.fill_rect(divider, palette.surface_alt);
        self.painter.fill_rect(
            match axis {
                Axis::Horizontal => Rect::new(divider.x, divider.y, 1, divider.height),
                Axis::Vertical => Rect::new(divider.x, divider.y, divider.width, 1),
            },
            palette.border,
        );
        let highlight = response.hovered || response.active;
        let grip = if axis == Axis::Horizontal {
            Rect::new(
                divider.x + thickness / 2 - 1,
                divider.y + (divider.height as i32 - 18) / 2,
                2,
                18,
            )
        } else {
            Rect::new(
                divider.x + (divider.width as i32 - 18) / 2,
                divider.y + thickness / 2 - 1,
                18,
                2,
            )
        };
        self.painter.fill_rounded_rect(
            grip,
            1,
            if highlight {
                palette.focus
            } else {
                palette.border_strong
            },
        );

        self.column(first_rect, metrics.gap, first);
        self.column(second_rect, metrics.gap, second);

        SplitResponse {
            response,
            first: first_rect,
            second: second_rect,
            divider,
        }
    }

    /// A modal drawer anchored to one edge of `anchor`.
    ///
    /// The drawer paints a scrim, a titled surface and the content produced by
    /// `f`. It closes on Escape, on the close button, or on a click outside the
    /// panel. Draw it at the end of the frame so it sits above other content.
    pub fn drawer<R>(
        &mut self,
        anchor: Rect,
        side: DrawerSide,
        state: &mut DrawerState,
        size: i32,
        title: &str,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if !state.open {
            return None;
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let padding = metrics.padding_x;
        let panel = match side {
            DrawerSide::Left => {
                let width = size.clamp(0, anchor.width as i32);
                Rect::new(anchor.x, anchor.y, width as u32, anchor.height)
            }
            DrawerSide::Right => {
                let width = size.clamp(0, anchor.width as i32);
                Rect::new(
                    anchor.right_i32() - width,
                    anchor.y,
                    width as u32,
                    anchor.height,
                )
            }
            DrawerSide::Top => {
                let height = size.clamp(0, anchor.height as i32);
                Rect::new(anchor.x, anchor.y, anchor.width, height as u32)
            }
            DrawerSide::Bottom => {
                let height = size.clamp(0, anchor.height as i32);
                Rect::new(
                    anchor.x,
                    anchor.bottom_i32() - height,
                    anchor.width,
                    height as u32,
                )
            }
        };

        self.painter.fill_rect_alpha(anchor, palette.overlay, 120);
        self.painter.shadow(panel, metrics.radius, 12, 160);
        self.painter
            .fill_rounded_rect(panel, metrics.radius, palette.surface);
        self.painter.stroke_rounded_rect(
            panel,
            metrics.radius,
            metrics.border_width,
            palette.border_strong,
        );

        let title_bar = Rect::new(
            panel.x,
            panel.y,
            panel.width,
            metrics.title_bar_height as u32,
        );
        self.painter
            .fill_rounded_rect(title_bar, metrics.radius, palette.surface_alt);
        self.painter.fill_rect(
            Rect::new(title_bar.x, title_bar.bottom_i32() - 1, title_bar.width, 1),
            palette.border,
        );

        let id = self.next_id();
        let close_size = (metrics.title_bar_height - 14).max(12);
        let close_rect = Rect::new(
            title_bar.right_i32() - close_size - 7,
            title_bar.y + (metrics.title_bar_height - close_size) / 2,
            close_size as u32,
            close_size as u32,
        );
        let close = self.interact(id, close_rect, Sense::click());
        paint_icon(
            &mut self.painter,
            close_rect.inset(2),
            Icon::Close,
            if close.hovered {
                palette.danger
            } else {
                palette.text_muted
            },
        );
        self.painter.draw_text_ellipsis(
            Rect::new(
                title_bar.x + padding,
                title_bar.y,
                title_bar
                    .width
                    .saturating_sub((padding * 2 + close_size + 14) as u32),
                title_bar.height,
            ),
            title,
            palette.text,
            metrics.title_scale,
            Align::Start,
            VerticalAlign::Middle,
        );

        let content = Rect::new(
            panel.x + padding,
            panel.y + metrics.title_bar_height + padding,
            panel.width.saturating_sub((padding * 2) as u32),
            panel
                .height
                .saturating_sub((metrics.title_bar_height + padding * 2) as u32),
        );
        let clip = self.painter.push_clip(content);
        let value = self.column_with(content, metrics.gap, f).1;
        self.painter.restore_clip(clip);

        let outside_clicked =
            self.pointer_pressed() && self.pointer_over(anchor) && !self.pointer_over(panel);
        if close.clicked || self.escape_pressed() || outside_clicked {
            state.open = false;
        }
        Some(value)
    }

    /// A floating panel anchored below `anchor`.
    ///
    /// `width` and `height` are the panel size; the panel flips above the
    /// anchor when there is not enough room. A click outside or Escape closes
    /// it. Draw it late in the frame so it floats above the content.
    pub fn popover<R>(
        &mut self,
        anchor: Rect,
        state: &mut PopoverState,
        width: i32,
        height: i32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if !state.open {
            return None;
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let padding = metrics.padding_x;
        let canvas = self.canvas_rect();
        let width = width.clamp(0, canvas.width as i32);
        let height = height.clamp(0, canvas.height as i32);
        let below = anchor.bottom_i32() + 2;
        let y = if below + height <= canvas.bottom_i32() {
            below
        } else {
            (anchor.y - height - 2).max(canvas.y)
        };
        let x = anchor
            .x
            .clamp(canvas.x, (canvas.right_i32() - width).max(canvas.x));
        let panel = Rect::new(x, y, width as u32, height as u32);

        self.painter.shadow(panel, metrics.radius.min(8), 10, 150);
        self.painter
            .fill_rounded_rect(panel, metrics.radius.min(8), palette.surface);
        self.painter.stroke_rounded_rect(
            panel,
            metrics.radius.min(8),
            metrics.border_width,
            palette.border_strong,
        );

        let content = Padding::all(padding).apply(panel);
        let clip = self.painter.push_clip(content);
        let value = self.column_with(content, metrics.gap, f).1;
        self.painter.restore_clip(clip);

        let outside_clicked = self.pointer_pressed()
            && self.pointer_over(canvas)
            && !self.pointer_over(panel)
            && !self.pointer_over(anchor);
        if outside_clicked || self.escape_pressed() {
            state.open = false;
        }
        Some(value)
    }
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A settings row with an icon, title, description and trailing control.
    ///
    /// This is the WinUI settings-card pattern: text on the left, a control
    /// built by `f` on the right. The closure runs in a right-aligned column,
    /// so ordinary widgets can be used without special sizing.
    pub fn settings_card<R>(
        &mut self,
        rect: Rect,
        icon: Icon,
        title: &str,
        description: &str,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let padding = metrics.padding_x;

        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );

        let icon_size = (metrics.control_height - 2).min(34);
        let icon_rect = Rect::new(
            rect.x + padding,
            rect.y + (rect.height as i32 - icon_size) / 2,
            icon_size as u32,
            icon_size as u32,
        );
        self.painter.fill_rounded_rect(
            icon_rect,
            (icon_size / 2) as u32,
            palette.accent.lerp(palette.surface, 200),
        );
        paint_icon(&mut self.painter, icon_rect.inset(8), icon, palette.accent);

        let control_width = (rect.width as i32 / 3).clamp(120, 280);
        let text_x = icon_rect.right_i32() + padding;
        let text_width =
            (rect.right_i32() - padding - control_width - metrics.gap - text_x).max(0) as u32;
        let line = self.painter.line_height(metrics.font_scale);
        if description.is_empty() {
            self.painter.draw_text_ellipsis(
                Rect::new(text_x, rect.y, text_width, rect.height),
                title,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
        } else {
            let total = line + 4 + metrics.small_scale as i32 * 8;
            let top = rect.y + (rect.height as i32 - total) / 2;
            self.painter.draw_text_ellipsis(
                Rect::new(text_x, top, text_width, line as u32),
                title,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Top,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(text_x, top + line + 4, text_width, metrics.small_scale * 9),
                description,
                palette.text_muted,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Top,
            );
        }

        let control = Rect::new(
            rect.right_i32() - padding - control_width,
            rect.y + metrics.padding_y,
            control_width as u32,
            rect.height.saturating_sub((metrics.padding_y * 2) as u32),
        );
        let clip = self.painter.push_clip(control);
        let (_, value) = self.column_with(control, metrics.gap, f);
        self.painter.restore_clip(clip);
        value
    }

    /// A section title with an optional right-aligned action link.
    ///
    /// Returns the action response; `clicked` is `true` when the link was
    /// activated.
    pub fn section_header(&mut self, title: &str, action: Option<&str>) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let height = metrics.heading_scale as i32 * 8 + metrics.padding_y * 2;
        let rect = self.allocate(0, height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        self.painter.draw_text_ellipsis(
            rect,
            title,
            palette.text,
            metrics.heading_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        if let Some(action) = action {
            let width = self
                .painter
                .text_width(action, metrics.font_scale)
                .max(metrics.min_control_size);
            let link = Rect::new(
                rect.right_i32() - width,
                rect.y,
                width.min(rect.width as i32) as u32,
                rect.height,
            );
            let hit = self.interact(id.child(1), link, Sense::click());
            if hit.clicked {
                response.clicked = true;
                response.changed = true;
            }
            self.painter.draw_text_ellipsis(
                link,
                action,
                if hit.hovered || hit.focused {
                    palette.accent_hover
                } else {
                    palette.accent
                },
                metrics.font_scale,
                Align::End,
                VerticalAlign::Middle,
            );
            if hit.focused {
                self.painter
                    .stroke_rounded_rect(link.inset(2), 4, 1, palette.focus);
            }
        }
        response
    }

    /// A large accent banner with an eyebrow, display title, message and an
    /// action area built by `f`.
    pub fn hero_banner<R>(
        &mut self,
        rect: Rect,
        eyebrow: &str,
        title: &str,
        message: &str,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let padding = (metrics.padding_x * 2).max(20);

        self.painter.fill_rounded_rect(
            rect,
            metrics.radius,
            palette.accent.lerp(palette.surface, 40),
        );
        self.painter.fill_circle(
            rect.right_i32() - 40,
            rect.y + 30,
            70,
            palette.accent.lerp(palette.surface, 70),
        );
        self.painter.fill_circle(
            rect.right_i32() - 120,
            rect.bottom_i32() - 10,
            46,
            palette.accent.lerp(palette.surface, 55),
        );

        let text_width = (rect.width as i32 - padding * 2).max(0) as u32;
        let mut y = rect.y + padding;
        self.painter.draw_text_ellipsis(
            Rect::new(rect.x + padding, y, text_width, metrics.small_scale * 10),
            eyebrow,
            palette.accent_text,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Top,
        );
        y += metrics.small_scale as i32 * 10 + 4;
        self.painter.draw_text_ellipsis(
            Rect::new(rect.x + padding, y, text_width, metrics.display_scale * 9),
            title,
            palette.accent_text,
            metrics.display_scale,
            Align::Start,
            VerticalAlign::Top,
        );
        y += metrics.display_scale as i32 * 9 + 6;
        let message_height =
            self.painter
                .wrapped_text_height(text_width as i32, message, metrics.font_scale);
        self.painter.draw_text_wrapped(
            Rect::new(
                rect.x + padding,
                y,
                text_width,
                message_height.max(0) as u32,
            ),
            message,
            palette.accent_text.lerp(palette.accent, 80),
            metrics.font_scale,
        );
        y += message_height + metrics.gap * 2;

        let actions = Rect::new(
            rect.x + padding,
            y,
            text_width,
            (rect.bottom_i32() - padding - y).max(0) as u32,
        );
        let clip = self.painter.push_clip(actions);
        let (_, value) = self.row_with(actions, metrics.gap, f);
        self.painter.restore_clip(clip);
        value
    }
}
