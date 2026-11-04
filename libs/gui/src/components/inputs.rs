//! Form controls: search fields, selects, menus, segmented controls, numeric
//! steppers, ratings and colour pickers.

use core::fmt::Write as _;

use feos_gfx::{Color, Rect};

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::input::Key;
use crate::layout::Axis;
use crate::response::{Response, Sense};
use crate::text::{format_f32, FixedBuf, TextBuffer};
use crate::ui::Ui;

use super::icons::{paint_icon, Icon};

/// Persistent state for [`Ui::select`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectState {
    /// Whether the popup list is open.
    pub open: bool,
    /// Highlighted row for keyboard navigation.
    pub highlighted: usize,
}

impl SelectState {
    pub const fn new() -> Self {
        Self {
            open: false,
            highlighted: 0,
        }
    }
}

/// Persistent state for [`Ui::menu_button`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MenuState {
    /// Whether the menu is open.
    pub open: bool,
    /// Highlighted entry, indexed into the entry slice.
    pub highlighted: usize,
}

impl MenuState {
    pub const fn new() -> Self {
        Self {
            open: false,
            highlighted: 0,
        }
    }
}

/// One entry in a [`Ui::menu_button`] menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEntry<'a> {
    /// A selectable command.
    Item {
        label: &'a str,
        shortcut: &'a str,
        enabled: bool,
    },
    /// A horizontal divider.
    Separator,
}

impl<'a> MenuEntry<'a> {
    pub const fn item(label: &'a str) -> Self {
        Self::Item {
            label,
            shortcut: "",
            enabled: true,
        }
    }

    pub const fn shortcut(label: &'a str, shortcut: &'a str) -> Self {
        Self::Item {
            label,
            shortcut,
            enabled: true,
        }
    }

    pub const fn disabled(label: &'a str) -> Self {
        Self::Item {
            label,
            shortcut: "",
            enabled: false,
        }
    }

    pub const fn is_separator(self) -> bool {
        matches!(self, Self::Separator)
    }

    pub const fn enabled(self) -> bool {
        match self {
            Self::Item { enabled, .. } => enabled,
            Self::Separator => false,
        }
    }
}

