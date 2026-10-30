//! Feedback components: alerts, toasts, spinners, skeletons, empty states,
//! loading overlays, progress rings and confirmation dialogs.

use feos_gfx::Rect;

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::layout::Axis;
use crate::math::{cos, sin};
use crate::response::Response;
use crate::response::Sense;
use crate::ui::Ui;

use super::icons::{paint_icon, Icon};
use super::Tone;

/// The result of [`Ui::confirm_dialog`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConfirmResult {
    /// The confirm button was pressed.
    pub confirmed: bool,
    /// The cancel button was pressed.
    pub cancelled: bool,
    /// The dialog was closed with its title-bar button.
    pub closed: bool,
}

impl ConfirmResult {
    /// Returns `true` when the user confirmed.
    pub const fn is_confirmed(self) -> bool {
        self.confirmed
    }

    /// Returns `true` when the dialog should be considered finished.
    pub const fn is_finished(self) -> bool {
        self.confirmed || self.cancelled || self.closed
    }
}

/// One transient notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Toast {
    /// Semantic colour and icon.
    pub tone: Tone,
    /// The message. Static text keeps the queue allocation-free.
    pub message: &'static str,
    /// Frame at which the toast was created.
    pub created: u64,
    /// Lifetime in frames, or `0` for a persistent toast.
    pub ttl: u32,
}

/// A fixed-capacity toast queue.
///
/// Push toasts when an action completes, call [`tick`](ToastQueue::tick) (or
/// let [`Ui::toasts`] do it), and draw the queue last so it floats above the
/// rest of the frame.
pub struct ToastQueue<const N: usize = 4> {
    entries: [Option<Toast>; N],
    len: usize,
}

impl<const N: usize> ToastQueue<N> {
    /// Creates an empty queue.
    pub const fn new() -> Self {
        Self {
            entries: [None; N],
            len: 0,
        }
    }

    /// Number of queued toasts.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when no toasts are queued.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Adds a toast. When the queue is full the oldest toast is dropped.
    pub fn push(&mut self, tone: Tone, message: &'static str, ttl: u32, frame: u64) {
        if N == 0 {
            return;
        }
        let toast = Toast {
            tone,
            message,
            created: frame,
            ttl,
        };
        if self.len < N {
            self.entries[self.len] = Some(toast);
            self.len += 1;
        } else {
            self.entries.copy_within(1..N, 0);
            self.entries[N - 1] = Some(toast);
        }
    }

    /// Returns a toast by index.
    pub fn get(&self, index: usize) -> Option<Toast> {
        if index < self.len {
            self.entries[index]
        } else {
            None
        }
    }

    /// Removes a toast, shifting newer entries down.
    pub fn dismiss(&mut self, index: usize) {
        if index >= self.len {
            return;
        }
        self.entries.copy_within(index + 1..self.len, index);
        self.len -= 1;
        self.entries[self.len] = None;
    }

    /// Removes toasts whose lifetime has elapsed.
    pub fn tick(&mut self, frame: u64) {
        let mut index = 0;
        while index < self.len {
            let expired = self.entries[index].is_some_and(|toast| {
                toast.ttl > 0 && frame >= toast.created.saturating_add(toast.ttl as u64)
            });
            if expired {
                self.dismiss(index);
            } else {
                index += 1;
            }
        }
    }

    /// Iterates over the queued toasts, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &Toast> + '_ {
        self.entries[..self.len]
            .iter()
            .filter_map(|entry| entry.as_ref())
    }
}

