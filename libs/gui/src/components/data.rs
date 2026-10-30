//! Data-display components: tables, key/value lists, timelines, trees, stat
//! cards, avatars and chips.

use feos_gfx::{Color, Rect};

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::response::{Response, Sense};
use crate::text::format_u32;
use crate::ui::Ui;

use super::icons::{paint_icon, Icon};
use super::Tone;

/// How a table column computes its width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnWidth {
    /// A fixed pixel width.
    Fixed(i32),
    /// A share of the remaining width.
    Flex(u16),
}

/// One table column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column<'a> {
    pub title: &'a str,
    pub width: ColumnWidth,
}

impl<'a> Column<'a> {
    pub const fn fixed(title: &'a str, width: i32) -> Self {
        Self {
            title,
            width: ColumnWidth::Fixed(width),
        }
    }

    pub const fn flex(title: &'a str, weight: u16) -> Self {
        Self {
            title,
            width: ColumnWidth::Flex(weight),
        }
    }
}

/// The result of [`Ui::table`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableResponse {
    pub response: Response,
    /// Column whose header was clicked this frame.
    pub header_clicked: Option<usize>,
}

/// One timeline entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineItem<'a> {
    pub title: &'a str,
    pub detail: &'a str,
    pub tone: Tone,
}

impl<'a> TimelineItem<'a> {
    pub const fn new(title: &'a str, detail: &'a str) -> Self {
        Self {
            title,
            detail,
            tone: Tone::Info,
        }
    }

    pub const fn with_tone(self, tone: Tone) -> Self {
        Self { tone, ..self }
    }
}

/// One visible row in a [`Ui::tree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRow<'a> {
    pub label: &'a str,
    /// Nesting depth, starting at zero for roots.
    pub depth: u8,
    pub has_children: bool,
    pub expanded: bool,
}

impl<'a> TreeRow<'a> {
    pub const fn new(label: &'a str, depth: u8) -> Self {
        Self {
            label,
            depth,
            has_children: false,
            expanded: false,
        }
    }

    pub const fn branch(label: &'a str, depth: u8, expanded: bool) -> Self {
        Self {
            label,
            depth,
            has_children: true,
            expanded,
        }
    }
}

/// Expansion and selection state for [`Ui::tree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeState<const N: usize = 32> {
    expanded: [bool; N],
    pub selected: usize,
}

impl<const N: usize> TreeState<N> {
    pub const fn new() -> Self {
        Self {
            expanded: [false; N],
            selected: 0,
        }
    }

    pub fn is_expanded(&self, index: usize) -> bool {
        self.expanded.get(index).copied().unwrap_or(false)
    }

    pub fn set_expanded(&mut self, index: usize, value: bool) {
        if let Some(slot) = self.expanded.get_mut(index) {
            *slot = value;
        }
    }

    pub fn toggle(&mut self, index: usize) {
        if let Some(slot) = self.expanded.get_mut(index) {
            *slot = !*slot;
        }
    }

    pub fn as_slice(&self) -> &[bool] {
        &self.expanded
    }

    pub fn expanded_mut(&mut self) -> &mut [bool] {
        &mut self.expanded
    }
}