/// The result of [`Ui::menu_button`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuResponse {
    /// Interaction with the menu button itself.
    pub response: Response,
    /// Index of the chosen entry, if the user activated one this frame.
    pub selected: Option<usize>,
    /// The menu was dismissed by Escape or by a click outside.
    pub dismissed: bool,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A single-line search field with a leading magnifier and a clear button.
    pub fn search_box<const M: usize>(
        &mut self,
        buffer: &mut TextBuffer<M>,
        placeholder: &str,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(240, metrics.control_height);
        let icon_size = (metrics.control_height - 8).max(12);
        let left_inset = icon_size + metrics.padding_x / 2;
        let clear_size = (metrics.control_height - 14).max(10);
        let right_inset = if buffer.is_empty() {
            0
        } else {
            clear_size + metrics.padding_x / 2
        };
        let mut response = self.text_input_at(buffer, rect, placeholder, left_inset, right_inset);

        let icon_rect = Rect::new(
            rect.x + metrics.padding_x / 2,
            rect.y + (rect.height as i32 - icon_size) / 2,
            icon_size as u32,
            icon_size as u32,
        );
        paint_icon(
            &mut self.painter,
            icon_rect,
            Icon::Search,
            palette.text_muted,
        );

        if !buffer.is_empty() {
            let clear_rect = Rect::new(
                rect.right_i32() - clear_size - metrics.padding_x / 2,
                rect.y + (rect.height as i32 - clear_size) / 2,
                clear_size as u32,
                clear_size as u32,
            );
            let clear = self.interact(
                response.id.child(0xc1ea),
                clear_rect,
                Sense::HOVER.with(Sense::CLICK),
            );
            if clear.hovered || clear.active {
                self.painter.fill_rounded_rect(
                    clear_rect,
                    (clear_size / 2) as u32,
                    palette.surface_hover,
                );
            }
            let glyph = clear_rect.inset(3);
            paint_icon(&mut self.painter, glyph, Icon::Close, palette.text_muted);
            if clear.clicked {
                buffer.clear();
                response.changed = true;
                response.clicked = true;
            }
        }
        response
    }

    /// A dropdown select bound to `selected`.
    ///
    /// Draw this after the content it may overlap; the popup is painted
    /// immediately rather than deferred to a hidden overlay pass.
    pub fn select(
        &mut self,
        state: &mut SelectState,
        selected: &mut usize,
        items: &[&str],
        placeholder: &str,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(180, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        let mut activated = false;

        if response.focused && state.open {
            if self.key_pressed(Key::Up) {
                state.highlighted = state.highlighted.saturating_sub(1);
            }
            if self.key_pressed(Key::Down) {
                state.highlighted = (state.highlighted + 1).min(items.len().saturating_sub(1));
            }
            if self.enter_pressed() && !items.is_empty() {
                let index = state.highlighted.min(items.len() - 1);
                if *selected != index {
                    *selected = index;
                }
                state.open = false;
                response.clicked = true;
                response.changed = true;
                activated = true;
            } else if self.escape_pressed() {
                state.open = false;
            }
        }

        if state.open && self.escape_pressed() {
            state.open = false;
        }

        if response.clicked && !activated {
            if state.open {
                state.open = false;
            } else {
                state.open = true;
                state.highlighted = (*selected).min(items.len().saturating_sub(1));
            }
        }

        let row_height = metrics.row_height;
        let popup_height = (items.len() as i32 * row_height + 4).max(4);
        let canvas = self.canvas_rect();
        let popup_width = items
            .iter()
            .map(|item| self.painter.text_width(item, metrics.font_scale))
            .max()
            .unwrap_or(0)
            .saturating_add(metrics.padding_x * 2)
            .max(rect.width as i32);
        let below = rect.bottom_i32() + 2;
        let popup_y = if below + popup_height <= canvas.bottom_i32() {
            below
        } else {
            (rect.y - popup_height - 2).max(canvas.y)
        };
        let popup_x = rect
            .x
            .clamp(canvas.x, (canvas.right_i32() - popup_width).max(canvas.x));
        let popup = Rect::new(
            popup_x,
            popup_y,
            popup_width.max(0) as u32,
            popup_height.max(0) as u32,
        );
        if state.open {
            self.float(popup);
        }

        if state.open
            && self.pointer_pressed()
            && !self.pointer_over(rect)
            && !self.pointer_over(popup)
        {
            state.open = false;
        }

        let fill = if response.focused {
            palette.surface
        } else if response.hovered {
            palette.surface_hover
        } else {
            palette.surface_alt
        };
        self.painter.fill_rounded_rect(rect, metrics.radius, fill);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            if response.focused {
                2
            } else {
                metrics.border_width
            },
            if response.focused {
                palette.focus
            } else {
                palette.border
            },
        );
        let label = items.get(*selected).copied().unwrap_or(placeholder);
        let text_color = if items.is_empty() {
            palette.text_disabled
        } else {
            palette.text
        };
        self.painter.draw_text_ellipsis(
            Rect::new(
                rect.x + metrics.padding_x,
                rect.y,
                rect.width
                    .saturating_sub((metrics.padding_x * 2 + 20) as u32),
                rect.height,
            ),
            label,
            text_color,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        let chevron = Rect::new(
            rect.right_i32() - 22,
            rect.y + (rect.height as i32 - 16) / 2,
            16,
            16,
        );
        paint_icon(
            &mut self.painter,
            chevron,
            if state.open {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            },
            palette.text_muted,
        );

        if state.open {
            self.painter.shadow(popup, metrics.radius.min(8), 8, 140);
            self.painter
                .fill_rounded_rect(popup, metrics.radius.min(8), palette.surface);
            self.painter.stroke_rounded_rect(
                popup,
                metrics.radius.min(8),
                metrics.border_width,
                palette.border_strong,
            );
            let old_clip = self.painter.push_clip(popup.inset(1));
            for (index, item) in items.iter().enumerate() {
                let row = Rect::new(
                    popup.x + 2,
                    popup.y + 2 + index as i32 * row_height,
                    popup.width.saturating_sub(4),
                    row_height as u32,
                );
                if row.y >= popup.bottom_i32() {
                    break;
                }
                let row_response = self.interact(
                    id.child(index as u64 + 1),
                    row,
                    Sense::HOVER.with(Sense::CLICK),
                );
                if row_response.hovered {
                    state.highlighted = index;
                }
                if *selected == index {
                    self.painter
                        .fill_rounded_rect(row.inset_xy(2, 1), 4, palette.surface_active);
                } else if row_response.hovered || state.highlighted == index {
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
                    palette.text,
                    metrics.font_scale,
                    Align::Start,
                    VerticalAlign::Middle,
                );
                if row_response.clicked {
                    if *selected != index {
                        *selected = index;
                    }
                    state.open = false;
                    response.clicked = true;
                    response.changed = true;
                }
            }
            self.painter.restore_clip(old_clip);
        }
        response
    }

    /// Alias for [`select`](Ui::select) for apps that use the dropdown name.
    pub fn dropdown(
        &mut self,
        state: &mut SelectState,
        selected: &mut usize,
        items: &[&str],
        placeholder: &str,
    ) -> Response {
        self.select(state, selected, items, placeholder)
    }

    /// A button that opens a menu of commands.
    ///
    /// Like [`select`](Ui::select), draw this late in the frame so the popup
    /// paints over content below it.
    pub fn menu_button(
        &mut self,
        state: &mut MenuState,
        label: &str,
        entries: &[MenuEntry<'_>],
    ) -> MenuResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = self
            .painter
            .text_width(label, metrics.font_scale)
            .saturating_add(metrics.padding_x * 2 + 22)
            .max(metrics.min_control_size);
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        let mut selected = None;
        let mut dismissed = false;

        if response.focused && state.open {
            if self.key_pressed(Key::Up) {
                state.highlighted = previous_enabled(entries, state.highlighted);
            }
            if self.key_pressed(Key::Down) {
                state.highlighted = next_enabled(entries, state.highlighted);
            }
            if self.enter_pressed() && entries.get(state.highlighted).is_some_and(|e| e.enabled()) {
                selected = Some(state.highlighted);
                state.open = false;
                response.clicked = true;
            }
        }
        if state.open && self.escape_pressed() {
            state.open = false;
            dismissed = true;
        }

        if response.clicked && selected.is_none() {
            state.open = !state.open;
            if state.open {
                state.highlighted = next_enabled(entries, usize::MAX);
            }
        }

        let item_height = metrics.row_height + 6;
        let popup_width = entries
            .iter()
            .map(|entry| match entry {
                MenuEntry::Item {
                    label, shortcut, ..
                } => self
                    .painter
                    .text_width(label, metrics.font_scale)
                    .saturating_add(if shortcut.is_empty() {
                        0
                    } else {
                        self.painter
                            .text_width(shortcut, metrics.small_scale)
                            .saturating_add(metrics.padding_x * 2)
                    }),
                MenuEntry::Separator => 0,
            })
            .max()
            .unwrap_or(0)
            .saturating_add(metrics.padding_x * 2)
            .max(160)
            .max(rect.width as i32);
        let popup_height: i32 = entries
            .iter()
            .map(|entry| match entry {
                MenuEntry::Item { .. } => item_height,
                MenuEntry::Separator => 7,
            })
            .sum::<i32>()
            .max(4)
            + 4;
        let canvas = self.canvas_rect();
        let below = rect.bottom_i32() + 2;
        let popup_y = if below + popup_height <= canvas.bottom_i32() {
            below
        } else {
            (rect.y - popup_height - 2).max(canvas.y)
        };
        let popup_x = rect
            .x
            .clamp(canvas.x, (canvas.right_i32() - popup_width).max(canvas.x));
        let popup = Rect::new(
            popup_x,
            popup_y,
            popup_width.max(0) as u32,
            popup_height.max(0) as u32,
        );
        if state.open {
            self.float(popup);
        }
        if state.open
            && self.pointer_pressed()
            && !self.pointer_over(rect)
            && !self.pointer_over(popup)
        {
            state.open = false;
            dismissed = true;
        }

        let fill = if response.hovered || state.open {
            palette.surface_hover
        } else {
            palette.surface_alt
        };
        self.painter.fill_rounded_rect(rect, metrics.radius, fill);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            if response.focused || state.open {
                palette.border_strong
            } else {
                palette.border
            },
        );
        if response.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), metrics.radius, 2, palette.focus);
        }
        self.painter.draw_text_ellipsis(
            Rect::new(
                rect.x + metrics.padding_x,
                rect.y,
                rect.width
                    .saturating_sub((metrics.padding_x * 2 + 18) as u32),
                rect.height,
            ),
            label,
            palette.text,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        paint_icon(
            &mut self.painter,
            Rect::new(
                rect.right_i32() - 22,
                rect.y + (rect.height as i32 - 16) / 2,
                16,
                16,
            ),
            if state.open {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            },
            palette.text_muted,
        );

        if state.open {
            self.painter.shadow(popup, 6, 8, 140);
            self.painter.fill_rounded_rect(popup, 6, palette.surface);
            self.painter
                .stroke_rounded_rect(popup, 6, metrics.border_width, palette.border_strong);
            let old_clip = self.painter.push_clip(popup.inset(1));
            let mut y = popup.y + 2;
            for (index, entry) in entries.iter().enumerate() {
                match *entry {
                    MenuEntry::Separator => {
                        self.painter.fill_rect(
                            Rect::new(
                                popup.x + metrics.padding_x,
                                y + 3,
                                popup.width.saturating_sub((metrics.padding_x * 2) as u32),
                                1,
                            ),
                            palette.border,
                        );
                        y += 7;
                    }
                    MenuEntry::Item {
                        label,
                        shortcut,
                        enabled,
                    } => {
                        let row = Rect::new(
                            popup.x + 2,
                            y,
                            popup.width.saturating_sub(4),
                            item_height as u32,
                        );
                        let row_response = if enabled {
                            Some(self.interact(
                                id.child(index as u64 + 1),
                                row,
                                Sense::HOVER.with(Sense::CLICK),
                            ))
                        } else {
                            None
                        };
                        if let Some(row_response) = row_response {
                            if row_response.hovered {
                                state.highlighted = index;
                            }
                            if row_response.clicked {
                                selected = Some(index);
                                state.open = false;
                            }
                        }
                        if selected == Some(index) {
                            self.painter.fill_rounded_rect(
                                row.inset_xy(2, 1),
                                4,
                                palette.surface_active,
                            );
                        } else if enabled && state.highlighted == index {
                            self.painter.fill_rounded_rect(
                                row.inset_xy(2, 1),
                                4,
                                palette.surface_hover,
                            );
                        }
                        let color = if enabled {
                            palette.text
                        } else {
                            palette.text_disabled
                        };
                        self.painter.draw_text_ellipsis(
                            Rect::new(
                                row.x + metrics.padding_x,
                                row.y,
                                row.width.saturating_sub((metrics.padding_x * 2) as u32),
                                row.height,
                            ),
                            label,
                            color,
                            metrics.font_scale,
                            Align::Start,
                            VerticalAlign::Middle,
                        );
                        if !shortcut.is_empty() {
                            self.painter.draw_text_ellipsis(
                                Rect::new(
                                    row.right_i32()
                                        - metrics.padding_x
                                        - self.painter.text_width(shortcut, metrics.small_scale),
                                    row.y,
                                    self.painter.text_width(shortcut, metrics.small_scale) as u32,
                                    row.height,
                                ),
                                shortcut,
                                palette.text_muted,
                                metrics.small_scale,
                                Align::End,
                                VerticalAlign::Middle,
                            );
                        }
                        y += item_height;
                    }
                }
            }
            self.painter.restore_clip(old_clip);
        }

        MenuResponse {
            response,
            selected,
            dismissed,
        }
    }

    /// A segmented control. Clicking a segment sets `selected`.
    pub fn segmented(&mut self, selected: &mut usize, labels: &[&str]) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(240, metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
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
            let segment = Rect::new(x, rect.y, width.max(0) as u32, rect.height);
            let segment_response =
                self.interact(id.child(index as u64 + 1), segment, Sense::click());
            if segment_response.clicked && *selected != index {
                *selected = index;
                response.changed = true;
                response.clicked = true;
            }
            if *selected == index {
                self.painter.fill_rounded_rect(
                    segment.inset(3),
                    metrics.radius.saturating_sub(2),
                    palette.accent,
                );
            } else if segment_response.hovered {
                self.painter.fill_rounded_rect(
                    segment.inset(3),
                    metrics.radius.saturating_sub(2),
                    palette.surface_hover,
                );
            }
            if segment_response.focused {
                self.painter.stroke_rounded_rect(
                    segment.inset(2),
                    metrics.radius,
                    1,
                    palette.focus,
                );
            }
            self.painter.draw_text_ellipsis(
                segment,
                label,
                if *selected == index {
                    palette.accent_text
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

    /// A numeric stepper with `-` and `+` buttons.
    pub fn number_stepper(
        &mut self,
        value: &mut f32,
        min: f32,
        max: f32,
        step: f32,
        decimals: usize,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(160, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        response.changed = false;
        let (low, high) = if min <= max { (min, max) } else { (max, min) };
        let step = if step.is_finite() { step.abs() } else { 1.0 };
        let mut current = if value.is_finite() {
            value.clamp(low, high)
        } else {
            low
        };
        let mut changed = false;

        let button_size = (metrics.control_height - 6).max(12);
        let minus_rect = Rect::new(
            rect.x + 3,
            rect.y + (rect.height as i32 - button_size) / 2,
            button_size as u32,
            button_size as u32,
        );
        let plus_rect = Rect::new(
            rect.right_i32() - button_size - 3,
            minus_rect.y,
            button_size as u32,
            button_size as u32,
        );
        let minus = self.interact(id.child(1), minus_rect, Sense::click());
        let plus = self.interact(id.child(2), plus_rect, Sense::click());
        if minus.clicked {
            current = (current - step).max(low);
            changed = true;
        }
        if plus.clicked {
            current = (current + step).min(high);
            changed = true;
        }

        let focused =
            self.focus() == id || self.focus() == id.child(1) || self.focus() == id.child(2);
        response.focused = focused;
        if focused {
            let delta = if self.shift() { step * 10.0 } else { step };
            if self.key_pressed(Key::Left) || self.key_pressed(Key::Down) {
                current = (current - delta).max(low);
                changed = true;
            }
            if self.key_pressed(Key::Right) || self.key_pressed(Key::Up) {
                current = (current + delta).min(high);
                changed = true;
            }
            if self.key_pressed(Key::Home) {
                current = low;
                changed = true;
            }
            if self.key_pressed(Key::End) {
                current = high;
                changed = true;
            }
        }

        if changed && current != *value {
            *value = current;
            response.changed = true;
        }

        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface_alt);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            if focused { 2 } else { metrics.border_width },
            if focused {
                palette.focus
            } else {
                palette.border
            },
        );
        for (button, rect, icon) in [
            (minus, minus_rect, Icon::Minus),
            (plus, plus_rect, Icon::Plus),
        ] {
            if button.hovered || button.active {
                self.painter
                    .fill_rounded_rect(rect, metrics.radius.min(5), palette.surface_hover);
            }
            paint_icon(
                &mut self.painter,
                rect.inset(4),
                icon,
                if button.hovered {
                    palette.text
                } else {
                    palette.text_muted
                },
            );
        }
        let mut formatted = [0u8; 24];
        let text = format_f32(current, decimals, &mut formatted);
        self.painter.draw_text_ellipsis(
            Rect::new(
                minus_rect.right_i32() + metrics.gap,
                rect.y,
                (plus_rect.x - minus_rect.right_i32() - metrics.gap * 2).max(0) as u32,
                rect.height,
            ),
            text,
            palette.text,
            metrics.font_scale,
            Align::Center,
            VerticalAlign::Middle,
        );
        response
    }

    /// A row of star-shaped rating buttons.
    pub fn rating(&mut self, value: &mut u8, max: u8) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let max = max.clamp(1, 10);
        let size = (metrics.control_height - 6).max(12);
        let gap = 2;
        let width = max as i32 * (size + gap);
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        for index in 0..max {
            let star = Rect::new(
                rect.x + index as i32 * (size + gap),
                rect.y + (rect.height as i32 - size) / 2,
                size as u32,
                size as u32,
            );
            let star_response = self.interact(
                id.child(index as u64 + 1),
                star,
                Sense::HOVER.with(Sense::CLICK).with(Sense::FOCUSABLE),
            );
            if star_response.clicked {
                let next = index + 1;
                *value = if *value == next { index } else { next };
                response.changed = true;
                response.clicked = true;
            }
            if star_response.focused {
                self.painter
                    .stroke_rounded_rect(star.inset(-1), 4, 1, palette.focus);
            }
            let filled = index < *value;
            paint_icon(
                &mut self.painter,
                star.inset(2),
                Icon::Star,
                if filled {
                    palette.warning
                } else if star_response.hovered {
                    palette.text_muted
                } else {
                    palette.text_disabled
                },
            );
        }
        response
    }

    /// A row of circular colour swatches.
    pub fn color_picker(&mut self, selected: &mut usize, colors: &[Color]) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let size = (metrics.control_height - 8).max(10);
        let gap = 6;
        let width = colors.len() as i32 * (size + gap);
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        for (index, color) in colors.iter().enumerate() {
            let swatch = Rect::new(
                rect.x + index as i32 * (size + gap),
                rect.y + (rect.height as i32 - size) / 2,
                size as u32,
                size as u32,
            );
            let swatch_response = self.interact(id.child(index as u64 + 1), swatch, Sense::click());
            if swatch_response.clicked && *selected != index {
                *selected = index;
                response.changed = true;
                response.clicked = true;
            }
            self.painter.fill_circle(
                swatch.center().x,
                swatch.center().y,
                (size / 2) as u32,
                *color,
            );
            if *selected == index {
                self.painter.circle(
                    swatch.center().x,
                    swatch.center().y,
                    (size / 2 + 2) as u32,
                    palette.focus,
                );
            } else if swatch_response.hovered {
                self.painter.circle(
                    swatch.center().x,
                    swatch.center().y,
                    (size / 2 + 1) as u32,
                    palette.border_strong,
                );
            }
        }
        response
    }

    /// A labelled form field with an optional hint below the control.
    ///
    /// The closure runs in a fresh ID scope, so repeated fields with the same
    /// label in different forms keep distinct widget IDs.
    pub fn form_field<R>(&mut self, label: &str, hint: &str, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_id(label, |ui| {
            ui.label_muted(label);
            let value = f(ui);
            if !hint.is_empty() {
                ui.label_muted(hint);
            }
            value
        })
    }
}