impl<const N: usize> Default for ToastQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// An informational alert with a title and optional message.
    pub fn alert(&mut self, tone: Tone, title: &str, message: &str) -> Rect {
        self.alert_inner(tone, title, message, None).rect
    }

    /// An alert with a close button.
    ///
    /// Sets `open` to `false` when the user dismisses it.
    pub fn alert_dismissible(
        &mut self,
        tone: Tone,
        title: &str,
        message: &str,
        open: &mut bool,
    ) -> Response {
        if !*open {
            return Response::empty(crate::Id::NONE, Rect::new(0, 0, 0, 0));
        }
        let response = self.alert_inner(tone, title, message, Some(open));
        if response.changed {
            *open = false;
        }
        response
    }

    fn alert_inner(
        &mut self,
        tone: Tone,
        title: &str,
        message: &str,
        close: Option<&mut bool>,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let scale = metrics.font_scale;
        let icon_size = (metrics.control_height - 8).max(14);
        let padding = metrics.padding_x;
        let gap = metrics.gap;
        let available = self.remaining().0;
        let width = if self.axis() == Axis::Vertical {
            available.max(1)
        } else {
            360.min(available.max(1))
        };
        let text_width =
            (width - padding * 2 - icon_size - gap - if close.is_some() { 24 } else { 0 }).max(1);
        let title_height = self.painter.line_height(scale);
        let message_height = if message.is_empty() {
            0
        } else {
            self.painter.wrapped_text_height(text_width, message, scale) + 4
        };
        let height = (title_height + message_height).max(icon_size) + padding * 2;
        let rect = self.allocate(
            if self.axis() == Axis::Vertical {
                0
            } else {
                width
            },
            height,
        );
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let color = tone.color(palette);

        self.painter
            .fill_rounded_rect(rect, metrics.radius, tone.tint(palette));
        self.painter
            .stroke_rounded_rect(rect, metrics.radius, metrics.border_width, color);
        self.painter.fill_rect(
            Rect::new(rect.x, rect.y + 6, 3, rect.height.saturating_sub(12)),
            color,
        );
        paint_icon(
            &mut self.painter,
            Rect::new(
                rect.x + padding,
                rect.y + padding + (title_height - icon_size).max(0) / 2,
                icon_size as u32,
                icon_size as u32,
            ),
            tone.icon(),
            color,
        );
        let text_x = rect.x + padding + icon_size + gap;
        let text_area = Rect::new(
            text_x,
            rect.y + padding,
            (rect.right_i32() - text_x - padding).max(0) as u32,
            rect.height.saturating_sub((padding * 2) as u32),
        );
        self.painter.draw_text_ellipsis(
            Rect::new(
                text_area.x,
                text_area.y,
                text_area.width,
                title_height as u32,
            ),
            title,
            palette.text,
            scale,
            Align::Start,
            VerticalAlign::Top,
        );
        if !message.is_empty() {
            self.painter.draw_text_wrapped(
                Rect::new(
                    text_area.x,
                    text_area.y + title_height + 4,
                    text_area.width,
                    message_height.max(0) as u32,
                ),
                message,
                palette.text_muted,
                scale,
            );
        }

        if close.is_some() {
            let close_size = (metrics.control_height - 12).max(12);
            let close_rect = Rect::new(
                rect.right_i32() - padding - close_size,
                rect.y + padding,
                close_size as u32,
                close_size as u32,
            );
            let hit = self.interact(
                id.child(0xc105e),
                close_rect,
                Sense::HOVER.with(Sense::CLICK),
            );
            paint_icon(
                &mut self.painter,
                close_rect.inset(2),
                Icon::Close,
                if hit.hovered {
                    palette.text
                } else {
                    palette.text_muted
                },
            );
            if hit.clicked {
                response.changed = true;
                response.clicked = true;
            }
        }
        response
    }

    /// Draws the toast queue at the top-right of `anchor`.
    ///
    /// Call this after the rest of the frame so notifications float above the
    /// content. Expired toasts are removed automatically.
    pub fn toasts<const M: usize>(&mut self, anchor: Rect, queue: &mut ToastQueue<M>) -> Rect {
        let frame = self.state().frame();
        queue.tick(frame);
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = 300.min(anchor.width.saturating_sub(24)) as i32;
        if width <= 0 {
            return Rect::new(anchor.x, anchor.y, 0, 0);
        }
        let height = (metrics.control_height + metrics.padding_y * 2).max(46);
        let gap = metrics.gap;
        let mut y = anchor.y + 16;
        let mut index = 0;
        while index < queue.len() {
            let Some(toast) = queue.get(index) else {
                break;
            };
            let rect = Rect::new(
                anchor.right_i32() - width - 16,
                y,
                width as u32,
                height as u32,
            );
            let color = toast.tone.color(palette);
            self.painter.shadow(rect, metrics.radius.min(8), 8, 120);
            self.painter
                .fill_rounded_rect(rect, metrics.radius.min(8), palette.surface);
            self.painter.stroke_rounded_rect(
                rect,
                metrics.radius.min(8),
                metrics.border_width,
                color,
            );
            self.painter.fill_rect(
                Rect::new(rect.x, rect.y + 6, 3, rect.height.saturating_sub(12)),
                color,
            );
            paint_icon(
                &mut self.painter,
                Rect::new(
                    rect.x + metrics.padding_x,
                    rect.y + (rect.height as i32 - 16) / 2,
                    16,
                    16,
                ),
                toast.tone.icon(),
                color,
            );
            let close_size = (metrics.control_height - 16).max(10);
            let close_rect = Rect::new(
                rect.right_i32() - metrics.padding_x - close_size,
                rect.y + (rect.height as i32 - close_size) / 2,
                close_size as u32,
                close_size as u32,
            );
            let close = self.interact(
                crate::Id::from_name(toast.message).child(index as u64),
                close_rect,
                Sense::HOVER.with(Sense::CLICK),
            );
            paint_icon(
                &mut self.painter,
                close_rect.inset(2),
                Icon::Close,
                if close.hovered {
                    palette.text
                } else {
                    palette.text_muted
                },
            );
            let text_x = rect.x + metrics.padding_x + 16 + metrics.gap;
            self.painter.draw_text_ellipsis(
                Rect::new(
                    text_x,
                    rect.y,
                    (close_rect.x - text_x - metrics.gap).max(0) as u32,
                    rect.height,
                ),
                toast.message,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            if close.clicked {
                queue.dismiss(index);
            } else {
                index += 1;
            }
            y += height + gap;
        }
        Rect::new(
            anchor.right_i32() - width - 16,
            anchor.y + 16,
            width as u32,
            (y - anchor.y - 16).max(0) as u32,
        )
    }

    /// A small animated loading spinner.
    pub fn spinner(&mut self, size: i32) -> Rect {
        let size = size.max(12);
        let rect = self.allocate_sized(size, size);
        self.spinner_at(rect);
        rect
    }

    /// Draws a spinner centered in `rect`.
    pub fn spinner_at(&mut self, rect: Rect) {
        let frame = self.state().frame() as usize;
        let palette = self.theme.palette;
        let size = rect.width.min(rect.height) as i32;
        let radius = (size / 2 - 2).max(2);
        let dot_radius = (size / 10).max(1) as u32;
        let center = rect.center();
        for index in 0..8 {
            let angle = index as f32 * core::f32::consts::TAU / 8.0;
            let x = center.x + (cos(angle) * radius as f32) as i32;
            let y = center.y + (sin(angle) * radius as f32) as i32;
            let active = index == frame % 8;
            self.painter.fill_circle(
                x,
                y,
                dot_radius,
                if active {
                    palette.accent
                } else {
                    palette.border_strong
                },
            );
        }
    }

    /// A rounded skeleton block with a subtle shimmer.
    pub fn skeleton(&mut self, rect: Rect) {
        let frame = self.state().frame();
        let palette = self.theme.palette;
        self.painter.fill_rounded_rect(rect, 4, palette.surface_alt);
        if rect.width > 8 {
            let band_width = (rect.width / 3).max(12) as i32;
            let travel = rect.width as i32 + band_width;
            let x = rect.x - band_width + (frame as i32 * 6) % travel.max(1);
            let old_clip = self.painter.push_clip(rect);
            self.painter.fill_rect_alpha(
                Rect::new(x, rect.y, band_width as u32, rect.height),
                palette.accent,
                18,
            );
            self.painter.restore_clip(old_clip);
        }
    }

    /// A stack of skeleton lines with varied widths.
    pub fn skeleton_lines(&mut self, lines: usize) -> Rect {
        let lines = lines.max(1);
        let line_height = 10;
        let gap = 6;
        let height = lines as i32 * (line_height + gap) - gap;
        let rect = self.allocate(0, height);
        let widths = [0.55f32, 0.9, 0.72, 0.8, 0.45];
        for index in 0..lines {
            let width = (rect.width as f32 * widths[index % widths.len()]) as i32;
            self.skeleton(Rect::new(
                rect.x,
                rect.y + index as i32 * (line_height + gap),
                width.max(1) as u32,
                line_height as u32,
            ));
        }
        rect
    }

    /// A centered empty-state placeholder.
    pub fn empty_state(&mut self, rect: Rect, icon: Icon, title: &str, message: &str) -> Rect {
        let palette = self.theme.palette;
        let metrics = self.theme.metrics;
        let icon_size = (metrics.control_height * 2).min(rect.width as i32).max(16);
        let title_height = self.painter.line_height(metrics.heading_scale);
        let message_height = if message.is_empty() {
            0
        } else {
            self.painter.wrapped_text_height(
                rect.width.saturating_sub(40).max(1) as i32,
                message,
                metrics.font_scale,
            )
        };
        let total = icon_size + metrics.gap + title_height + message_height;
        let mut y = rect.y + (rect.height as i32 - total) / 2;
        paint_icon(
            &mut self.painter,
            Rect::new(
                rect.x + (rect.width as i32 - icon_size) / 2,
                y,
                icon_size as u32,
                icon_size as u32,
            ),
            icon,
            palette.text_muted,
        );
        y += icon_size + metrics.gap;
        self.painter.draw_text_aligned(
            Rect::new(rect.x, y, rect.width, title_height as u32),
            title,
            palette.text,
            metrics.heading_scale,
            Align::Center,
            VerticalAlign::Top,
        );
        y += title_height;
        if !message.is_empty() {
            self.painter.draw_text_wrapped(
                Rect::new(
                    rect.x + 20,
                    y,
                    rect.width.saturating_sub(40),
                    message_height.max(0) as u32,
                ),
                message,
                palette.text_muted,
                metrics.font_scale,
            );
        }
        Rect::new(
            rect.x,
            y,
            rect.width,
            (message_height + title_height + icon_size) as u32,
        )
    }

    /// Dims `rect` and draws a centered spinner plus message.
    pub fn loading_overlay(&mut self, rect: Rect, message: &str) {
        let palette = self.theme.palette;
        let metrics = self.theme.metrics;
        self.painter.fill_rect_alpha(rect, palette.background, 180);
        let size = 28;
        let center = rect.center();
        let spinner = Rect::new(
            center.x - size / 2,
            center.y - size / 2 - if message.is_empty() { 0 } else { 8 },
            size as u32,
            size as u32,
        );
        self.spinner_at(spinner);
        if !message.is_empty() {
            self.painter.draw_text_aligned(
                Rect::new(rect.x, spinner.bottom_i32() + 6, rect.width, 20),
                message,
                palette.text_muted,
                metrics.font_scale,
                Align::Center,
                VerticalAlign::Top,
            );
        }
    }

    /// A circular progress gauge with an optional centered label.
    pub fn progress_ring(&mut self, rect: Rect, value: f32, label: &str) -> Rect {
        let palette = self.theme.palette;
        let metrics = self.theme.metrics;
        let size = rect.width.min(rect.height) as i32;
        let cx = rect.x + rect.width as i32 / 2;
        let cy = rect.y + rect.height as i32 / 2;
        let radius = (size / 2 - 3).max(4);
        let thickness = (size / 12).clamp(2, 6);
        let value = value.clamp(0.0, 1.0);
        for step in 0..64 {
            let t = step as f32 / 64.0;
            let angle = -core::f32::consts::FRAC_PI_2 + t * core::f32::consts::TAU;
            let x = cx + (cos(angle) * radius as f32) as i32;
            let y = cy + (sin(angle) * radius as f32) as i32;
            self.painter.fill_circle(
                x,
                y,
                (thickness / 2).max(1) as u32,
                if t <= value {
                    palette.accent
                } else {
                    palette.surface_active
                },
            );
        }
        if !label.is_empty() {
            self.painter.draw_text_centered(
                Rect::new(
                    cx - radius,
                    cy - metrics.font_scale as i32 * 4,
                    (radius * 2) as u32,
                    (metrics.font_scale as i32 * 8) as u32,
                ),
                label,
                palette.text,
                metrics.font_scale,
            );
        }
        rect
    }

    /// A modal confirmation dialog.
    ///
    /// The dialog draws its own overlay; skip interaction with the content
    /// underneath while it is open. `open` is cleared when the user confirms,
    /// cancels or closes the dialog.
    pub fn confirm_dialog(
        &mut self,
        rect: Rect,
        title: &str,
        message: &str,
        open: &mut bool,
        confirm_label: &str,
        cancel_label: &str,
    ) -> ConfirmResult {
        if !*open {
            return ConfirmResult::default();
        }
        let mut result = ConfirmResult::default();
        let mut dialog_open = true;
        let closed = self.dialog(rect, title, &mut dialog_open, |ui| {
            ui.paragraph(message);
            let row = ui.allocate(0, ui.theme.metrics.control_height);
            ui.row(row, 8, |ui| {
                if ui.button_sized(100, cancel_label).clicked {
                    result.cancelled = true;
                }
                if ui.primary_button_sized(100, confirm_label).clicked {
                    result.confirmed = true;
                }
            });
        });
        if closed {
            result.closed = true;
        }
        if result.is_finished() || !dialog_open {
            *open = false;
        }
        result
    }
}

