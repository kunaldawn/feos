//! Containers: panels, cards, scroll areas, tabs, windows, dialogs, lists and
//! desktop chrome.

use feos_gfx::Rect;

use crate::geometry::{Align, Padding, RectExt, VerticalAlign};
use crate::id::Id;
use crate::response::{Response, Sense};
use crate::ui::{Ui, WindowResponse, WindowState};

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A raised surface with a border and padding.
    pub fn panel<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Self) -> R) -> R {
        self.panel_with_padding(rect, Padding::all(self.theme.metrics.padding_x), f)
    }

    /// A panel with custom padding.
    pub fn panel_with_padding<R>(
        &mut self,
        rect: Rect,
        padding: Padding,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let palette = self.theme.palette;
        let radius = self.theme.metrics.radius;
        self.painter
            .fill_rounded_rect(rect, radius, palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            radius,
            self.theme.metrics.border_width,
            palette.border,
        );
        let inner = padding.apply(rect);
        let (_, value) = self.column_with(inner, self.theme.metrics.gap, f);
        value
    }

    /// A panel with a title separated from its body.
    pub fn card<R>(&mut self, rect: Rect, title: &str, f: impl FnOnce(&mut Self) -> R) -> R {
        let padding = Padding::all(self.theme.metrics.padding_x);
        let palette = self.theme.palette;
        let radius = self.theme.metrics.radius;
        self.painter
            .fill_rounded_rect(rect, radius, palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            radius,
            self.theme.metrics.border_width,
            palette.border,
        );
        let inner = padding.apply(rect);
        let (_, value) = self.column_with(inner, self.theme.metrics.gap, |ui| {
            ui.heading(title);
            ui.separator();
            f(ui)
        });
        value
    }

    /// A vertically scrollable region.
    ///
    /// The offset is stored in [`InteractionState`](crate::InteractionState)
    /// under an automatically allocated ID, so no state parameter is needed.
    /// Children are clipped to `rect`. Returns the used content area.
    pub fn scroll_area<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Self) -> R) -> Rect {
        let id = self.next_id();
        self.scroll_area_with_id(id, rect, f)
    }

    /// A scroll area with an explicit stable ID.
    ///
    /// Use this when the scroll region can move around the widget tree and its
    /// offset must stay attached to the same logical view.
    pub fn scroll_area_with_id<R>(
        &mut self,
        id: Id,
        rect: Rect,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let mut offset = self.state.scroll_offset(id);
        let previous_content = self.state.scroll_content(id);
        let max_scroll = (previous_content - rect.height as i32).max(0);

        if self.pointer_over(rect) && self.wheel() != 0 {
            offset = (offset - self.wheel() * metrics.row_height).clamp(0, max_scroll);
        }

        let content_rect = Rect::new(
            rect.x,
            rect.y.saturating_sub(offset),
            rect.width,
            rect.height,
        );
        let old_clip = self.painter.push_clip(rect);
        let used = self.column(content_rect, metrics.gap, f);
        self.painter.restore_clip(old_clip);

        let content_height = used.height as i32;
        let max_scroll = (content_height - rect.height as i32).max(0);
        offset = offset.clamp(0, max_scroll);
        self.state.set_scroll(id, offset, content_height);

        if content_height > rect.height as i32 {
            let bar_width = metrics.scrollbar_width;
            let track = Rect::new(
                rect.right_i32() - bar_width - 2,
                rect.y + 2,
                bar_width as u32,
                rect.height.saturating_sub(4),
            );
            let bar_id = id.child(0x5c);
            let response = self.interact(bar_id, track, Sense::drag());

            let thumb_height =
                ((track.height as f32 * rect.height as f32 / content_height as f32) as i32).clamp(
                    metrics.min_control_size.min(track.height as i32),
                    track.height as i32,
                );

            if response.active && self.pointer_down() {
                let travel = (track.height as i32 - thumb_height).max(1);
                let position = (self.pointer().y - track.y - thumb_height / 2).clamp(0, travel);
                offset = (position as f32 / travel as f32 * max_scroll as f32) as i32;
                self.state.set_scroll(id, offset, content_height);
            }

            self.painter
                .fill_rounded_rect(track, (bar_width / 2) as u32, palette.scrollbar_track);
            let thumb_y = if max_scroll == 0 {
                track.y
            } else {
                track.y
                    + ((track.height as i32 - thumb_height) as f32
                        * (offset as f32 / max_scroll as f32)) as i32
            };
            let thumb = Rect::new(track.x, thumb_y, track.width, thumb_height.max(1) as u32);
            self.painter.fill_rounded_rect(
                thumb,
                (bar_width / 2) as u32,
                if response.hovered || response.active {
                    palette.scrollbar_hover
                } else {
                    palette.scrollbar_thumb
                },
            );
        }

        Rect::new(rect.x, rect.y, rect.width, used.height)
    }

    /// A horizontal tab strip. Clicking a tab sets `selected`.
    pub fn tabs(&mut self, selected: &mut usize, labels: &[&str]) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(0, metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        response.id = id;
        response.rect = rect;
        if labels.is_empty() {
            return response;
        }

        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface_alt);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );

        let segment_width = rect.width as i32 / labels.len() as i32;
        for (index, label) in labels.iter().enumerate() {
            let x = rect.x + index as i32 * segment_width;
            let width = if index + 1 == labels.len() {
                rect.right_i32() - x
            } else {
                segment_width
            };
            let tab = Rect::new(x, rect.y, width.max(0) as u32, rect.height);
            let tab_id = id.child(index as u64 + 1);
            let tab_response = self.interact(tab_id, tab, Sense::click());
            if tab_response.clicked && *selected != index {
                *selected = index;
                response.changed = true;
            }
            response.clicked |= tab_response.clicked;

            if *selected == index {
                self.painter.fill_rounded_rect(
                    tab.inset(3),
                    metrics.radius.saturating_sub(2),
                    palette.surface,
                );
                self.painter.fill_rounded_rect(
                    Rect::new(
                        tab.x + 8,
                        tab.bottom_i32() - 3,
                        (tab.width as i32 - 16).max(0) as u32,
                        3,
                    ),
                    2,
                    palette.accent,
                );
            }
            if tab_response.focused {
                self.painter
                    .stroke_rounded_rect(tab.inset(2), metrics.radius, 1, palette.focus);
            }
            self.painter.draw_text_ellipsis(
                tab,
                label,
                if *selected == index {
                    palette.text
                } else {
                    palette.text_muted
                },
                metrics.font_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
        }
        response
    }

    /// A draggable, resizable window with a title bar and close button.
    pub fn window<R>(
        &mut self,
        state: &mut WindowState,
        title: &str,
        f: impl FnOnce(&mut Self) -> R,
    ) -> WindowResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        if !state.open {
            return WindowResponse {
                response: Response::empty(Id::NONE, Rect::new(0, 0, 0, 0)),
                closed: false,
                content: Rect::new(0, 0, 0, 0),
            };
        }

        let canvas = self.canvas_rect();
        let min_visible = metrics.control_height * 3;
        state.rect.x = state.rect.x.clamp(
            -(state.rect.width as i32) + min_visible,
            canvas.right_i32() - min_visible,
        );
        state.rect.y = state
            .rect
            .y
            .clamp(0, (canvas.bottom_i32() - metrics.title_bar_height).max(0));

        let id = self.next_id();
        let title_bar = Rect::new(
            state.rect.x,
            state.rect.y,
            state.rect.width,
            metrics.title_bar_height as u32,
        );
        let close_size = (metrics.title_bar_height - 14).max(12);
        let close_rect = Rect::new(
            title_bar.right_i32() - close_size - 7,
            title_bar.y + (metrics.title_bar_height - close_size) / 2,
            close_size as u32,
            close_size as u32,
        );
        let drag_rect = Rect::new(
            title_bar.x,
            title_bar.y,
            title_bar.width.saturating_sub((close_size + 14) as u32),
            title_bar.height,
        );

        let drag = self.interact(id, drag_rect, Sense::drag());
        if drag.dragged && self.pointer_down() {
            state.rect.x = state.rect.x.saturating_add(drag.drag_delta.x);
            state.rect.y = state.rect.y.saturating_add(drag.drag_delta.y);
        }

        let close_id = id.child(0xc105e);
        let close = self.interact(close_id, close_rect, Sense::click());
        let mut closed = false;
        if close.clicked {
            state.open = false;
            closed = true;
        }

        self.painter.shadow(state.rect, metrics.radius, 8, 140);
        self.painter
            .fill_rounded_rect(state.rect, metrics.radius, palette.surface);
        self.painter.stroke_rounded_rect(
            state.rect,
            metrics.radius,
            metrics.border_width,
            palette.border_strong,
        );

        self.painter.fill_rounded_rect(
            Rect::new(
                state.rect.x,
                state.rect.y,
                state.rect.width,
                metrics.title_bar_height as u32,
            ),
            metrics.radius,
            palette.surface_alt,
        );
        self.painter.fill_rect(
            Rect::new(
                state.rect.x,
                state.rect.y + metrics.title_bar_height - 1,
                state.rect.width,
                1,
            ),
            palette.border,
        );
        self.painter.draw_text_ellipsis(
            Rect::new(
                state.rect.x + metrics.padding_x,
                state.rect.y,
                state
                    .rect
                    .width
                    .saturating_sub((metrics.padding_x * 2 + close_size + 14) as u32),
                metrics.title_bar_height as u32,
            ),
            title,
            palette.text,
            metrics.title_scale,
            Align::Start,
            VerticalAlign::Middle,
        );

        if close.hovered {
            self.painter
                .fill_rounded_rect(close_rect, 4, palette.danger);
        }
        let cross_color = if close.hovered {
            palette.accent_text
        } else {
            palette.text_muted
        };
        self.painter.line(
            close_rect.x + 4,
            close_rect.y + 4,
            close_rect.right_i32() - 4,
            close_rect.bottom_i32() - 4,
            cross_color,
        );
        self.painter.line(
            close_rect.right_i32() - 4,
            close_rect.y + 4,
            close_rect.x + 4,
            close_rect.bottom_i32() - 4,
            cross_color,
        );

        if state.minimized {
            return WindowResponse {
                response: drag,
                closed,
                content: Rect::new(state.rect.x, state.rect.y, state.rect.width, 0),
            };
        }

        let content = Rect::new(
            state.rect.x + 1,
            state.rect.y + metrics.title_bar_height,
            state.rect.width.saturating_sub(2),
            state
                .rect
                .height
                .saturating_sub(metrics.title_bar_height as u32 + 1),
        );
        let padded = Padding::all(metrics.padding_x).apply(content);
        self.column(padded, metrics.gap, f);

        let handle = Rect::new(
            state.rect.right_i32() - 14,
            state.rect.bottom_i32() - 14,
            14,
            14,
        );
        let resize = self.interact(id.child(0x5e5), handle, Sense::drag());
        if resize.dragged && self.pointer_down() {
            state.rect.width = (state.rect.width as i32 + resize.drag_delta.x)
                .max(metrics.control_height * 6) as u32;
            state.rect.height = (state.rect.height as i32 + resize.drag_delta.y)
                .max(metrics.control_height * 3) as u32;
        }
        self.painter.line(
            handle.right_i32() - 2,
            handle.y + 4,
            handle.x + 4,
            handle.bottom_i32() - 2,
            palette.border_strong,
        );
        self.painter.line(
            handle.right_i32() - 2,
            handle.y + 8,
            handle.x + 8,
            handle.bottom_i32() - 2,
            palette.border_strong,
        );

        WindowResponse {
            response: drag,
            closed,
            content,
        }
    }

    /// A modal dialog with an overlay, title and close button.
    ///
    /// Returns `true` when the dialog was closed this frame. The caller is
    /// responsible for skipping interaction with the content underneath while
    /// the dialog is open.
    pub fn dialog<R>(
        &mut self,
        rect: Rect,
        title: &str,
        open: &mut bool,
        f: impl FnOnce(&mut Self) -> R,
    ) -> bool {
        if !*open {
            return false;
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;

        self.painter
            .fill_rect_alpha(self.canvas_rect(), palette.overlay, 160);
        self.painter.shadow(rect, metrics.radius, 12, 180);
        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface);
        self.painter
            .stroke_rounded_rect(rect, metrics.radius, 2, palette.border_strong);
        let title_bar = Rect::new(rect.x, rect.y, rect.width, metrics.title_bar_height as u32);
        self.painter
            .fill_rounded_rect(title_bar, metrics.radius, palette.surface_alt);
        self.painter.fill_rect(
            Rect::new(rect.x, rect.y + metrics.title_bar_height - 1, rect.width, 1),
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
        let mut closed = false;
        if close.clicked {
            *open = false;
            closed = true;
        }

        self.painter.draw_text_ellipsis(
            Rect::new(
                rect.x + metrics.padding_x,
                rect.y,
                rect.width
                    .saturating_sub((metrics.padding_x * 2 + close_size + 14) as u32),
                metrics.title_bar_height as u32,
            ),
            title,
            palette.text,
            metrics.title_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        let cross_color = if close.hovered {
            palette.danger
        } else {
            palette.text_muted
        };
        self.painter.line(
            close_rect.x + 4,
            close_rect.y + 4,
            close_rect.right_i32() - 4,
            close_rect.bottom_i32() - 4,
            cross_color,
        );
        self.painter.line(
            close_rect.right_i32() - 4,
            close_rect.y + 4,
            close_rect.x + 4,
            close_rect.bottom_i32() - 4,
            cross_color,
        );

        let content = Rect::new(
            rect.x + 1,
            rect.y + metrics.title_bar_height,
            rect.width.saturating_sub(2),
            rect.height
                .saturating_sub(metrics.title_bar_height as u32 + 1),
        );
        let padded = Padding::all(metrics.padding_x).apply(content);
        self.column(padded, metrics.gap, f);
        closed
    }

    /// Paints a desktop wallpaper: a vertical gradient and a faint grid.
    pub fn desktop_background(&mut self, rect: Rect) {
        let palette = self.theme.palette;
        let top = palette.background.lerp(palette.accent, 6);
        let bottom = palette.background;
        let height = rect.height as i32;
        let denominator = height.saturating_sub(1).max(1) as u32;
        for row in 0..height {
            let t = ((row as u32 * 255) / denominator) as u8;
            self.painter.fill_rect(
                Rect::new(rect.x, rect.y + row, rect.width, 1),
                top.lerp(bottom, t),
            );
        }
        let step = 64;
        let color = palette.accent.lerp(palette.background, 238);
        let mut x = rect.x;
        while x < rect.right_i32() {
            self.painter
                .fill_rect(Rect::new(x, rect.y, 1, rect.height), color);
            x += step;
        }
        let mut y = rect.y;
        while y < rect.bottom_i32() {
            self.painter
                .fill_rect(Rect::new(rect.x, y, rect.width, 1), color);
            y += step;
        }
    }

    /// A floating tooltip anchored below `anchor`.
    pub fn tooltip(&mut self, anchor: Rect, text: &str) {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let scale = metrics.small_scale;
        let width = self.painter.text_width(text, scale) + metrics.tooltip_padding * 2;
        let height = self.painter.line_height(scale) + metrics.tooltip_padding * 2;
        let canvas = self.canvas_rect();
        let x = (anchor.center().x - width / 2).clamp(
            canvas.x + 2,
            (canvas.right_i32() - width - 2).max(canvas.x + 2),
        );
        let y = (anchor.bottom_i32() + 6).min((canvas.bottom_i32() - height - 2).max(canvas.y + 2));
        let rect = Rect::new(x, y, width as u32, height as u32);
        self.painter
            .fill_rounded_rect(rect, metrics.radius.min(6), palette.surface_alt);
        self.painter
            .stroke_rounded_rect(rect, metrics.radius.min(6), 1, palette.border_strong);
        self.painter
            .draw_text_centered(rect, text, palette.text, scale);
    }

    /// A simple selectable list.
    ///
    /// Rows that do not fit are clipped; wrap the call in
    /// [`scroll_area`](Ui::scroll_area) for scrolling.
    pub fn list(&mut self, rect: Rect, items: &[&str], selected: &mut usize) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let old_clip = self.painter.push_clip(rect);
        for (index, item) in items.iter().enumerate() {
            let row = Rect::new(
                rect.x,
                rect.y + index as i32 * metrics.row_height,
                rect.width,
                metrics.row_height as u32,
            );
            if row.y >= rect.bottom_i32() {
                break;
            }
            let row_id = id.child(index as u64 + 1);
            let row_response = self.interact(row_id, row, Sense::click());
            if row_response.clicked && *selected != index {
                *selected = index;
                response.changed = true;
            }
            if *selected == index {
                self.painter
                    .fill_rounded_rect(row.inset_xy(2, 1), 4, palette.surface_active);
                self.painter.fill_rect(
                    Rect::new(row.x + 2, row.y + 3, 3, (metrics.row_height - 6) as u32),
                    palette.accent,
                );
            } else if row_response.hovered {
                self.painter
                    .fill_rounded_rect(row.inset_xy(2, 1), 4, palette.surface_hover);
            }
            self.painter.draw_text_ellipsis(
                Rect::new(
                    row.x + metrics.padding_x,
                    row.y,
                    row.width.saturating_sub((metrics.padding_x * 2) as u32),
                    row.height,
                ),
                item,
                if *selected == index {
                    palette.text
                } else {
                    palette.text_muted
                },
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
        }
        self.painter.restore_clip(old_clip);
        response
    }
}