fn next_enabled(entries: &[MenuEntry<'_>], from: usize) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let start = if from == usize::MAX {
        0
    } else {
        (from + 1).min(entries.len())
    };
    for offset in 0..entries.len() {
        let index = (start + offset) % entries.len();
        if entries[index].enabled() {
            return index;
        }
    }
    from.min(entries.len().saturating_sub(1))
}

fn previous_enabled(entries: &[MenuEntry<'_>], from: usize) -> usize {
    if entries.is_empty() {
        return 0;
    }
    for offset in 1..=entries.len() {
        let index = (from + entries.len() - offset) % entries.len();
        if entries[index].enabled() {
            return index;
        }
    }
    from.min(entries.len().saturating_sub(1))
}

/// The result of [`Ui::split_button`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitButtonResponse {
    /// Interaction with the main action half.
    pub response: Response,
    /// Index of the chosen menu entry, if any.
    pub selected: Option<usize>,
    /// The menu was dismissed by Escape or a click outside.
    pub dismissed: bool,
}

/// Persistent state for [`Ui::auto_suggest`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoSuggestState {
    /// Whether the suggestion list is visible.
    pub open: bool,
    /// Highlighted suggestion row.
    pub highlighted: usize,
}

impl AutoSuggestState {
    pub const fn new() -> Self {
        Self {
            open: false,
            highlighted: 0,
        }
    }
}