impl<const N: usize> Default for TreeState<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of [`Ui::chip`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChipResponse {
    pub response: Response,
    pub removed: bool,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A data table with a sticky header, selectable rows and wheel scrolling.
    ///
    /// `selected` is a row index; `TableResponse::header_clicked` is returned
    /// so the caller can implement sorting.
    pub fn table<'r>(
        &mut self,
        rect: Rect,
        columns: &[Column<'_>],
        rows: &[&'r [&'r str]],
        selected: &mut usize,
    ) -> TableResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let mut header_clicked = None;
        if columns.is_empty() {
            return TableResponse {
                response,
                header_clicked,
            };
        }

        let header_height = metrics.row_height + 6;
        let body = Rect::new(
            rect.x,
            rect.y + header_height,
            rect.width,
            rect.height.saturating_sub(header_height as u32),
        );
        let content_height = rows.len() as i32 * metrics.row_height;
        let previous = self.state.scroll_content(id);
        let max_scroll = (previous - body.height as i32).max(0);
        let mut offset = self.state.scroll_offset(id);
        if self.pointer_over(body) && self.wheel() != 0 {
            offset = (offset - self.wheel() * metrics.row_height).clamp(0, max_scroll);
        }

        let fixed: i32 = columns
            .iter()
            .map(|column| match column.width {
                ColumnWidth::Fixed(width) => width.max(0),
                ColumnWidth::Flex(_) => 0,
            })
            .sum();
        let flex: i32 = columns
            .iter()
            .map(|column| match column.width {
                ColumnWidth::Flex(weight) => weight.max(1) as i32,
                ColumnWidth::Fixed(_) => 0,
            })
            .sum();
        let remaining = (rect.width as i32 - fixed).max(0);

        self.painter
            .fill_rounded_rect(rect, metrics.radius.min(8), palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius.min(8),
            metrics.border_width,
            palette.border,
        );
        self.painter.fill_rounded_rect(
            Rect::new(rect.x, rect.y, rect.width, header_height as u32),
            metrics.radius.min(8),
            palette.surface_alt,
        );
        self.painter.fill_rect(
            Rect::new(rect.x, rect.y + header_height - 1, rect.width, 1),
            palette.border,
        );

        let mut x = rect.x;
        for (index, column) in columns.iter().enumerate() {
            let width = match column.width {
                ColumnWidth::Fixed(width) => width.max(0),
                ColumnWidth::Flex(weight) => remaining * weight.max(1) as i32 / flex.max(1),
            };
            let width = if index + 1 == columns.len() {
                (rect.right_i32() - x).max(0)
            } else {
                width
            };
            let cell = Rect::new(x, rect.y, width.max(0) as u32, header_height as u32);
            let hit = self.interact(
                id.child(index as u64 + 1),
                cell,
                Sense::HOVER.with(Sense::CLICK),
            );
            if hit.clicked {
                header_clicked = Some(index);
                response.clicked = true;
            }
            if hit.hovered {
                self.painter.fill_rect(cell, palette.surface_hover);
            }
            self.painter.draw_text_ellipsis(
                Rect::new(
                    cell.x + metrics.padding_x,
                    cell.y,
                    cell.width.saturating_sub((metrics.padding_x * 2) as u32),
                    cell.height,
                ),
                column.title,
                palette.text_muted,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            x += width;
        }

        let old_clip = self.painter.push_clip(body);
        for (index, row) in rows.iter().enumerate() {
            let y = body.y + index as i32 * metrics.row_height - offset;
            let row_rect = Rect::new(rect.x, y, rect.width, metrics.row_height as u32);
            if row_rect.bottom_i32() < body.y || row_rect.y >= body.bottom_i32() {
                continue;
            }
            let row_response =
                self.interact(id.child(0x1000 + index as u64), row_rect, Sense::click());
            if row_response.clicked && *selected != index {
                *selected = index;
                response.changed = true;
                response.clicked = true;
            }
            if *selected == index {
                self.painter.fill_rect(row_rect, palette.surface_active);
            } else if row_response.hovered {
                self.painter.fill_rect(row_rect, palette.surface_hover);
            }
            let mut cell_x = rect.x;
            for (column_index, column) in columns.iter().enumerate() {
                let width = match column.width {
                    ColumnWidth::Fixed(width) => width.max(0),
                    ColumnWidth::Flex(weight) => remaining * weight.max(1) as i32 / flex.max(1),
                };
                let width = if column_index + 1 == columns.len() {
                    (rect.right_i32() - cell_x).max(0)
                } else {
                    width
                };
                let text = row.get(column_index).copied().unwrap_or("");
                if width > metrics.padding_x * 2 {
                    self.painter.draw_text_ellipsis(
                        Rect::new(
                            cell_x + metrics.padding_x,
                            y,
                            width as u32 - (metrics.padding_x * 2) as u32,
                            metrics.row_height as u32,
                        ),
                        text,
                        palette.text,
                        metrics.font_scale,
                        Align::Start,
                        VerticalAlign::Middle,
                    );
                }
                cell_x += width;
            }
            if index + 1 < rows.len() {
                self.painter.fill_rect(
                    Rect::new(rect.x, row_rect.bottom_i32() - 1, rect.width, 1),
                    palette.border,
                );
            }
        }
        self.painter.restore_clip(old_clip);

        let max_scroll = (content_height - body.height as i32).max(0);
        offset = offset.clamp(0, max_scroll);
        self.state.set_scroll(id, offset, content_height);
        if content_height > body.height as i32 && body.height > 0 {
            let bar_width = metrics.scrollbar_width;
            let track = Rect::new(
                body.right_i32() - bar_width - 2,
                body.y + 2,
                bar_width as u32,
                body.height.saturating_sub(4),
            );
            let thumb_height =
                (track.height as f32 * body.height as f32 / content_height as f32) as i32;
            let thumb_height = thumb_height.clamp(
                metrics.min_control_size.min(track.height as i32),
                track.height as i32,
            );
            let bar = self.interact(id.child(0x5c), track, Sense::drag());
            if bar.active && self.pointer_down() {
                let travel = (track.height as i32 - thumb_height).max(1);
                let position = (self.pointer().y - track.y - thumb_height / 2).clamp(0, travel);
                offset = (position as f32 / travel as f32 * max_scroll as f32) as i32;
                self.state.set_scroll(id, offset, content_height);
            }
            self.painter
                .fill_rounded_rect(track, (bar_width / 2) as u32, palette.scrollbar_track);
            let thumb_y = track.y
                + ((track.height as i32 - thumb_height) as f32
                    * (offset as f32 / max_scroll.max(1) as f32)) as i32;
            self.painter.fill_rounded_rect(
                Rect::new(track.x, thumb_y, track.width, thumb_height.max(1) as u32),
                (bar_width / 2) as u32,
                if bar.hovered || bar.active {
                    palette.scrollbar_hover
                } else {
                    palette.scrollbar_thumb
                },
            );
        }

        TableResponse {
            response,
            header_clicked,
        }
    }

    /// One label/value row. Values are right-aligned.
    pub fn key_value(&mut self, label: &str, value: &str) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(0, metrics.row_height);
        self.painter.draw_text_ellipsis(
            Rect::new(
                rect.x,
                rect.y,
                (rect.width as i32 / 2).max(0) as u32,
                rect.height,
            ),
            label,
            palette.text_muted,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        self.painter.draw_text_ellipsis(
            Rect::new(
                rect.x + rect.width as i32 / 2,
                rect.y,
                rect.width / 2,
                rect.height,
            ),
            value,
            palette.text,
            metrics.font_scale,
            Align::End,
            VerticalAlign::Middle,
        );
        rect
    }

    /// A compact key/value list inside `rect`.
    pub fn key_value_list(&mut self, rect: Rect, rows: &[(&str, &str)]) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let old_clip = self.painter.push_clip(rect);
        for (index, (label, value)) in rows.iter().enumerate() {
            let row = Rect::new(
                rect.x,
                rect.y + index as i32 * metrics.row_height,
                rect.width,
                metrics.row_height as u32,
            );
            if row.y >= rect.bottom_i32() {
                break;
            }
            self.painter.draw_text_ellipsis(
                Rect::new(
                    row.x,
                    row.y,
                    (row.width as i32 / 2).max(0) as u32,
                    row.height,
                ),
                label,
                palette.text_muted,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(
                    row.x + row.width as i32 / 2,
                    row.y,
                    row.width / 2,
                    row.height,
                ),
                value,
                palette.text,
                metrics.font_scale,
                Align::End,
                VerticalAlign::Middle,
            );
            if index + 1 < rows.len() {
                self.painter.fill_rect(
                    Rect::new(row.x, row.bottom_i32() - 1, row.width, 1),
                    palette.border,
                );
            }
        }
        self.painter.restore_clip(old_clip);
        rect
    }

    /// A vertical timeline.
    pub fn timeline(&mut self, rect: Rect, items: &[TimelineItem<'_>]) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let item_height = 50;
        let old_clip = self.painter.push_clip(rect);
        let x = rect.x + 10;
        for (index, item) in items.iter().enumerate() {
            let y = rect.y + index as i32 * item_height;
            if y >= rect.bottom_i32() {
                break;
            }
            let color = item.tone.color(palette);
            if index + 1 < items.len() {
                self.painter
                    .vline(x, y + 10, y + item_height + 10, palette.border);
            }
            self.painter.fill_circle(x, y + 8, 4, color);
            self.painter.draw_text_ellipsis(
                Rect::new(
                    x + 16,
                    y,
                    rect.width.saturating_sub(26),
                    metrics.font_scale * 8,
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
                        x + 16,
                        y + metrics.font_scale as i32 * 9,
                        rect.width.saturating_sub(26),
                        metrics.small_scale * 8,
                    ),
                    item.detail,
                    palette.text_muted,
                    metrics.small_scale,
                    Align::Start,
                    VerticalAlign::Top,
                );
            }
        }
        self.painter.restore_clip(old_clip);
        rect
    }

    /// A wheel-scrollable tree view.
    ///
    /// `expanded` is indexed by row, so the caller decides which rows are
    /// visible and uses the same flags to toggle children.
    pub fn tree(
        &mut self,
        rect: Rect,
        rows: &[TreeRow<'_>],
        expanded: &mut [bool],
        selected: &mut usize,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let content_height = rows.len() as i32 * metrics.row_height;
        let previous = self.state.scroll_content(id);
        let max_scroll = (previous - rect.height as i32).max(0);
        let mut offset = self.state.scroll_offset(id);
        if self.pointer_over(rect) && self.wheel() != 0 {
            offset = (offset - self.wheel() * metrics.row_height).clamp(0, max_scroll);
        }

        self.painter
            .fill_rounded_rect(rect, metrics.radius.min(8), palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius.min(8),
            metrics.border_width,
            palette.border,
        );
        let old_clip = self.painter.push_clip(rect);
        for (index, row) in rows.iter().enumerate() {
            let y = rect.y + index as i32 * metrics.row_height - offset;
            let row_rect = Rect::new(rect.x, y, rect.width, metrics.row_height as u32);
            if row_rect.bottom_i32() < rect.y || row_rect.y >= rect.bottom_i32() {
                continue;
            }
            let response_row = self.interact(id.child(index as u64 + 1), row_rect, Sense::click());
            if response_row.clicked && *selected != index {
                *selected = index;
                response.changed = true;
                response.clicked = true;
            }
            if *selected == index {
                self.painter.fill_rect(row_rect, palette.surface_active);
            } else if response_row.hovered {
                self.painter.fill_rect(row_rect, palette.surface_hover);
            }
            let indent = 8 + row.depth as i32 * 14;
            let chevron = Rect::new(rect.x + indent, y + (metrics.row_height - 14) / 2, 14, 14);
            let mut label_x = rect.x + indent + 18;
            if row.has_children {
                let toggle = self.interact(
                    id.child(0x2000 + index as u64),
                    chevron,
                    Sense::HOVER.with(Sense::CLICK),
                );
                if toggle.clicked {
                    if let Some(slot) = expanded.get_mut(index) {
                        *slot = !*slot;
                    }
                    response.changed = true;
                    response.clicked = true;
                }
                paint_icon(
                    &mut self.painter,
                    chevron.inset(1),
                    if expanded.get(index).copied().unwrap_or(false) {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    },
                    palette.text_muted,
                );
            } else {
                label_x = rect.x + indent + 4;
            }
            self.painter.draw_text_ellipsis(
                Rect::new(
                    label_x,
                    y,
                    (rect.right_i32() - label_x - metrics.padding_x).max(0) as u32,
                    metrics.row_height as u32,
                ),
                row.label,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
        }
        self.painter.restore_clip(old_clip);
        let max_scroll = (content_height - rect.height as i32).max(0);
        offset = offset.clamp(0, max_scroll);
        self.state.set_scroll(id, offset, content_height);
        response
    }

    /// A tree view backed by [`TreeState`].
    ///
    /// This is the convenience form of [`tree`](Ui::tree) for apps that do not
    /// need a caller-owned expansion slice.
    pub fn tree_state<const M: usize>(
        &mut self,
        rect: Rect,
        rows: &[TreeRow<'_>],
        state: &mut TreeState<M>,
    ) -> Response {
        let mut selected = state.selected;
        let response = self.tree(rect, rows, state.expanded_mut(), &mut selected);
        state.selected = selected;
        response
    }

    /// A stat card with a label, large value, detail and tone accent.
    pub fn stat_card(
        &mut self,
        rect: Rect,
        label: &str,
        value: &str,
        detail: &str,
        tone: Tone,
    ) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );
        self.painter.fill_rect(
            Rect::new(
                rect.x + metrics.padding_x,
                rect.y + metrics.padding_y,
                24,
                3,
            ),
            tone.color(palette),
        );
        let inner = Rect::new(
            rect.x + metrics.padding_x,
            rect.y + metrics.padding_y + 8,
            rect.width.saturating_sub((metrics.padding_x * 2) as u32),
            rect.height
                .saturating_sub((metrics.padding_y * 2 + 8) as u32),
        );
        self.painter.draw_text_ellipsis(
            Rect::new(inner.x, inner.y, inner.width, 12),
            label,
            palette.text_muted,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Top,
        );
        self.painter.draw_text_ellipsis(
            Rect::new(inner.x, inner.y + 12, inner.width, 28),
            value,
            palette.text,
            metrics.heading_scale,
            Align::Start,
            VerticalAlign::Top,
        );
        if !detail.is_empty() {
            self.painter.draw_text_ellipsis(
                Rect::new(inner.x, inner.bottom_i32() - 12, inner.width, 12),
                detail,
                tone.color(palette),
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Bottom,
            );
        }
        rect
    }

    /// A circular avatar with initials.
    pub fn avatar(&mut self, initials: &str, color: Color, size: i32) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let size = size.max(16);
        let rect = self.allocate_sized(size, size);
        self.painter
            .fill_circle(rect.center().x, rect.center().y, (size / 2) as u32, color);
        self.painter.circle(
            rect.center().x,
            rect.center().y,
            (size / 2) as u32,
            color.lerp(Color::WHITE, 40),
        );
        self.painter.draw_text_centered(
            rect,
            initials,
            palette.accent_text,
            if size >= 32 {
                metrics.font_scale
            } else {
                metrics.small_scale
            },
        );
        rect
    }

    /// A removable tag/chip.
    pub fn chip(&mut self, label: &str, color: Color, removable: bool) -> ChipResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let remove_space = if removable { 20 } else { 0 };
        let width = self
            .painter
            .text_width(label, metrics.small_scale)
            .saturating_add(metrics.padding_x * 2 + 12 + remove_space);
        let rect = self.allocate(width, metrics.row_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        let mut removed = false;
        self.painter.fill_rounded_rect(
            rect,
            (metrics.row_height / 2) as u32,
            palette.surface_alt.lerp(color, 28),
        );
        self.painter.stroke_rounded_rect(
            rect,
            (metrics.row_height / 2) as u32,
            metrics.border_width,
            color,
        );
        let dot_x = rect.x + metrics.padding_x / 2 + 4;
        self.painter.fill_circle(dot_x, rect.center().y, 3, color);
        let text_x = dot_x + 8;
        self.painter.draw_text_ellipsis(
            Rect::new(
                text_x,
                rect.y,
                (rect.right_i32() - text_x - 4 - remove_space).max(0) as u32,
                rect.height,
            ),
            label,
            palette.text,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        if removable {
            let close_rect = Rect::new(
                rect.right_i32() - 18,
                rect.y + (rect.height as i32 - 14) / 2,
                14,
                14,
            );
            let close = self.interact(id.child(0xc1), close_rect, Sense::HOVER.with(Sense::CLICK));
            paint_icon(
                &mut self.painter,
                close_rect.inset(1),
                Icon::Close,
                if close.hovered {
                    palette.text
                } else {
                    palette.text_muted
                },
            );
            if close.clicked {
                removed = true;
                response.changed = true;
                response.clicked = true;
            }
        }
        if response.hovered {
            self.painter.stroke_rounded_rect(
                rect.inset(1),
                (metrics.row_height / 2) as u32,
                1,
                palette.focus,
            );
        }
        ChipResponse { response, removed }
    }

    /// A small notification badge.
    ///
    /// A zero count renders a dot; a positive count renders a compact pill
    /// with the number. The badge is sized by its content.
    pub fn info_badge(&mut self, count: u32, tone: Tone) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let mut buffer = [0u8; 10];
        let label = if count == 0 {
            ""
        } else {
            format_u32(count.min(999), &mut buffer)
        };
        let height = 18;
        let width = if count == 0 {
            height
        } else {
            self.painter
                .text_width(label, metrics.small_scale)
                .saturating_add(12)
                .max(height)
        };
        let rect = self.allocate_sized(width, height);
        let color = tone.color(palette);
        if count == 0 {
            self.painter
                .fill_circle(rect.center().x, rect.center().y, 4, color);
        } else {
            self.painter
                .fill_rounded_rect(rect, (height / 2) as u32, color);
            self.painter
                .draw_text_centered(rect, label, palette.background, metrics.small_scale);
        }
        rect
    }

    /// A person row: avatar, name and supporting detail.
    ///
    /// Initials are derived from the first letters of the first two words.
    pub fn persona(&mut self, name: &str, detail: &str, color: Color, size: i32) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let size = size.max(24);
        let detail_width = if detail.is_empty() {
            0
        } else {
            self.painter.text_width(detail, metrics.small_scale)
        };
        let name_width = self.painter.text_width(name, metrics.font_scale);
        let width = size
            .saturating_add(metrics.gap * 2)
            .saturating_add(name_width.max(detail_width));
        let rect = self.allocate(width, size.max(metrics.control_height));
        let avatar = Rect::new(
            rect.x,
            rect.y + (rect.height as i32 - size) / 2,
            size as u32,
            size as u32,
        );
        self.painter.fill_circle(
            avatar.center().x,
            avatar.center().y,
            (size / 2) as u32,
            color,
        );
        let (initials, length) = initials(name);
        let label = core::str::from_utf8(&initials[..length]).unwrap_or("?");
        self.painter.draw_text_centered(
            avatar,
            label,
            palette.accent_text,
            if size >= 32 {
                metrics.font_scale
            } else {
                metrics.small_scale
            },
        );
        let text_x = avatar.right_i32() + metrics.gap;
        let text_width = (rect.right_i32() - text_x).max(0) as u32;
        if detail.is_empty() {
            self.painter.draw_text_ellipsis(
                Rect::new(text_x, rect.y, text_width, rect.height),
                name,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
        } else {
            let line = self.painter.line_height(metrics.font_scale);
            self.painter.draw_text_ellipsis(
                Rect::new(text_x, rect.y + 2, text_width, line as u32),
                name,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Top,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(text_x, rect.y + line + 4, text_width, line as u32),
                detail,
                palette.text_muted,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Top,
            );
        }
        rect
    }
}

fn initials(name: &str) -> ([u8; 3], usize) {
    let mut buffer = [b'?'; 3];
    let mut length = 0;
    for word in name.split_whitespace().take(2) {
        if let Some(ch) = word.chars().next() {
            let upper = ch.to_ascii_uppercase();
            if upper.is_ascii() {
                buffer[length] = upper as u8;
                length += 1;
            }
        }
    }
    if length == 0 {
        buffer[0] = b'?';
        length = 1;
    }
    (buffer, length)
}