/// The result of [`Ui::info_bar`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InfoBarResponse {
    /// Interaction with the bar.
    pub response: Response,
    /// The optional action button was pressed.
    pub action_clicked: bool,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A WinUI-style info bar with a severity, title, message, optional action
    /// and a close button.
    ///
    /// The bar spans the available width in a column. It sets `open` to
    /// `false` when dismissed; the action button only reports a click, so the
    /// caller decides whether that also dismisses the bar.
    pub fn info_bar(
        &mut self,
        tone: Tone,
        title: &str,
        message: &str,
        action: Option<&str>,
        open: &mut bool,
    ) -> InfoBarResponse {
        if !*open {
            return InfoBarResponse {
                response: Response::empty(crate::Id::NONE, Rect::new(0, 0, 0, 0)),
                action_clicked: false,
            };
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let scale = metrics.font_scale;
        let padding = metrics.padding_x;
        let icon_size = (metrics.control_height - 8).max(14);
        let close_size = (metrics.control_height - 14).max(12);
        let width = if self.axis() == Axis::Vertical {
            self.remaining().0.max(1)
        } else {
            460.min(self.remaining().0.max(1))
        };
        let action_width = action.map_or(0, |label| {
            self.painter
                .text_width(label, scale)
                .saturating_add(padding * 2)
                .max(72)
                .min((width - padding * 2 - close_size - metrics.gap).max(0))
        });
        let text_width = (width
            - padding * 2
            - icon_size
            - metrics.gap
            - close_size
            - metrics.gap
            - if action_width > 0 {
                action_width + metrics.gap
            } else {
                0
            })
        .max(1);
        let title_height = self.painter.line_height(scale);
        let message_height = if message.is_empty() {
            0
        } else {
            self.painter.wrapped_text_height(text_width, message, scale)
        };
        let height = (title_height + message_height).max(icon_size) + padding * 2;
        let rect = self.allocate(
            if self.axis() == Axis::Vertical {
                0
            } else {
                width
            },
            height,
        );
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let color = tone.color(palette);

        self.painter
            .fill_rounded_rect(rect, metrics.radius, tone.tint(palette));
        self.painter
            .stroke_rounded_rect(rect, metrics.radius, metrics.border_width, color);
        self.painter.fill_rect(
            Rect::new(rect.x, rect.y + 6, 3, rect.height.saturating_sub(12)),
            color,
        );
        paint_icon(
            &mut self.painter,
            Rect::new(
                rect.x + padding,
                rect.y + padding + (title_height - icon_size).max(0) / 2,
                icon_size as u32,
                icon_size as u32,
            ),
            tone.icon(),
            color,
        );
        let text_x = rect.x + padding + icon_size + metrics.gap;
        self.painter.draw_text_ellipsis(
            Rect::new(
                text_x,
                rect.y + padding,
                text_width as u32,
                title_height as u32,
            ),
            title,
            palette.text,
            scale,
            Align::Start,
            VerticalAlign::Top,
        );
        if !message.is_empty() {
            self.painter.draw_text_wrapped(
                Rect::new(
                    text_x,
                    rect.y + padding + title_height + 3,
                    text_width as u32,
                    message_height.max(0) as u32,
                ),
                message,
                palette.text_muted,
                scale,
            );
        }

        let close = Rect::new(
            rect.right_i32() - padding - close_size,
            rect.y + padding,
            close_size as u32,
            close_size as u32,
        );
        let close_hit = self.interact(id.child(0xc105e), close, Sense::HOVER.with(Sense::CLICK));
        paint_icon(
            &mut self.painter,
            close.inset(2),
            Icon::Close,
            if close_hit.hovered {
                palette.text
            } else {
                palette.text_muted
            },
        );
        if close_hit.clicked {
            *open = false;
            response.changed = true;
            response.clicked = true;
        }

        let mut action_clicked = false;
        if let Some(label) = action {
            let button = Rect::new(
                close.x - metrics.gap - action_width,
                rect.y + (rect.height as i32 - metrics.control_height) / 2,
                action_width.max(0) as u32,
                metrics.control_height as u32,
            );
            let hit = self.button_at(button, label, false);
            if hit.clicked {
                action_clicked = true;
                response.changed = true;
            }
        }

        InfoBarResponse {
            response,
            action_clicked,
        }
    }
}