/// The result of [`Ui::auto_suggest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoSuggestResponse {
    /// Interaction with the text field.
    pub response: Response,
    /// Index into the suggestion slice that was chosen this frame.
    pub selected: Option<usize>,
}

/// A calendar date and the picker's open state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalendarState {
    /// Four-digit year.
    pub year: i32,
    /// Month, from 1 to 12.
    pub month: u32,
    /// Day of month, from 1 to 31.
    pub day: u32,
    /// Whether the date picker popup is open.
    pub open: bool,
}

impl CalendarState {
    /// A new date. Out-of-range fields are clamped.
    pub const fn new(year: i32, month: u32, day: u32) -> Self {
        let month = if month < 1 {
            1
        } else if month > 12 {
            12
        } else {
            month
        };
        let day = if day < 1 {
            1
        } else if day > 31 {
            31
        } else {
            day
        };
        Self {
            year,
            month,
            day,
            open: false,
        }
    }

    /// The number of days in the current month.
    pub fn days_in_month(&self) -> u32 {
        days_in_month(self.year, self.month)
    }

    /// Moves by `delta` months, keeping the day inside the new month.
    pub fn add_months(&mut self, delta: i32) {
        let total = self.year.saturating_mul(12) + self.month as i32 - 1 + delta;
        self.year = total.div_euclid(12);
        self.month = total.rem_euclid(12) as u32 + 1;
        self.day = self.day.clamp(1, self.days_in_month());
    }
}

