//! Collection views: list views, card grids, flip views and pips pagers.
//!
//! These are the desktop-style collection controls: a scrollable
//! `ListView`, a wrapping `GridView`, a `FlipView` with arrows and page dots,
//! and the small `PipsPager` often paired with a carousel.

use feos_gfx::Rect;

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::response::{Response, Sense};
use crate::ui::Ui;

use super::icons::{paint_icon, Icon};
use super::Tone;

/// One row in a [`Ui::list_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListItem<'a> {
    /// Leading icon.
    pub icon: Icon,
    /// Primary text.
    pub label: &'a str,
    /// Optional second line.
    pub detail: &'a str,
    /// Optional right-aligned text.
    pub trailing: &'a str,
    /// Semantic colour for the icon.
    pub tone: Tone,
    /// Disabled rows are dimmed and cannot be selected.
    pub enabled: bool,
}

impl<'a> ListItem<'a> {
    pub const fn new(icon: Icon, label: &'a str) -> Self {
        Self {
            icon,
            label,
            detail: "",
            trailing: "",
            tone: Tone::Neutral,
            enabled: true,
        }
    }

    pub const fn with_detail(self, detail: &'a str) -> Self {
        Self { detail, ..self }
    }

    pub const fn with_trailing(self, trailing: &'a str) -> Self {
        Self { trailing, ..self }
    }

    pub const fn with_tone(self, tone: Tone) -> Self {
        Self { tone, ..self }
    }

    pub const fn disabled(self) -> Self {
        Self {
            enabled: false,
            ..self
        }
    }
}

/// Selection state for [`Ui::list_view`].
///
/// `N` is the maximum number of rows tracked. Rows beyond `N` render as
/// unselected instead of panicking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListState<const N: usize = 32> {
    selected: [bool; N],
    /// When `true`, clicking rows toggles them independently.
    pub multi: bool,
}

impl<const N: usize> ListState<N> {
    /// A single-selection list.
    pub const fn new() -> Self {
        Self {
            selected: [false; N],
            multi: false,
        }
    }

    /// A multi-selection list.
    pub const fn multi() -> Self {
        Self {
            selected: [false; N],
            multi: true,
        }
    }

    /// Returns `true` when row `index` is selected.
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.get(index).copied().unwrap_or(false)
    }

    /// Selects exactly `index`, clearing the rest. Returns whether the
    /// selection changed.
    pub fn select(&mut self, index: usize) -> bool {
        if self.is_selected(index) && self.selected.iter().filter(|slot| **slot).count() == 1 {
            return false;
        }
        let mut changed = false;
        for (slot_index, slot) in self.selected.iter_mut().enumerate() {
            let value = slot_index == index;
            if *slot != value {
                *slot = value;
                changed = true;
            }
        }
        changed
    }

    /// Toggles `index` in multi-select mode.
    pub fn toggle(&mut self, index: usize) -> bool {
        if let Some(slot) = self.selected.get_mut(index) {
            *slot = !*slot;
            return true;
        }
        false
    }

    /// The first selected row, if any.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected.iter().position(|slot| *slot)
    }

    /// Number of selected rows.
    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|slot| **slot).count()
    }

    /// Clears every selected row.
    pub fn clear(&mut self) {
        self.selected = [false; N];
    }

    /// The raw selection flags.
    pub fn as_slice(&self) -> &[bool] {
        &self.selected
    }
}

impl<const N: usize> Default for ListState<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of [`Ui::list_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListResponse {
    /// Interaction with the list surface.
    pub response: Response,
    /// Row activated by a click this frame.
    pub activated: Option<usize>,
    /// Row whose selection changed this frame.
    pub changed_row: Option<usize>,
}

/// One card in a [`Ui::grid_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridItem<'a> {
    /// Large leading icon.
    pub icon: Icon,
    /// Card title.
    pub title: &'a str,
    /// Supporting line.
    pub detail: &'a str,
    /// Icon and accent colour.
    pub tone: Tone,
}

impl<'a> GridItem<'a> {
    pub const fn new(icon: Icon, title: &'a str) -> Self {
        Self {
            icon,
            title,
            detail: "",
            tone: Tone::Info,
        }
    }

    pub const fn with_detail(self, detail: &'a str) -> Self {
        Self { detail, ..self }
    }

    pub const fn with_tone(self, tone: Tone) -> Self {
        Self { tone, ..self }
    }
}

/// One slide in a [`Ui::flip_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlipItem<'a> {
    /// Slide title.
    pub title: &'a str,
    /// Wrapped body text.
    pub body: &'a str,
    /// Large leading icon.
    pub icon: Icon,
    /// Accent colour.
    pub tone: Tone,
}

