//! Basic controls: text, buttons, checkboxes, radio buttons, toggles,
//! sliders, progress bars, badges and separators.

use feos_gfx::{Color, Rect};

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::layout::Axis;
use crate::response::{Response, Sense};
use crate::ui::Ui;

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// Draws a body-text label. Returns the rectangle it used.
    pub fn label(&mut self, text: &str) -> Rect {
        self.label_colored(text, self.theme.palette.text)
    }

    /// Draws a muted label.
    pub fn label_muted(&mut self, text: &str) -> Rect {
        self.label_colored(text, self.theme.palette.text_muted)
    }

    /// Draws a label in a specific colour.
    pub fn label_colored(&mut self, text: &str, color: Color) -> Rect {
        let scale = self.theme.metrics.font_scale;
        let width = self.painter.text_width(text, scale);
        let height = self.painter.text_height(text, scale);
        let rect = self.allocate(width, height);
        self.painter.draw_text(rect.x, rect.y, text, color, scale);
        rect
    }

    /// Draws a section heading.
    pub fn heading(&mut self, text: &str) -> Rect {
        let scale = self.theme.metrics.heading_scale;
        let width = self.painter.text_width(text, scale);
        let height = self.painter.text_height(text, scale);
        let rect = self.allocate(width, height);
        self.painter
            .draw_text(rect.x, rect.y, text, self.theme.palette.text, scale);
        rect
    }

    /// Draws hero/display text.
    pub fn display_text(&mut self, text: &str) -> Rect {
        let scale = self.theme.metrics.display_scale;
        let width = self.painter.text_width(text, scale);
        let height = self.painter.text_height(text, scale);
        let rect = self.allocate(width, height);
        self.painter
            .draw_text(rect.x, rect.y, text, self.theme.palette.text, scale);
        rect
    }

    /// Draws word-wrapped paragraph text.
    pub fn paragraph(&mut self, text: &str) -> Rect {
        let scale = self.theme.metrics.font_scale;
        let color = self.theme.palette.text_muted;
        let (available, _) = self.remaining();
        let width = if self.axis() == Axis::Vertical {
            available.max(1)
        } else {
            self.painter.text_width(text, scale).max(1)
        };
        let height = self.painter.wrapped_text_height(width, text, scale);
        let rect = self.allocate(width, height);
        self.painter.draw_text_wrapped(rect, text, color, scale);
        rect
    }

    /// A standard button. The click state is in the returned response.
    pub fn button(&mut self, text: &str) -> Response {
        let width = self.intrinsic_button_width(text);
        let rect = self.allocate(width, self.theme.metrics.control_height);
        self.button_at(rect, text, false)
    }

    /// A button with an explicit width.
    pub fn button_sized(&mut self, width: i32, text: &str) -> Response {
        let rect = self.allocate_sized(width, self.theme.metrics.control_height);
        self.button_at(rect, text, false)
    }

    /// A filled accent button for the primary action.
    pub fn primary_button(&mut self, text: &str) -> Response {
        let width = self.intrinsic_button_width(text);
        let rect = self.allocate(width, self.theme.metrics.control_height);
        self.button_at(rect, text, true)
    }

    /// A filled accent button with an explicit width.
    pub fn primary_button_sized(&mut self, width: i32, text: &str) -> Response {
        let rect = self.allocate_sized(width, self.theme.metrics.control_height);
        self.button_at(rect, text, true)
    }

    /// Draws a button into a caller-provided rectangle.
    pub fn button_at(&mut self, rect: Rect, text: &str, primary: bool) -> Response {
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        let palette = self.theme.palette;
        let metrics = self.theme.metrics;
        let radius = metrics.radius;

        let (mut fill, text_color) = if primary {
            (palette.accent, palette.accent_text)
        } else {
            (palette.surface_alt, palette.text)
        };
        if response.pressed {
            fill = if primary {
                palette.accent.lerp(Color::BLACK, 50)
            } else {
                palette.surface_active
            };
        } else if response.hovered {
            fill = if primary {
                palette.accent_hover
            } else {
                palette.surface_hover
            };
        }

        self.painter.fill_rounded_rect(rect, radius, fill);
        if primary {
            self.painter.stroke_rounded_rect(
                rect,
                radius,
                metrics.border_width,
                fill.lerp(Color::WHITE, 24),
            );
        } else {
            self.painter
                .stroke_rounded_rect(rect, radius, metrics.border_width, palette.border);
        }
        if response.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), radius, 2, palette.focus);
        }

        self.painter.draw_text_ellipsis(
            rect,
            text,
            text_color,
            metrics.font_scale,
            Align::Center,
            VerticalAlign::Middle,
        );
        response.changed = false;
        response
    }

    fn intrinsic_button_width(&self, text: &str) -> i32 {
        self.painter
            .text_width(text, self.theme.metrics.font_scale)
            .saturating_add(self.theme.metrics.padding_x * 2)
            .max(self.theme.metrics.min_control_size)
    }

    /// A checkbox bound to `value`.
    pub fn checkbox(&mut self, value: &mut bool, text: &str) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let scale = metrics.font_scale;
        let box_size = (metrics.control_height - 10).clamp(12, 28);
        let width = box_size
            .saturating_add(metrics.gap)
            .saturating_add(self.painter.text_width(text, scale));
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        if response.clicked {
            *value = !*value;
            response.changed = true;
        }

        let box_rect = Rect::new(
            rect.x,
            rect.y + (rect.height as i32 - box_size) / 2,
            box_size as u32,
            box_size as u32,
        );
        let fill = if *value {
            palette.accent
        } else if response.hovered {
            palette.surface_hover
        } else {
            palette.surface_alt
        };
        self.painter
            .fill_rounded_rect(box_rect, metrics.radius.min(5), fill);
        self.painter.stroke_rounded_rect(
            box_rect,
            metrics.radius.min(5),
            metrics.border_width,
            if *value {
                palette.accent
            } else {
                palette.border
            },
        );
        if *value {
            let s = box_size;
            self.painter.line(
                box_rect.x + s / 4,
                box_rect.y + s / 2,
                box_rect.x + s / 2,
                box_rect.y + 3 * s / 4,
                palette.accent_text,
            );
            self.painter.line(
                box_rect.x + s / 2,
                box_rect.y + 3 * s / 4,
                box_rect.x + 3 * s / 4,
                box_rect.y + s / 4,
                palette.accent_text,
            );
        }
        if response.focused {
            self.painter.stroke_rounded_rect(
                box_rect.inset(-2),
                metrics.radius.min(5) + 2,
                1,
                palette.focus,
            );
        }

        self.painter.draw_text_aligned(
            Rect::new(
                box_rect.x + box_size + metrics.gap,
                rect.y,
                (rect.right_i32() - box_rect.x - box_size - metrics.gap).max(0) as u32,
                rect.height,
            ),
            text,
            palette.text,
            scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        response
    }

    /// A radio button. Clicking sets `selected` to `index`.
    pub fn radio(&mut self, selected: &mut usize, index: usize, text: &str) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let scale = metrics.font_scale;
        let box_size = (metrics.control_height - 12).clamp(12, 24);
        let width = box_size
            .saturating_add(metrics.gap)
            .saturating_add(self.painter.text_width(text, scale));
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        if response.clicked && *selected != index {
            *selected = index;
            response.changed = true;
        }

        let cx = rect.x + box_size / 2;
        let cy = rect.y + rect.height as i32 / 2;
        let radius = (box_size / 2) as u32;
        self.painter.circle(cx, cy, radius, palette.border_strong);
        if *selected == index {
            self.painter
                .fill_circle(cx, cy, radius.saturating_sub(3), palette.accent);
        }
        if response.hovered && *selected != index {
            self.painter
                .fill_circle(cx, cy, radius.saturating_sub(3), palette.surface_hover);
        }
        if response.focused {
            self.painter.circle(cx, cy, radius + 2, palette.focus);
        }

        self.painter.draw_text_aligned(
            Rect::new(
                rect.x + box_size + metrics.gap,
                rect.y,
                (rect.right_i32() - rect.x - box_size - metrics.gap).max(0) as u32,
                rect.height,
            ),
            text,
            palette.text,
            scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        response
    }

    /// A pill-shaped on/off switch.
    pub fn toggle(&mut self, value: &mut bool, text: &str) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let scale = metrics.font_scale;
        let track_height = (metrics.control_height - 10).clamp(14, 24);
        let track_width = track_height * 2;
        let width = track_width
            .saturating_add(metrics.gap)
            .saturating_add(self.painter.text_width(text, scale));
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        if response.clicked {
            *value = !*value;
            response.changed = true;
        }

        let track = Rect::new(
            rect.x,
            rect.y + (rect.height as i32 - track_height) / 2,
            track_width as u32,
            track_height as u32,
        );
        let fill = if *value {
            palette.accent
        } else if response.hovered {
            palette.surface_hover
        } else {
            palette.surface_alt
        };
        self.painter
            .fill_rounded_rect(track, (track_height / 2) as u32, fill);
        self.painter.stroke_rounded_rect(
            track,
            (track_height / 2) as u32,
            metrics.border_width,
            if *value {
                palette.accent
            } else {
                palette.border
            },
        );
        let knob_radius = (track_height / 2 - 3).max(3) as u32;
        let knob_x = if *value {
            track.right_i32() - knob_radius as i32 - 3
        } else {
            track.x + knob_radius as i32 + 3
        };
        self.painter.fill_circle(
            knob_x,
            track.y + track_height / 2,
            knob_radius,
            if *value {
                palette.accent_text
            } else {
                palette.text_muted
            },
        );
        if response.focused {
            self.painter.stroke_rounded_rect(
                track.inset(-2),
                (track_height / 2 + 2) as u32,
                1,
                palette.focus,
            );
        }

        self.painter.draw_text_aligned(
            Rect::new(
                track.right_i32() + metrics.gap,
                rect.y,
                (rect.right_i32() - track.right_i32() - metrics.gap).max(0) as u32,
                rect.height,
            ),
            text,
            palette.text,
            scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        response
    }

    /// A horizontal slider bound to `value`.
    ///
    /// The track occupies the full cell in a column and a 220 px cell in a
    /// row. Dragging and arrow keys both update the value.
    pub fn slider(&mut self, value: &mut f32, min: f32, max: f32) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(220, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::drag());
        let (low, high) = if min <= max { (min, max) } else { (max, min) };
        let range = (high - low).max(f32::EPSILON);

        if response.focused {
            let step = if self.shift() {
                range / 200.0
            } else {
                range / 20.0
            };
            if self.key_pressed(crate::input::Key::Left) {
                *value = (*value - step).max(low);
                response.changed = true;
            }
            if self.key_pressed(crate::input::Key::Right) {
                *value = (*value + step).min(high);
                response.changed = true;
            }
            if self.key_pressed(crate::input::Key::Home) {
                *value = low;
                response.changed = true;
            }
            if self.key_pressed(crate::input::Key::End) {
                *value = high;
                response.changed = true;
            }
        }

        let knob_radius = (metrics.control_height / 2 - 6).clamp(5, 10);
        let track = Rect::new(
            rect.x + knob_radius,
            rect.y + rect.height as i32 / 2 - 2,
            rect.width.saturating_sub((knob_radius * 2) as u32),
            4,
        );
        let effective = value.clamp(low, high);
        if response.active && self.pointer_down() {
            let fraction = if track.width == 0 {
                0.0
            } else {
                ((self.pointer().x - track.x) as f32 / track.width as f32).clamp(0.0, 1.0)
            };
            let new_value = low + fraction * range;
            if new_value != *value {
                *value = new_value;
                response.changed = true;
            }
        }
        let value_now = if effective.is_finite() {
            value.clamp(low, high)
        } else {
            low
        };
        let fraction = if high > low {
            (value_now - low) / (high - low)
        } else {
            0.0
        };

        self.painter
            .fill_rounded_rect(track, 2, palette.surface_active);
        let filled = (track.width as f32 * fraction) as u32;
        if filled > 0 {
            self.painter.fill_rounded_rect(
                Rect::new(track.x, track.y, filled.min(track.width), track.height),
                2,
                palette.accent,
            );
        }
        let knob_x = track.x + (track.width as f32 * fraction) as i32;
        self.painter
            .fill_circle(knob_x, track.y + 2, knob_radius as u32, palette.text);
        self.painter
            .circle(knob_x, track.y + 2, knob_radius as u32, palette.accent);
        if response.focused {
            self.painter
                .circle(knob_x, track.y + 2, knob_radius as u32 + 2, palette.focus);
        }
        response
    }

    /// A progress bar. `value` is clamped to 0..=1.
    pub fn progress(&mut self, value: f32) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let height = (metrics.control_height / 3).clamp(6, 14);
        let rect = self.allocate(0, height);
        let radius = (height / 2) as u32;
        self.painter
            .fill_rounded_rect(rect, radius, palette.surface_active);
        let value = value.clamp(0.0, 1.0);
        let width = (rect.width as f32 * value) as u32;
        if width > 0 {
            self.painter.fill_rounded_rect(
                Rect::new(rect.x, rect.y, width, rect.height),
                radius,
                palette.accent,
            );
        }
        rect
    }

    /// A small coloured pill with a label.
    pub fn badge(&mut self, text: &str, color: Color) -> Rect {
        let metrics = self.theme.metrics;
        let scale = metrics.small_scale;
        let width = self
            .painter
            .text_width(text, scale)
            .saturating_add(metrics.padding_x);
        let rect = self.allocate(width, metrics.row_height);
        let pill = Rect::new(
            rect.x,
            rect.y + (rect.height as i32 - metrics.row_height) / 2,
            width.min(rect.width as i32) as u32,
            metrics.row_height as u32,
        );
        self.painter
            .fill_rounded_rect(pill, (metrics.row_height / 2) as u32, color);
        self.painter
            .draw_text_centered(pill, text, self.theme.palette.accent_text, scale);
        pill
    }

    /// A horizontal or vertical rule, depending on the layout axis.
    pub fn separator(&mut self) -> Rect {
        let color = self.theme.palette.border;
        if self.axis() == Axis::Vertical {
            let rect = self.allocate(0, 1);
            self.painter.fill_rect(rect, color);
            rect
        } else {
            let rect = self.allocate(1, 0);
            self.painter
                .fill_rect(Rect::new(rect.x, rect.y, 1, rect.height), color);
            rect
        }
    }

    /// Adds empty space along the current layout axis.
    pub fn space(&mut self, amount: i32) -> Rect {
        if self.axis() == Axis::Vertical {
            self.allocate(0, amount)
        } else {
            self.allocate(amount, 0)
        }
    }
}