/// The result of [`Ui::calendar`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalendarResponse {
    /// Interaction with the calendar surface.
    pub response: Response,
    /// Day selected by a click this frame.
    pub selected: Option<u32>,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A toggleable button, the WinUI `ToggleButton`.
    ///
    /// The selected state is drawn with the accent colour; the returned
    /// response reports a change when the value flips.
    pub fn toggle_button(&mut self, value: &mut bool, label: &str) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = self
            .painter
            .text_width(label, metrics.font_scale)
            .saturating_add(metrics.padding_x * 2 + 26)
            .max(metrics.min_control_size);
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        if response.clicked {
            *value = !*value;
            response.changed = true;
        }
        let (fill, text) = if *value {
            (palette.accent, palette.accent_text)
        } else if response.hovered {
            (palette.surface_hover, palette.text)
        } else {
            (palette.surface_alt, palette.text)
        };
        self.painter.fill_rounded_rect(rect, metrics.radius, fill);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            if *value {
                palette.accent
            } else {
                palette.border
            },
        );
        if response.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), metrics.radius, 2, palette.focus);
        }
        let text_rect = Rect::new(
            rect.x + metrics.padding_x / 2 + 16,
            rect.y,
            rect.width.saturating_sub((metrics.padding_x + 16) as u32),
            rect.height,
        );
        if *value {
            paint_icon(
                &mut self.painter,
                Rect::new(
                    rect.x + metrics.padding_x / 2,
                    rect.y + (rect.height as i32 - 14) / 2,
                    14,
                    14,
                ),
                Icon::Check,
                text,
            );
        }
        self.painter.draw_text_ellipsis(
            text_rect,
            label,
            text,
            metrics.font_scale,
            Align::Center,
            VerticalAlign::Middle,
        );
        response
    }

    /// A split button: a primary action with a chevron that opens a menu.
    pub fn split_button(
        &mut self,
        state: &mut MenuState,
        label: &str,
        entries: &[MenuEntry<'_>],
    ) -> SplitButtonResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = self
            .painter
            .text_width(label, metrics.font_scale)
            .saturating_add(metrics.padding_x * 3 + metrics.control_height)
            .max(metrics.min_control_size * 2);
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let chevron_width = metrics.control_height.min(rect.width as i32);
        let (main, chevron) = rect.split_right(chevron_width);
        let mut response = self.interact(id, main, Sense::click());
        let arrow = self.interact(id.child(0xc4e), chevron, Sense::click());

        let (mut selected, dismissed) = self.menu_keys(state, entries);
        if arrow.clicked && selected.is_none() {
            state.open = !state.open;
            if state.open {
                state.highlighted = next_enabled(entries, usize::MAX);
            }
        }
        let popup = self.menu_popup(id, chevron, state, entries, rect.width as i32);
        if popup.selected.is_some() {
            selected = popup.selected;
        }
        if selected.is_some() {
            response.changed = true;
            response.clicked = true;
        }

        let fill = if response.pressed {
            palette.surface_active
        } else if response.hovered || state.open {
            palette.surface_hover
        } else {
            palette.surface_alt
        };
        self.painter.fill_rounded_rect(rect, metrics.radius, fill);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            if response.focused || state.open {
                palette.border_strong
            } else {
                palette.border
            },
        );
        self.painter.fill_rect(
            Rect::new(
                chevron.x,
                chevron.y + 6,
                1,
                chevron.height.saturating_sub(12),
            ),
            palette.border,
        );
        if arrow.hovered {
            self.painter.fill_rounded_rect(
                chevron.inset(3),
                metrics.radius.saturating_sub(3),
                palette.surface_hover,
            );
        }
        if response.focused || arrow.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), metrics.radius, 2, palette.focus);
        }
        self.painter.draw_text_ellipsis(
            Rect::new(
                main.x + metrics.padding_x,
                main.y,
                main.width.saturating_sub((metrics.padding_x * 2) as u32),
                main.height,
            ),
            label,
            palette.text,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        paint_icon(
            &mut self.painter,
            Rect::new(
                chevron.center().x - 8,
                chevron.y + (chevron.height as i32 - 16) / 2,
                16,
                16,
            ),
            if state.open {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            },
            palette.text_muted,
        );

        SplitButtonResponse {
            response,
            selected,
            dismissed,
        }
    }

    /// A full-width dropdown button that opens a command menu.
    ///
    /// This is the visual counterpart of [`select`](Ui::select): a labelled
    /// field with a chevron, but its popup contains [`MenuEntry`] commands.
    pub fn dropdown_button(
        &mut self,
        state: &mut MenuState,
        label: &str,
        entries: &[MenuEntry<'_>],
    ) -> MenuResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = if self.axis() == Axis::Vertical {
            self.remaining().0.max(1)
        } else {
            self.painter
                .text_width(label, metrics.font_scale)
                .saturating_add(metrics.padding_x * 2 + 22)
                .max(metrics.min_control_size)
        };
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        let (mut selected, mut dismissed) = self.menu_keys(state, entries);
        if response.clicked && selected.is_none() {
            state.open = !state.open;
            if state.open {
                state.highlighted = next_enabled(entries, usize::MAX);
            }
        }
        let popup = self.menu_popup(id, rect, state, entries, rect.width as i32);
        if popup.selected.is_some() {
            selected = popup.selected;
        }
        if popup.dismissed {
            dismissed = true;
        }
        if selected.is_some() {
            response.changed = true;
            response.clicked = true;
        }

        let fill = if response.hovered || state.open {
            palette.surface_hover
        } else {
            palette.surface_alt
        };
        self.painter.fill_rounded_rect(rect, metrics.radius, fill);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            if response.focused || state.open {
                palette.border_strong
            } else {
                palette.border
            },
        );
        if response.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), metrics.radius, 2, palette.focus);
        }
        self.painter.draw_text_ellipsis(
            Rect::new(
                rect.x + metrics.padding_x,
                rect.y,
                rect.width
                    .saturating_sub((metrics.padding_x * 2 + 18) as u32),
                rect.height,
            ),
            label,
            palette.text,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        paint_icon(
            &mut self.painter,
            Rect::new(
                rect.right_i32() - 22,
                rect.y + (rect.height as i32 - 16) / 2,
                16,
                16,
            ),
            if state.open {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            },
            palette.text_muted,
        );

        MenuResponse {
            response,
            selected,
            dismissed,
        }
    }

    /// A text link styled like a hyperlink.
    pub fn hyperlink_button(&mut self, label: &str) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = self
            .painter
            .text_width(label, metrics.font_scale)
            .max(metrics.min_control_size);
        let rect = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        let color = if response.hovered || response.active {
            palette.accent_hover
        } else {
            palette.accent
        };
        self.painter.draw_text_ellipsis(
            rect,
            label,
            color,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        if response.hovered || response.focused {
            self.painter.hline(
                rect.x,
                rect.x + width - 1,
                rect.y + rect.height as i32 / 2 + metrics.font_scale as i32 * 4 + 1,
                color,
            );
        }
        if response.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), 4, 1, palette.focus);
        }
        response.changed = false;
        response
    }

    /// A radio button group. Clicking a row sets `selected`.
    pub fn radio_group(&mut self, selected: &mut usize, labels: &[&str]) -> Response {
        let metrics = self.theme.metrics;
        let width = labels
            .iter()
            .map(|label| self.painter.text_width(label, metrics.font_scale))
            .max()
            .unwrap_or(0)
            .saturating_add(metrics.control_height + metrics.gap);
        let rect = self.allocate(width, metrics.control_height * labels.len().max(1) as i32);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        if labels.is_empty() {
            return response;
        }
        self.column(rect, 0, |ui| {
            for (index, label) in labels.iter().enumerate() {
                let item = ui.radio(selected, index, label);
                response.clicked |= item.clicked;
                response.changed |= item.changed;
                response.focused |= item.focused;
            }
        });
        response
    }

    /// A search field with a filtered suggestion list.
    ///
    /// Suggestions are prefix-filtered against the buffer (ASCII
    /// case-insensitive). Up/Down move the highlight, Enter accepts and Escape
    /// closes the list.
    pub fn auto_suggest<const M: usize>(
        &mut self,
        state: &mut AutoSuggestState,
        buffer: &mut TextBuffer<M>,
        placeholder: &str,
        suggestions: &[&str],
    ) -> AutoSuggestResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let mut response = self.search_box(buffer, placeholder);
        let rect = response.rect;
        let mut selected = None;

        if response.changed {
            state.open = !buffer.is_empty();
            state.highlighted = 0;
        }

        let matches = suggestions
            .iter()
            .filter(|suggestion| suggestion_matches(buffer.as_str(), suggestion))
            .count();
        if state.open && response.focused {
            if self.key_pressed(Key::Down) {
                state.highlighted = (state.highlighted + 1).min(matches.saturating_sub(1));
            }
            if self.key_pressed(Key::Up) {
                state.highlighted = state.highlighted.saturating_sub(1);
            }
            if self.escape_pressed() {
                state.open = false;
            }
            if self.enter_pressed() && matches > 0 {
                if let Some((index, suggestion)) = suggestions
                    .iter()
                    .enumerate()
                    .filter(|(_, suggestion)| suggestion_matches(buffer.as_str(), suggestion))
                    .nth(state.highlighted)
                {
                    buffer.set(suggestion);
                    state.open = false;
                    selected = Some(index);
                    response.changed = true;
                    response.submitted = true;
                }
            }
        }

        if matches == 0 || !state.open {
            return AutoSuggestResponse { response, selected };
        }

        let row_height = metrics.row_height;
        let visible = matches.min(6) as i32;
        let popup_height = visible * row_height + 4;
        let canvas = self.canvas_rect();
        let below = rect.bottom_i32() + 2;
        let popup_y = if below + popup_height <= canvas.bottom_i32() {
            below
        } else {
            (rect.y - popup_height - 2).max(canvas.y)
        };
        let popup = Rect::new(rect.x, popup_y, rect.width, popup_height.max(0) as u32);
        let id = self.next_id();
        if self.pointer_pressed() && !self.pointer_over(rect) && !self.pointer_over(popup) {
            state.open = false;
        }

        self.painter.shadow(popup, 6, 8, 140);
        self.painter.fill_rounded_rect(popup, 6, palette.surface);
        self.painter
            .stroke_rounded_rect(popup, 6, metrics.border_width, palette.border_strong);
        let clip = self.painter.push_clip(popup.inset(1));
        let mut row_index = 0usize;
        let mut y = popup.y + 2;
        for (index, suggestion) in suggestions.iter().enumerate() {
            if !suggestion_matches(buffer.as_str(), suggestion) {
                continue;
            }
            if row_index >= visible as usize {
                break;
            }
            let row = Rect::new(
                popup.x + 2,
                y,
                popup.width.saturating_sub(4),
                row_height as u32,
            );
            let hit = self.interact(
                id.child(index as u64 + 1),
                row,
                Sense::HOVER.with(Sense::CLICK),
            );
            if hit.hovered {
                state.highlighted = row_index;
            }
            if state.highlighted == row_index {
                self.painter
                    .fill_rounded_rect(row.inset_xy(2, 1), 4, palette.surface_active);
            } else if hit.hovered {
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
                suggestion,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            if hit.clicked {
                buffer.set(suggestion);
                state.open = false;
                selected = Some(index);
                response.changed = true;
                response.clicked = true;
            }
            y += row_height;
            row_index += 1;
        }
        self.painter.restore_clip(clip);

        AutoSuggestResponse { response, selected }
    }

    /// A month calendar.
    ///
    /// `state` stores the visible month and the selected day. The returned
    /// response reports month navigation and day selection.
    pub fn calendar(&mut self, rect: Rect, state: &mut CalendarState) -> CalendarResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let mut selected = None;
        if rect.width < 7 || rect.height < 8 {
            return CalendarResponse { response, selected };
        }

        let header_height = (metrics.control_height - 4).max(22);
        let weekday_height = metrics.small_scale as i32 * 10 + 6;
        let (header, rest) = rect.split_top(header_height);
        let (weekday, days) = rest.split_top(weekday_height);

        let button = (header_height - 8).max(14);
        let prev = Rect::new(
            header.x + 2,
            header.y + (header_height - button) / 2,
            button as u32,
            button as u32,
        );
        let next = Rect::new(
            header.right_i32() - button - 2,
            header.y + (header_height - button) / 2,
            button as u32,
            button as u32,
        );
        let prev_hit = self.interact(id.child(1), prev, Sense::click());
        let next_hit = self.interact(id.child(2), next, Sense::click());
        if prev_hit.clicked {
            state.add_months(-1);
            response.changed = true;
            response.clicked = true;
        }
        if next_hit.clicked {
            state.add_months(1);
            response.changed = true;
            response.clicked = true;
        }
        paint_icon(
            &mut self.painter,
            prev.inset(1),
            Icon::ChevronLeft,
            if prev_hit.hovered {
                palette.text
            } else {
                palette.text_muted
            },
        );
        paint_icon(
            &mut self.painter,
            next.inset(1),
            Icon::ChevronRight,
            if next_hit.hovered {
                palette.text
            } else {
                palette.text_muted
            },
        );
        let mut label_buf = [0u8; 24];
        let mut sink = FixedBuf::new(&mut label_buf);
        let _ = write!(&mut sink, "{} {}", month_name(state.month), state.year);
        let label = sink.into_str();
        self.painter.draw_text_ellipsis(
            Rect::new(
                prev.right_i32() + 6,
                header.y,
                (next.x - prev.right_i32() - 12).max(0) as u32,
                header.height,
            ),
            label,
            palette.text,
            metrics.font_scale,
            Align::Center,
            VerticalAlign::Middle,
        );

        let weekdays = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
        let column_width = days.width as i32 / 7;
        for (index, name) in weekdays.iter().enumerate() {
            self.painter.draw_text_ellipsis(
                Rect::new(
                    weekday.x + index as i32 * column_width,
                    weekday.y,
                    column_width.max(0) as u32,
                    weekday.height,
                ),
                name,
                palette.text_muted,
                metrics.small_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
        }

        let first = first_weekday(state.year, state.month);
        let total = state.days_in_month();
        state.day = state.day.clamp(1, total);
        let cell_height = (days.height as i32 / 6).max(1);
        let mut any_focused = false;
        for day in 1..=total {
            let index = first + day - 1;
            let row = index / 7;
            let column = index % 7;
            if row >= 6 {
                continue;
            }
            let cell = Rect::new(
                days.x + column as i32 * column_width,
                days.y + row as i32 * cell_height,
                column_width.max(0) as u32,
                cell_height.max(0) as u32,
            );
            let hit = self.interact(id.child(100 + day as u64), cell, Sense::click());
            any_focused |= hit.focused;
            if hit.clicked {
                state.day = day;
                selected = Some(day);
                response.changed = true;
                response.clicked = true;
            }
            let dot = (cell.width.min(cell.height) as i32 / 2 - 2).max(2) as u32;
            if state.day == day {
                self.painter
                    .fill_circle(cell.center().x, cell.center().y, dot, palette.accent);
            } else if hit.hovered {
                self.painter
                    .fill_rounded_rect(cell.inset(3), 4, palette.surface_hover);
            }
            self.painter.draw_text_ellipsis(
                cell,
                day_label(day),
                if state.day == day {
                    palette.accent_text
                } else {
                    palette.text
                },
                metrics.font_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
        }
        if any_focused {
            let mut changed = false;
            let mut day = state.day;
            if self.key_pressed(Key::Left) {
                day = day.saturating_sub(1).max(1);
                changed = true;
            }
            if self.key_pressed(Key::Right) {
                day = (day + 1).min(total);
                changed = true;
            }
            if self.key_pressed(Key::Up) {
                day = day.saturating_sub(7).max(1);
                changed = true;
            }
            if self.key_pressed(Key::Down) {
                day = (day + 7).min(total);
                changed = true;
            }
            if changed {
                state.day = day;
                response.changed = true;
            }
        }

        response.focused = any_focused;
        CalendarResponse { response, selected }
    }

    /// A date field that opens a calendar popup.
    pub fn date_picker(&mut self, state: &mut CalendarState) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(170, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());

        let mut buffer = [0u8; 16];
        let mut sink = FixedBuf::new(&mut buffer);
        let _ = write!(
            &mut sink,
            "{:04}-{:02}-{:02}",
            state.year, state.month, state.day
        );
        let label = sink.into_str();

        if response.clicked {
            state.open = !state.open;
        }

        let fill = if response.hovered || state.open {
            palette.surface_hover
        } else {
            palette.surface_alt
        };
        self.painter.fill_rounded_rect(rect, metrics.radius, fill);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            if response.focused || state.open {
                palette.border_strong
            } else {
                palette.border
            },
        );
        if response.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), metrics.radius, 2, palette.focus);
        }
        self.painter.draw_text_ellipsis(
            Rect::new(
                rect.x + metrics.padding_x,
                rect.y,
                rect.width
                    .saturating_sub((metrics.padding_x * 2 + 20) as u32),
                rect.height,
            ),
            label,
            palette.text,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        paint_icon(
            &mut self.painter,
            Rect::new(
                rect.right_i32() - 24,
                rect.y + (rect.height as i32 - 16) / 2,
                16,
                16,
            ),
            Icon::Calendar,
            palette.text_muted,
        );

        if state.open {
            let canvas = self.canvas_rect();
            let width = 300.min(canvas.width as i32).max(200);
            let height = 236.min(canvas.height as i32).max(160);
            let below = rect.bottom_i32() + 2;
            let y = if below + height <= canvas.bottom_i32() {
                below
            } else {
                (rect.y - height - 2).max(canvas.y)
            };
            let x = rect
                .x
                .clamp(canvas.x, (canvas.right_i32() - width).max(canvas.x));
            let popup = Rect::new(x, y, width as u32, height as u32);
            self.float(popup);
            if self.pointer_pressed() && !self.pointer_over(rect) && !self.pointer_over(popup) {
                state.open = false;
            }
            if self.escape_pressed() {
                state.open = false;
            }
            self.painter.shadow(popup, 8, 10, 150);
            self.painter.fill_rounded_rect(popup, 8, palette.surface);
            self.painter
                .stroke_rounded_rect(popup, 8, metrics.border_width, palette.border_strong);
            let inner = Rect::new(
                popup.x + metrics.padding_x,
                popup.y + metrics.padding_y,
                popup.width.saturating_sub((metrics.padding_x * 2) as u32),
                popup.height.saturating_sub((metrics.padding_y * 2) as u32),
            );
            let clip = self.painter.push_clip(inner);
            let calendar = self.calendar(inner, state);
            self.painter.restore_clip(clip);
            if calendar.selected.is_some() {
                state.open = false;
                response.changed = true;
                response.clicked = true;
            } else if calendar.response.changed {
                response.changed = true;
            }
        }
        response
    }

    /// A compact 24-hour time field.
    ///
    /// Click the upper half of the hour or minute segment to increment it and
    /// the lower half to decrement. Arrow keys work while a segment is
    /// focused.
    pub fn time_picker(&mut self, hour: &mut u32, minute: &mut u32) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(130, metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        *hour %= 24;
        *minute %= 60;

        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface_alt);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );

        let colon_width = self.painter.text_width(":", metrics.font_scale) + 8;
        let segment_width = (rect.width as i32 - colon_width) / 2;
        let hour_rect = Rect::new(rect.x, rect.y, segment_width.max(0) as u32, rect.height);
        let minute_rect = Rect::new(
            hour_rect.right_i32() + colon_width,
            rect.y,
            (rect.right_i32() - hour_rect.right_i32() - colon_width).max(0) as u32,
            rect.height,
        );
        let middle = rect.y + rect.height as i32 / 2;
        let hour_hit = self.interact(id.child(1), hour_rect, Sense::click());
        let minute_hit = self.interact(id.child(2), minute_rect, Sense::click());

        let mut changed = false;
        if hour_hit.clicked {
            if self.pointer().y >= middle {
                *hour = if *hour == 0 { 23 } else { *hour - 1 };
            } else {
                *hour = (*hour + 1) % 24;
            }
            changed = true;
        }
        if minute_hit.clicked {
            if self.pointer().y >= middle {
                *minute = if *minute == 0 { 59 } else { *minute - 1 };
            } else {
                *minute = (*minute + 1) % 60;
            }
            changed = true;
        }
        if hour_hit.focused {
            if self.key_pressed(Key::Up) || self.key_pressed(Key::Right) {
                *hour = (*hour + 1) % 24;
                changed = true;
            }
            if self.key_pressed(Key::Down) || self.key_pressed(Key::Left) {
                *hour = if *hour == 0 { 23 } else { *hour - 1 };
                changed = true;
            }
        }
        if minute_hit.focused {
            if self.key_pressed(Key::Up) || self.key_pressed(Key::Right) {
                *minute = (*minute + 1) % 60;
                changed = true;
            }
            if self.key_pressed(Key::Down) || self.key_pressed(Key::Left) {
                *minute = if *minute == 0 { 59 } else { *minute - 1 };
                changed = true;
            }
        }

        if hour_hit.hovered || hour_hit.focused {
            self.painter.fill_rounded_rect(
                hour_rect.inset(3),
                metrics.radius.saturating_sub(3),
                palette.surface_hover,
            );
        }
        if minute_hit.hovered || minute_hit.focused {
            self.painter.fill_rounded_rect(
                minute_rect.inset(3),
                metrics.radius.saturating_sub(3),
                palette.surface_hover,
            );
        }
        if hour_hit.focused || minute_hit.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), metrics.radius, 2, palette.focus);
        }

        let mut buffer = [0u8; 8];
        let mut sink = FixedBuf::new(&mut buffer);
        let _ = write!(&mut sink, "{:02}", hour);
        let hour_text = sink.into_str();
        let mut buffer = [0u8; 8];
        let mut sink = FixedBuf::new(&mut buffer);
        let _ = write!(&mut sink, "{:02}", minute);
        let minute_text = sink.into_str();
        self.painter.draw_text_ellipsis(
            hour_rect,
            hour_text,
            palette.text,
            metrics.font_scale,
            Align::Center,
            VerticalAlign::Middle,
        );
        self.painter.draw_text_ellipsis(
            minute_rect,
            minute_text,
            palette.text,
            metrics.font_scale,
            Align::Center,
            VerticalAlign::Middle,
        );
        self.painter.draw_text_ellipsis(
            Rect::new(
                hour_rect.right_i32(),
                rect.y,
                colon_width as u32,
                rect.height,
            ),
            ":",
            palette.text_muted,
            metrics.font_scale,
            Align::Center,
            VerticalAlign::Middle,
        );

        response.changed = changed;
        response.focused = hour_hit.focused || minute_hit.focused;
        response
    }
}

fn suggestion_matches(prefix: &str, suggestion: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    suggestion
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 30,
    }
}

/// Monday-based weekday of the first day of a month.
fn first_weekday(year: i32, month: u32) -> u32 {
    let (y, m) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let k = y.rem_euclid(100);
    let j = y.div_euclid(100);
    let h = (1 + 13 * (m as i32 + 1) / 5 + k + k / 4 + j / 4 + 5 * j).rem_euclid(7);
    ((h + 5) % 7) as u32
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn month_name(month: u32) -> &'static str {
    MONTHS[(month.max(1) - 1).min(11) as usize]
}

fn day_label(day: u32) -> &'static str {
    const DAYS: [&str; 31] = [
        "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
        "17", "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "28", "29", "30", "31",
    ];
    DAYS[(day.max(1) - 1).min(30) as usize]
}