impl<'a> FlipItem<'a> {
    pub const fn new(title: &'a str, body: &'a str) -> Self {
        Self {
            title,
            body,
            icon: Icon::Info,
            tone: Tone::Info,
        }
    }

    pub const fn with_icon(self, icon: Icon) -> Self {
        Self { icon, ..self }
    }

    pub const fn with_tone(self, tone: Tone) -> Self {
        Self { tone, ..self }
    }
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A scrollable list with icons, two-line rows and selection.
    ///
    /// Selection lives in [`ListState`]. The list scrolls with the wheel and
    /// clips its rows. Draw it wherever it fits; like tables, it does not
    /// allocate a layout cell per row.
    pub fn list_view<const M: usize>(
        &mut self,
        rect: Rect,
        items: &[ListItem<'_>],
        state: &mut ListState<M>,
    ) -> ListResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let mut activated = None;
        let mut changed_row = None;

        self.painter
            .fill_rounded_rect(rect, metrics.radius.min(8), palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius.min(8),
            metrics.border_width,
            palette.border,
        );

        let row_height = metrics.row_height + 10;
        let content_height = items.len() as i32 * row_height;
        let previous = self.state.scroll_content(id);
        let max_scroll = (previous - rect.height as i32).max(0);
        let mut offset = self.state.scroll_offset(id);
        if self.pointer_over(rect) && self.wheel() != 0 {
            offset = (offset - self.wheel() * row_height).clamp(0, max_scroll);
        }
        let old_clip = self.painter.push_clip(rect.inset(4));

        let mut y = rect.y + 4 - offset;
        for (index, item) in items.iter().enumerate() {
            let row = Rect::new(
                rect.x + 4,
                y,
                rect.width.saturating_sub(8),
                row_height as u32,
            );
            if row.bottom_i32() < rect.y {
                y += row_height;
                continue;
            }
            if y >= rect.bottom_i32() {
                break;
            }
            let hit = if item.enabled {
                Some(self.interact(
                    id.child(index as u64 + 1),
                    row,
                    Sense::HOVER.with(Sense::CLICK),
                ))
            } else {
                None
            };
            let selected = state.is_selected(index);
            if selected {
                self.painter
                    .fill_rounded_rect(row.inset_xy(2, 1), 5, palette.surface_active);
                self.painter.fill_rect(
                    Rect::new(row.x + 2, row.y + 5, 3, row.height.saturating_sub(10)),
                    palette.accent,
                );
            } else if hit.as_ref().is_some_and(|hit| hit.hovered) {
                self.painter
                    .fill_rounded_rect(row.inset_xy(2, 1), 5, palette.surface_hover);
            }

            let icon_size = (metrics.control_height - 12).clamp(14, 24);
            let icon_color = if item.enabled {
                item.tone.color(palette)
            } else {
                palette.text_disabled
            };
            paint_icon(
                &mut self.painter,
                Rect::new(
                    row.x + metrics.padding_x,
                    row.y + (row_height - icon_size) / 2,
                    icon_size as u32,
                    icon_size as u32,
                ),
                item.icon,
                icon_color,
            );
            let text_x = row.x + metrics.padding_x + icon_size + metrics.gap;
            let trailing_width = if item.trailing.is_empty() {
                0
            } else {
                self.painter.text_width(item.trailing, metrics.small_scale)
            };
            let text_width = (row.right_i32()
                - text_x
                - metrics.padding_x
                - trailing_width
                - if trailing_width > 0 { metrics.gap } else { 0 })
            .max(0) as u32;
            let text_color = if item.enabled {
                palette.text
            } else {
                palette.text_disabled
            };
            if item.detail.is_empty() {
                self.painter.draw_text_ellipsis(
                    Rect::new(text_x, row.y, text_width, row.height),
                    item.label,
                    text_color,
                    metrics.font_scale,
                    Align::Start,
                    VerticalAlign::Middle,
                );
            } else {
                let line = self.painter.line_height(metrics.font_scale);
                self.painter.draw_text_ellipsis(
                    Rect::new(text_x, row.y + 5, text_width, line as u32),
                    item.label,
                    text_color,
                    metrics.font_scale,
                    Align::Start,
                    VerticalAlign::Top,
                );
                self.painter.draw_text_ellipsis(
                    Rect::new(text_x, row.y + 5 + line + 2, text_width, row.height),
                    item.detail,
                    if item.enabled {
                        palette.text_muted
                    } else {
                        palette.text_disabled
                    },
                    metrics.small_scale,
                    Align::Start,
                    VerticalAlign::Top,
                );
            }
            if trailing_width > 0 {
                self.painter.draw_text_ellipsis(
                    Rect::new(
                        row.right_i32() - metrics.padding_x - trailing_width,
                        row.y,
                        trailing_width as u32,
                        row.height,
                    ),
                    item.trailing,
                    palette.text_muted,
                    metrics.small_scale,
                    Align::End,
                    VerticalAlign::Middle,
                );
            }
            if let Some(hit) = hit {
                if hit.clicked {
                    let changed = if state.multi {
                        state.toggle(index)
                    } else {
                        state.select(index)
                    };
                    if changed {
                        changed_row = Some(index);
                        response.changed = true;
                        response.clicked = true;
                    }
                    activated = Some(index);
                }
            }
            y += row_height;
        }
        self.painter.restore_clip(old_clip);
        let max_scroll = (content_height - rect.height as i32).max(0);
        offset = offset.clamp(0, max_scroll);
        self.state.set_scroll(id, offset, content_height);

        ListResponse {
            response,
            activated,
            changed_row,
        }
    }

