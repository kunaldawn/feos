//! Overlay components: shared menu popups, context menus, flyouts and
//! teaching tips.
//!
//! These build on the same rule as the rest of the toolkit: floating surfaces
//! are painted immediately, so callers draw them late in the frame. The menu
//! renderer is shared by [`Ui::menu_button`](crate::Ui::menu_button),
//! [`Ui::context_menu`], [`Ui::dropdown_button`](crate::Ui::dropdown_button),
//! [`Ui::split_button`](crate::Ui::split_button),
//! [`Ui::menu_bar`](crate::Ui::menu_bar) and
//! [`Ui::command_bar`](crate::Ui::command_bar).

use feos_gfx::Rect;

use crate::geometry::{Align, Padding, RectExt, VerticalAlign};
use crate::id::Id;
use crate::input::Key;
use crate::response::{Response, Sense};
use crate::ui::Ui;

use super::icons::{paint_icon, Icon};
use super::inputs::{MenuEntry, MenuResponse, MenuState};

/// The result of drawing a menu popup.
pub(crate) struct MenuPopup {
    /// Entry activated by a click this frame.
    pub selected: Option<usize>,
    /// The menu was dismissed by a click outside.
    pub dismissed: bool,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// Handles keyboard navigation for an open menu.
    ///
    /// Returns the activated entry and whether Escape closed the menu. Menu
    /// navigation is deliberately global while a menu is open: menus behave
    /// like an overlay, so they take the arrow and Enter keys even if focus
    /// still points at the owning button.
    pub(crate) fn menu_keys(
        &mut self,
        state: &mut MenuState,
        entries: &[MenuEntry<'_>],
    ) -> (Option<usize>, bool) {
        let mut selected = None;
        let mut dismissed = false;
        if !state.open {
            return (selected, dismissed);
        }
        if self.key_pressed(Key::Up) {
            state.highlighted = previous_enabled(entries, state.highlighted);
        }
        if self.key_pressed(Key::Down) {
            state.highlighted = next_enabled(entries, state.highlighted);
        }
        if self.key_pressed(Key::Home) {
            state.highlighted = next_enabled(entries, usize::MAX);
        }
        if self.key_pressed(Key::End) {
            state.highlighted = previous_enabled(entries, usize::MAX);
        }
        if self.enter_pressed() && entries.get(state.highlighted).is_some_and(|e| e.enabled()) {
            selected = Some(state.highlighted);
            state.open = false;
        }
        if self.escape_pressed() {
            state.open = false;
            dismissed = true;
        }
        (selected, dismissed)
    }

    /// Draws an open menu anchored below `anchor`.
    ///
    /// The popup flips above the anchor when there is no room below and is
    /// clamped to the canvas horizontally. The caller owns keyboard handling
    /// through [`menu_keys`](Ui::menu_keys).
    pub(crate) fn menu_popup(
        &mut self,
        id: Id,
        anchor: Rect,
        state: &mut MenuState,
        entries: &[MenuEntry<'_>],
        min_width: i32,
    ) -> MenuPopup {
        let empty = MenuPopup {
            selected: None,
            dismissed: false,
        };
        if !state.open {
            return empty;
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
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
            .max(min_width);
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
        let below = anchor.bottom_i32() + 2;
        let popup_y = if below + popup_height <= canvas.bottom_i32() {
            below
        } else {
            (anchor.y - popup_height - 2).max(canvas.y)
        };
        let popup_x = anchor
            .x
            .clamp(canvas.x, (canvas.right_i32() - popup_width).max(canvas.x));
        let popup = Rect::new(
            popup_x,
            popup_y,
            popup_width.max(0) as u32,
            popup_height.max(0) as u32,
        );
        self.float(popup);

        let mut dismissed = false;
        if self.pointer_pressed() && !self.pointer_over(anchor) && !self.pointer_over(popup) {
            state.open = false;
            dismissed = true;
        }

        let mut selected = None;
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
                    if y >= popup.bottom_i32() {
                        break;
                    }
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
                        let shortcut_width = self.painter.text_width(shortcut, metrics.small_scale);
                        self.painter.draw_text_ellipsis(
                            Rect::new(
                                row.right_i32() - metrics.padding_x - shortcut_width,
                                row.y,
                                shortcut_width as u32,
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

        MenuPopup {
            selected,
            dismissed,
        }
    }

    /// A context menu at `anchor` (usually a small rectangle at the pointer).
    ///
    /// Draw it at the end of the frame. The caller decides what a chosen entry
    /// means; `MenuResponse::selected` is the index into `entries`.
    pub fn context_menu(
        &mut self,
        anchor: Rect,
        state: &mut MenuState,
        entries: &[MenuEntry<'_>],
    ) -> MenuResponse {
        let id = self.next_id();
        let (mut selected, mut dismissed) = self.menu_keys(state, entries);
        let popup = self.menu_popup(id, anchor, state, entries, 0);
        if popup.selected.is_some() {
            selected = popup.selected;
        }
        if popup.dismissed {
            dismissed = true;
        }
        let mut response = Response::empty(id, anchor);
        response.changed = selected.is_some();
        response.clicked = selected.is_some();
        MenuResponse {
            response,
            selected,
            dismissed,
        }
    }
}

/// Where a [`Ui::flyout`] is placed relative to its anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FlyoutPlacement {
    /// Below the anchor, flipping above when there is no room.
    #[default]
    Bottom,
    /// Above the anchor.
    Top,
    /// To the left of the anchor.
    Left,
    /// To the right of the anchor.
    Right,
}

/// Persistent state for [`Ui::flyout`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlyoutState {
    /// Whether the flyout is visible.
    pub open: bool,
}

impl FlyoutState {
    pub const fn new() -> Self {
        Self { open: false }
    }
}

/// Persistent state for [`Ui::teaching_tip`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TeachingTipState {
    /// Whether the tip is visible.
    pub open: bool,
}

impl TeachingTipState {
    pub const fn new() -> Self {
        Self { open: false }
    }
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A lightweight floating panel anchored on one side of `anchor`.
    ///
    /// Unlike [`popover`](crate::Ui::popover), a flyout can be placed on any
    /// side and has no arrow. It closes on Escape or a click outside the
    /// panel. Draw it late in the frame.
    pub fn flyout<R>(
        &mut self,
        anchor: Rect,
        state: &mut FlyoutState,
        placement: FlyoutPlacement,
        width: i32,
        height: i32,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if !state.open {
            return None;
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let canvas = self.canvas_rect();
        let width = width.clamp(0, canvas.width as i32);
        let height = height.clamp(0, canvas.height as i32);
        let x = match placement {
            FlyoutPlacement::Left => anchor.x - width - 2,
            FlyoutPlacement::Right => anchor.right_i32() + 2,
            FlyoutPlacement::Top | FlyoutPlacement::Bottom => anchor.x,
        };
        let y = match placement {
            FlyoutPlacement::Top => anchor.y - height - 2,
            FlyoutPlacement::Bottom => anchor.bottom_i32() + 2,
            FlyoutPlacement::Left | FlyoutPlacement::Right => anchor.y,
        };
        let panel = Rect::new(
            x.clamp(canvas.x, (canvas.right_i32() - width).max(canvas.x)),
            y.clamp(canvas.y, (canvas.bottom_i32() - height).max(canvas.y)),
            width.max(0) as u32,
            height.max(0) as u32,
        );

        self.painter.shadow(panel, metrics.radius.min(8), 10, 150);
        self.painter
            .fill_rounded_rect(panel, metrics.radius.min(8), palette.surface);
        self.painter.stroke_rounded_rect(
            panel,
            metrics.radius.min(8),
            metrics.border_width,
            palette.border_strong,
        );

        let content = Padding::all(metrics.padding_x).apply(panel);
        let clip = self.painter.push_clip(content);
        let value = self.column_with(content, metrics.gap, f).1;
        self.painter.restore_clip(clip);

        let outside = self.pointer_pressed()
            && self.pointer_over(canvas)
            && !self.pointer_over(panel)
            && !self.pointer_over(anchor);
        if outside || self.escape_pressed() {
            state.open = false;
        }
        Some(value)
    }

    /// A dismissible teaching tip anchored near `anchor`.
    ///
    /// The tip is intentionally simple: a title, wrapped body, a close button
    /// and a confirming button. It sets `state.open` to `false` when dismissed.
    pub fn teaching_tip(
        &mut self,
        anchor: Rect,
        state: &mut TeachingTipState,
        title: &str,
        message: &str,
    ) -> Response {
        if !state.open {
            return Response::empty(Id::NONE, Rect::new(0, 0, 0, 0));
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let canvas = self.canvas_rect();
        let width = 300.min(canvas.width as i32).max(120);
        let padding = metrics.padding_x;
        let text_width = (width - padding * 2).max(1);
        let message_height = self
            .painter
            .wrapped_text_height(text_width, message, metrics.font_scale)
            .max(metrics.control_height);
        let height = (metrics.title_scale as i32 * 8
            + message_height
            + metrics.control_height
            + padding * 3)
            .max(120);
        let below = anchor.bottom_i32() + 8;
        let y = if below + height <= canvas.bottom_i32() {
            below
        } else {
            (anchor.y - height - 8).max(canvas.y)
        };
        let x = anchor
            .x
            .clamp(canvas.x, (canvas.right_i32() - width).max(canvas.x));
        let panel = Rect::new(x, y, width as u32, height as u32);

        self.painter.shadow(panel, metrics.radius, 14, 170);
        self.painter
            .fill_rounded_rect(panel, metrics.radius, palette.surface);
        self.painter.stroke_rounded_rect(
            panel,
            metrics.radius,
            metrics.border_width,
            palette.accent,
        );

        let id = self.next_id();
        let icon_size = (metrics.control_height - 12).max(14);
        let icon_rect = Rect::new(
            panel.x + padding,
            panel.y + padding,
            icon_size as u32,
            icon_size as u32,
        );
        self.painter.fill_circle(
            icon_rect.center().x,
            icon_rect.center().y,
            (icon_size / 2) as u32,
            palette.accent,
        );
        paint_icon(
            &mut self.painter,
            icon_rect.inset(3),
            Icon::Info,
            palette.accent_text,
        );
        self.painter.draw_text_ellipsis(
            Rect::new(
                icon_rect.right_i32() + metrics.gap,
                panel.y + padding,
                (panel.right_i32() - icon_rect.right_i32() - metrics.gap - padding).max(0) as u32,
                icon_size as u32,
            ),
            title,
            palette.text,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );

        let close_size = (metrics.control_height - 14).max(12);
        let close_rect = Rect::new(
            panel.right_i32() - padding - close_size,
            panel.y + padding,
            close_size as u32,
            close_size as u32,
        );
        let close = self.interact(id.child(1), close_rect, Sense::HOVER.with(Sense::CLICK));
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

        let body = Rect::new(
            panel.x + padding,
            icon_rect.bottom_i32() + metrics.gap,
            text_width as u32,
            message_height as u32,
        );
        self.painter
            .draw_text_wrapped(body, message, palette.text_muted, metrics.font_scale);

        let button_rect = Rect::new(
            panel.right_i32() - padding - 96,
            panel.bottom_i32() - padding - metrics.control_height,
            96,
            metrics.control_height as u32,
        );
        let confirm = self.button_at(button_rect, "Got it", true);

        let mut response = Response::empty(id, panel);
        response.changed = close.clicked || confirm.clicked || self.escape_pressed();
        response.clicked = confirm.clicked;
        if response.changed {
            state.open = false;
        }
        response
    }
}

fn next_enabled(entries: &[MenuEntry<'_>], current: usize) -> usize {
    let len = entries.len();
    if len == 0 {
        return 0;
    }
    let start = if current == usize::MAX {
        0
    } else {
        (current + 1) % len
    };
    for offset in 0..len {
        let index = (start + offset) % len;
        if entries[index].enabled() {
            return index;
        }
    }
    current.min(len - 1)
}

fn previous_enabled(entries: &[MenuEntry<'_>], current: usize) -> usize {
    let len = entries.len();
    if len == 0 {
        return 0;
    }
    let start = if current == usize::MAX {
        len - 1
    } else {
        (current + len - 1) % len
    };
    for offset in 0..len {
        let index = (start + len - offset) % len;
        if entries[index].enabled() {
            return index;
        }
    }
    current.min(len - 1)
}