    /// A wrapping grid of cards.
    ///
    /// The column count follows the available width (140–190 px cards), so a
    /// grid reflows naturally between narrow and wide panes.
    pub fn grid_view(
        &mut self,
        rect: Rect,
        items: &[GridItem<'_>],
        selected: &mut usize,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        if items.is_empty() || rect.is_empty() {
            return response;
        }
        let gap = metrics.gap;
        let columns = (rect.width as i32 / 160).clamp(1, 4) as usize;
        let card_width = ((rect.width as i32 - gap * (columns as i32 - 1)) / columns as i32).max(1);
        let card_height = 104;
        let old_clip = self.painter.push_clip(rect);
        for (index, item) in items.iter().enumerate() {
            let column = index % columns;
            let row = index / columns;
            let card = Rect::new(
                rect.x + column as i32 * (card_width + gap),
                rect.y + row as i32 * (card_height + gap),
                card_width as u32,
                card_height as u32,
            );
            if card.y >= rect.bottom_i32() {
                break;
            }
            let hit = self.interact(
                id.child(index as u64 + 1),
                card,
                Sense::HOVER.with(Sense::CLICK),
            );
            if hit.clicked && *selected != index {
                *selected = index;
                response.changed = true;
                response.clicked = true;
            }
            let active = *selected == index;
            self.painter.fill_rounded_rect(
                card,
                metrics.radius,
                if hit.hovered {
                    palette.surface_hover
                } else {
                    palette.surface
                },
            );
            self.painter.stroke_rounded_rect(
                card,
                metrics.radius,
                if active { 2 } else { metrics.border_width },
                if active {
                    palette.accent
                } else {
                    palette.border
                },
            );
            let icon_size = 30;
            let icon_rect = Rect::new(
                card.x + metrics.padding_x,
                card.y + metrics.padding_x,
                icon_size as u32,
                icon_size as u32,
            );
            self.painter.fill_rounded_rect(
                icon_rect,
                (icon_size / 2) as u32,
                item.tone.tint(palette),
            );
            paint_icon(
                &mut self.painter,
                icon_rect.inset(6),
                item.icon,
                item.tone.color(palette),
            );
            self.painter.draw_text_ellipsis(
                Rect::new(
                    card.x + metrics.padding_x,
                    icon_rect.bottom_i32() + 8,
                    card.width.saturating_sub((metrics.padding_x * 2) as u32),
                    metrics.font_scale * 9,
                ),
                item.title,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Top,
            );
            if !item.detail.is_empty() {
                self.painter.draw_text_ellipsis(
                    Rect::new(
                        card.x + metrics.padding_x,
                        card.bottom_i32() - metrics.padding_x - metrics.small_scale as i32 * 8,
                        card.width.saturating_sub((metrics.padding_x * 2) as u32),
                        metrics.small_scale * 9,
                    ),
                    item.detail,
                    palette.text_muted,
                    metrics.small_scale,
                    Align::Start,
                    VerticalAlign::Top,
                );
            }
            if hit.focused {
                self.painter
                    .stroke_rounded_rect(card.inset(2), metrics.radius, 1, palette.focus);
            }
        }
        self.painter.restore_clip(old_clip);
        response
    }

    /// A carousel with one slide, arrow buttons and page dots.
    pub fn flip_view(&mut self, rect: Rect, items: &[FlipItem<'_>], index: &mut usize) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        if items.is_empty() || rect.is_empty() {
            return response;
        }
        *index = (*index).min(items.len() - 1);
        let item = &items[*index];

        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );

        let padding = (metrics.padding_x * 2).max(20);
        let arrow = (metrics.control_height + 6)
            .min(rect.width as i32 / 5)
            .max(20);
        let pager_height = 26;
        let content = Rect::new(
            rect.x + padding,
            rect.y + padding,
            rect.width
                .saturating_sub((padding * 2) as u32)
                .saturating_sub((arrow * 2 + 12) as u32),
            rect.height
                .saturating_sub((padding * 2 + pager_height) as u32),
        );

        let icon_size = (metrics.control_height + 10).min(content.width as i32);
        let icon_rect = Rect::new(
            content.x,
            content.y,
            icon_size.max(0) as u32,
            icon_size.max(0) as u32,
        );
        self.painter.fill_rounded_rect(
            icon_rect,
            (icon_size / 2).max(0) as u32,
            item.tone.tint(palette),
        );
        paint_icon(
            &mut self.painter,
            icon_rect.inset(icon_size / 5),
            item.icon,
            item.tone.color(palette),
        );
        let text_x = icon_rect.right_i32() + metrics.gap * 2;
        let text_width = (content.right_i32() - text_x).max(0) as u32;
        self.painter.draw_text_ellipsis(
            Rect::new(text_x, content.y, text_width, metrics.heading_scale * 9),
            item.title,
            palette.text,
            metrics.heading_scale,
            Align::Start,
            VerticalAlign::Top,
        );
        let body_top = content.y + metrics.heading_scale as i32 * 10 + 6;
        self.painter.draw_text_wrapped(
            Rect::new(
                text_x,
                body_top,
                text_width,
                (content.bottom_i32() - body_top).max(0) as u32,
            ),
            item.body,
            palette.text_muted,
            metrics.font_scale,
        );

        let arrow_y = rect.y + (rect.height as i32 - arrow) / 2;
        let left = Rect::new(rect.x + padding / 2, arrow_y, arrow as u32, arrow as u32);
        let right = Rect::new(
            rect.right_i32() - padding / 2 - arrow,
            arrow_y,
            arrow as u32,
            arrow as u32,
        );
        let left_hit = self.interact(id.child(1), left, Sense::click());
        let right_hit = self.interact(id.child(2), right, Sense::click());
        for (hit, glyph) in [
            (left_hit, Icon::ChevronLeft),
            (right_hit, Icon::ChevronRight),
        ] {
            self.painter.fill_rounded_rect(
                hit.rect,
                (arrow / 2) as u32,
                if hit.hovered || hit.focused {
                    palette.surface_hover
                } else {
                    palette.surface_alt
                },
            );
            paint_icon(
                &mut self.painter,
                hit.rect.inset(arrow / 4),
                glyph,
                palette.text,
            );
        }
        if left_hit.clicked && *index > 0 {
            *index -= 1;
            response.changed = true;
            response.clicked = true;
        }
        if right_hit.clicked && *index + 1 < items.len() {
            *index += 1;
            response.changed = true;
            response.clicked = true;
        }

        let pager = Rect::new(
            rect.x,
            rect.bottom_i32() - pager_height - 4,
            rect.width,
            pager_height as u32,
        );
        let pager = self.pips_pager_in(pager, index, items.len());
        response.changed |= pager.changed;
        response.clicked |= pager.clicked;

        response
    }

    /// Page dots for a carousel. Draws in the next layout cell.
    pub fn pips_pager(&mut self, current: &mut usize, count: usize) -> Response {
        let rect = self.allocate(count as i32 * 16 + 8, 20);
        self.pips_pager_in(rect, current, count)
    }

    fn pips_pager_in(&mut self, rect: Rect, current: &mut usize, count: usize) -> Response {
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        if count == 0 {
            return response;
        }
        *current = (*current).min(count - 1);
        let dot = 8;
        let gap = 8;
        let active_width = 20;
        let total = (count as i32 - 1) * (dot + gap) + active_width;
        let mut x = rect.x + (rect.width as i32 - total).max(0) / 2;
        let y = rect.y + (rect.height as i32 - dot) / 2;
        for index in 0..count {
            let width = if index == *current { active_width } else { dot };
            let hit_rect = Rect::new(x, rect.y, width as u32, rect.height);
            let hit = self.interact(
                id.child(index as u64 + 1),
                hit_rect,
                Sense::HOVER.with(Sense::CLICK),
            );
            if hit.clicked && *current != index {
                *current = index;
                response.changed = true;
                response.clicked = true;
            }
            self.painter.fill_rounded_rect(
                Rect::new(x, y, width as u32, dot as u32),
                (dot / 2) as u32,
                if index == *current {
                    palette.accent
                } else if hit.hovered {
                    palette.text_muted
                } else {
                    palette.border_strong
                },
            );
            x += width + gap;
        }
        response
    }
}
