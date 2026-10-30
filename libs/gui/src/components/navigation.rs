//! Navigation components: toolbars, status bars, breadcrumbs, pagination and
//! step indicators.

use feos_gfx::Rect;

use crate::geometry::{Align, Padding, RectExt, VerticalAlign};
use crate::input::Key;
use crate::layout::Axis;
use crate::response::{Response, Sense};
use crate::text::format_f32;
use crate::ui::Ui;

use super::icons::{paint_icon, Icon};
use super::inputs::{MenuEntry, MenuState};

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A horizontal toolbar with a raised surface and inset padding.
    pub fn toolbar<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Self) -> R) -> R {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface_alt);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );
        let inner = Padding::symmetric(metrics.padding_x / 2, metrics.padding_y / 2).apply(rect);
        let (_, value) = self.row_with(inner, metrics.gap, f);
        value
    }

    /// A status strip with left and right aligned text.
    pub fn status_bar(&mut self, rect: Rect, left: &str, right: &str) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        self.painter.fill_rect(rect, palette.surface_alt);
        self.painter
            .fill_rect(Rect::new(rect.x, rect.y, rect.width, 1), palette.border);
        let inner = Rect::new(
            rect.x + metrics.padding_x,
            rect.y + 1,
            rect.width.saturating_sub((metrics.padding_x * 2) as u32),
            rect.height.saturating_sub(1),
        );
        let gap = metrics.gap;
        let right_width = self
            .painter
            .text_width(right, metrics.small_scale)
            .min(inner.width as i32);
        self.painter.draw_text_ellipsis(
            Rect::new(
                inner.x,
                inner.y,
                (inner.width as i32 - right_width - gap).max(0) as u32,
                inner.height,
            ),
            left,
            palette.text_muted,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        if right_width > 0 {
            self.painter.draw_text_ellipsis(
                Rect::new(
                    inner.right_i32() - right_width,
                    inner.y,
                    right_width as u32,
                    inner.height,
                ),
                right,
                palette.text_muted,
                metrics.small_scale,
                Align::End,
                VerticalAlign::Middle,
            );
        }
        rect
    }

    /// A clickable breadcrumb trail. Clicking a crumb sets `current`.
    pub fn breadcrumbs(&mut self, items: &[&str], current: &mut usize) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let separator = 14;
        let width = items
            .iter()
            .map(|item| {
                self.painter
                    .text_width(item, metrics.font_scale)
                    .saturating_add(metrics.padding_x)
            })
            .sum::<i32>()
            + separator * items.len().saturating_sub(1) as i32;
        let rect = self.allocate(width.max(1), metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        if items.is_empty() {
            return response;
        }
        let mut x = rect.x;
        for (index, item) in items.iter().enumerate() {
            let item_width = self
                .painter
                .text_width(item, metrics.font_scale)
                .saturating_add(metrics.padding_x);
            let crumb = Rect::new(x, rect.y, item_width.max(0) as u32, rect.height);
            let crumb_response = self.interact(id.child(index as u64 + 1), crumb, Sense::click());
            if crumb_response.clicked && *current != index {
                *current = index;
                response.changed = true;
                response.clicked = true;
            }
            if crumb_response.hovered {
                self.painter
                    .fill_rounded_rect(crumb.inset_xy(1, 3), 4, palette.surface_hover);
            }
            self.painter.draw_text_ellipsis(
                crumb,
                item,
                if *current == index || crumb_response.hovered {
                    palette.text
                } else {
                    palette.text_muted
                },
                metrics.font_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
            x += item_width;
            if index + 1 < items.len() {
                paint_icon(
                    &mut self.painter,
                    Rect::new(x + 2, rect.y + (rect.height as i32 - 12) / 2, 12, 12),
                    Icon::ChevronRight,
                    palette.text_disabled,
                );
                x += separator;
            }
        }
        response
    }

    /// A pagination control with previous/next arrows and page numbers.
    pub fn pagination(&mut self, page: &mut usize, pages: usize) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let total = pages.max(1);
        let current = (*page).min(total - 1);
        *page = current;

        let mut slots = [None; 7];
        let mut len = 0;
        if total <= 7 {
            for index in 0..total {
                slots[len] = Some(index);
                len += 1;
            }
        } else {
            slots[len] = Some(0);
            len += 1;
            let start = current.saturating_sub(1).max(1);
            let end = (start + 2).min(total - 2);
            if start > 1 {
                slots[len] = None;
                len += 1;
            }
            for index in start..=end {
                slots[len] = Some(index);
                len += 1;
            }
            if end < total - 2 {
                slots[len] = None;
                len += 1;
            }
            slots[len] = Some(total - 1);
            len += 1;
        }

        let button = metrics.control_height;
        let gap = 4;
        let width = (len + 2) as i32 * (button + gap);
        let rect = self.allocate(width, button);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);

        let prev = Rect::new(rect.x, rect.y, button as u32, button as u32);
        if self.page_button(prev, id.child(1), Icon::ChevronLeft, "", current == 0) && *page > 0 {
            *page -= 1;
            response.changed = true;
            response.clicked = true;
        }

        let mut x = prev.right_i32() + gap;
        let mut buffer = [0u8; 16];
        for (index, slot) in slots[..len].iter().enumerate() {
            let cell = Rect::new(x, rect.y, button as u32, button as u32);
            match *slot {
                Some(number) => {
                    let text = format_f32((number + 1) as f32, 0, &mut buffer);
                    let active = number == current;
                    if self.page_button(cell, id.child(2 + index as u64), Icon::Info, text, active)
                        && *page != number
                    {
                        *page = number;
                        response.changed = true;
                        response.clicked = true;
                    }
                }
                None => {
                    self.painter.draw_text_centered(
                        cell,
                        "...",
                        palette.text_disabled,
                        metrics.small_scale,
                    );
                }
            }
            x += button + gap;
        }

        let next = Rect::new(x, rect.y, button as u32, button as u32);
        if self.page_button(
            next,
            id.child(0xff),
            Icon::ChevronRight,
            "",
            current + 1 >= total,
        ) && *page + 1 < total
        {
            *page += 1;
            response.changed = true;
            response.clicked = true;
        }
        response
    }

    fn page_button(
        &mut self,
        rect: Rect,
        id: crate::Id,
        icon: Icon,
        text: &str,
        active: bool,
    ) -> bool {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let response = self.interact(id, rect, Sense::click());
        if active && !text.is_empty() {
            self.painter
                .fill_rounded_rect(rect, metrics.radius.min(5), palette.accent);
        } else if response.hovered {
            self.painter
                .fill_rounded_rect(rect, metrics.radius.min(5), palette.surface_hover);
        } else {
            self.painter
                .fill_rounded_rect(rect, metrics.radius.min(5), palette.surface_alt);
        }
        if response.focused {
            self.painter.stroke_rounded_rect(
                rect.inset(1),
                metrics.radius.min(5),
                1,
                palette.focus,
            );
        }
        if text.is_empty() {
            paint_icon(
                &mut self.painter,
                rect.inset(6),
                icon,
                if active {
                    palette.text_disabled
                } else {
                    palette.text
                },
            );
        } else {
            self.painter.draw_text_ellipsis(
                rect,
                text,
                if active {
                    palette.accent_text
                } else {
                    palette.text
                },
                metrics.font_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
        }
        response.clicked
    }

    /// A horizontal step indicator. Clicking a step jumps to it.
    pub fn steps(&mut self, current: &mut usize, labels: &[&str]) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        if labels.is_empty() {
            return Response::empty(crate::Id::NONE, Rect::new(0, 0, 0, 0));
        }
        let line_height = self.painter.line_height(metrics.small_scale);
        let circle = 24;
        let height = circle.max(metrics.control_height) + 6 + line_height;
        let width = if self.axis() == Axis::Vertical {
            0
        } else {
            labels.len() as i32 * 120
        };
        let rect = self.allocate(width, height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let step_width = rect.width as i32 / labels.len() as i32;
        let center_y = rect.y + circle / 2;

        for index in 0..labels.len() {
            let center_x = rect.x + step_width * index as i32 + step_width / 2;
            let active = index <= *current;
            let is_current = index == *current;
            if index + 1 < labels.len() {
                let next_x = rect.x + step_width * (index + 1) as i32 + step_width / 2;
                self.painter.hline(
                    center_x + circle / 2,
                    next_x - circle / 2,
                    center_y,
                    if index < *current {
                        palette.accent
                    } else {
                        palette.border
                    },
                );
            }
            let circle_rect = Rect::new(
                center_x - circle / 2,
                center_y - circle / 2,
                circle as u32,
                circle as u32,
            );
            let step_response =
                self.interact(id.child(index as u64 + 1), circle_rect, Sense::click());
            if step_response.clicked && *current != index {
                *current = index;
                response.changed = true;
                response.clicked = true;
            }
            let fill = if is_current {
                palette.accent
            } else if index < *current {
                palette.success
            } else {
                palette.surface_alt
            };
            self.painter
                .fill_circle(center_x, center_y, (circle / 2) as u32, fill);
            if !active || is_current {
                self.painter.circle(
                    center_x,
                    center_y,
                    (circle / 2) as u32,
                    if is_current {
                        palette.focus
                    } else {
                        palette.border_strong
                    },
                );
            }
            if index < *current {
                paint_icon(
                    &mut self.painter,
                    circle_rect.inset(5),
                    Icon::Check,
                    palette.accent_text,
                );
            } else {
                let mut buffer = [0u8; 16];
                let text = format_f32((index + 1) as f32, 0, &mut buffer);
                self.painter.draw_text_centered(
                    circle_rect,
                    text,
                    if is_current {
                        palette.accent_text
                    } else {
                        palette.text_muted
                    },
                    metrics.font_scale,
                );
            }
            let label_x = (center_x - step_width / 2).max(rect.x);
            let label_width = step_width.max(1) as u32;
            self.painter.draw_text_ellipsis(
                Rect::new(
                    label_x,
                    center_y + circle / 2 + 4,
                    label_width,
                    line_height as u32,
                ),
                labels[index],
                if is_current {
                    palette.text
                } else {
                    palette.text_muted
                },
                metrics.small_scale,
                Align::Center,
                VerticalAlign::Top,
            );
        }
        response
    }
}

/// One item in a [`Ui::navigation_view`] pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavItem<'a> {
    /// Leading icon.
    pub icon: Icon,
    /// Item label, hidden when the pane is collapsed.
    pub label: &'a str,
    /// Optional notification dot. Drawn when non-zero.
    pub badge: u32,
    /// Disabled items are dimmed and cannot be selected.
    pub enabled: bool,
}

impl<'a> NavItem<'a> {
    pub const fn new(icon: Icon, label: &'a str) -> Self {
        Self {
            icon,
            label,
            badge: 0,
            enabled: true,
        }
    }

    pub const fn with_badge(self, badge: u32) -> Self {
        Self { badge, ..self }
    }

    pub const fn disabled(self) -> Self {
        Self {
            enabled: false,
            ..self
        }
    }
}

/// Persistent state for [`Ui::navigation_view`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NavigationViewState {
    /// Selected item index.
    pub selected: usize,
    /// Whether the pane is expanded. When `false`, it becomes a narrow rail.
    pub pane_open: bool,
}

impl NavigationViewState {
    pub const fn new() -> Self {
        Self {
            selected: 0,
            pane_open: true,
        }
    }
}

/// The result of [`Ui::navigation_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavigationViewResponse {
    /// Interaction with the pane header (hamburger and items).
    pub response: Response,
    /// Item selected by a click this frame.
    pub selected: Option<usize>,
    /// The pane was collapsed or expanded this frame.
    pub pane_toggled: bool,
    /// The content area left for the app.
    pub content: Rect,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A WinUI-style navigation pane plus content.
    ///
    /// The pane has a hamburger button, a title, icon items and a footer. When
    /// `state.pane_open` is `false` only the icons remain. `f` runs in the
    /// content area and receives its rectangle, so it can build any layout.
    pub fn navigation_view<R>(
        &mut self,
        state: &mut NavigationViewState,
        rect: Rect,
        title: &str,
        items: &[NavItem<'_>],
        f: impl FnOnce(&mut Self, Rect) -> R,
    ) -> (NavigationViewResponse, R) {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let mut selected = None;
        let mut pane_toggled = false;

        let expanded = 210.min(rect.width as i32 * 2 / 5);
        let pane_width = if state.pane_open {
            expanded
        } else {
            metrics.control_height + 22
        }
        .max(0)
        .min(rect.width as i32);
        let (pane, content) = rect.split_left(pane_width);

        self.painter.fill_rect(pane, palette.surface_alt);
        self.painter.fill_rect(
            Rect::new(pane.right_i32() - 1, pane.y, 1, pane.height),
            palette.border,
        );

        let toggle_size = metrics.control_height - 4;
        let toggle = Rect::new(
            pane.x + 8,
            pane.y + 8,
            toggle_size as u32,
            toggle_size as u32,
        );
        let toggle_hit = self.interact(id.child(1), toggle, Sense::click());
        if toggle_hit.hovered || toggle_hit.focused {
            self.painter
                .fill_rounded_rect(toggle.inset(2), metrics.radius, palette.surface_hover);
        }
        paint_icon(
            &mut self.painter,
            toggle.inset(6),
            if state.pane_open {
                Icon::ChevronLeft
            } else {
                Icon::Menu
            },
            palette.text,
        );
        if toggle_hit.clicked {
            state.pane_open = !state.pane_open;
            pane_toggled = true;
            response.changed = true;
            response.clicked = true;
        }
        if state.pane_open {
            self.painter.draw_text_ellipsis(
                Rect::new(
                    toggle.right_i32() + 8,
                    pane.y,
                    (pane.right_i32() - toggle.right_i32() - 16).max(0) as u32,
                    toggle.height + 16,
                ),
                title,
                palette.text,
                metrics.heading_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
        }

        let start_y = pane.y + toggle.height as i32 + 22;
        let item_height = metrics.control_height + 6;
        let footer = 24;
        for (index, item) in items.iter().enumerate() {
            let y = start_y + index as i32 * item_height;
            if y + item_height > pane.bottom_i32() - footer {
                break;
            }
            let item_rect = Rect::new(
                pane.x + 8,
                y,
                pane.width.saturating_sub(16),
                item_height as u32,
            );
            let hit = if item.enabled {
                Some(self.interact(
                    id.child(10 + index as u64),
                    item_rect,
                    Sense::HOVER.with(Sense::CLICK),
                ))
            } else {
                None
            };
            let active = state.selected == index;
            if active {
                self.painter.fill_rounded_rect(
                    item_rect.inset(1),
                    metrics.radius,
                    palette.surface_active,
                );
                self.painter.fill_rounded_rect(
                    Rect::new(
                        item_rect.x + 1,
                        item_rect.y + 8,
                        3,
                        item_rect.height.saturating_sub(16),
                    ),
                    1,
                    palette.accent,
                );
            } else if hit.as_ref().is_some_and(|hit| hit.hovered) {
                self.painter.fill_rounded_rect(
                    item_rect.inset(1),
                    metrics.radius,
                    palette.surface_hover,
                );
            }
            let icon_size = 18;
            let icon_x = item_rect.x
                + if state.pane_open {
                    metrics.padding_x / 2
                } else {
                    6
                };
            paint_icon(
                &mut self.painter,
                Rect::new(
                    icon_x,
                    item_rect.y + (item_height - icon_size) / 2,
                    icon_size as u32,
                    icon_size as u32,
                ),
                item.icon,
                if !item.enabled {
                    palette.text_disabled
                } else if active {
                    palette.accent
                } else {
                    palette.text_muted
                },
            );
            if state.pane_open {
                self.painter.draw_text_ellipsis(
                    Rect::new(
                        icon_x + icon_size + metrics.gap,
                        item_rect.y,
                        (item_rect.right_i32() - icon_x - icon_size - metrics.gap - 20).max(0)
                            as u32,
                        item_rect.height,
                    ),
                    item.label,
                    if item.enabled {
                        palette.text
                    } else {
                        palette.text_disabled
                    },
                    metrics.font_scale,
                    Align::Start,
                    VerticalAlign::Middle,
                );
            }
            let dot = if item.badge > 0 { 6 } else { 0 };
            if dot > 0 {
                self.painter.fill_circle(
                    item_rect.right_i32() - 12,
                    item_rect.center().y,
                    dot as u32,
                    palette.accent,
                );
            }
            if hit.is_some_and(|hit| hit.clicked) && state.selected != index {
                state.selected = index;
                selected = Some(index);
                response.changed = true;
                response.clicked = true;
            }
        }
        if state.pane_open {
            self.painter.draw_text_ellipsis(
                Rect::new(
                    pane.x + 12,
                    pane.bottom_i32() - footer,
                    pane.width.saturating_sub(24),
                    footer as u32,
                ),
                "feos-gui navigation",
                palette.text_muted,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
        }

        let content = content.inset(metrics.padding_x);
        let clip = self.painter.push_clip(content);
        let (_, value) = self.column_with(content, metrics.gap, |ui| f(ui, content));
        self.painter.restore_clip(clip);

        (
            NavigationViewResponse {
                response,
                selected,
                pane_toggled,
                content,
            },
            value,
        )
    }
}

/// Persistent state for [`Ui::tab_view`].
///
/// Tabs are stored as `&'static str` so the state is fixed-capacity and
/// allocation-free. `M` is the maximum number of tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabViewState<const M: usize = 8> {
    titles: [&'static str; M],
    len: usize,
    /// Active tab.
    pub selected: usize,
    /// Show a close button on every tab.
    pub closable: bool,
    /// Show a trailing `+` button.
    pub addable: bool,
}

impl<const M: usize> TabViewState<M> {
    pub const fn new() -> Self {
        Self {
            titles: [""; M],
            len: 0,
            selected: 0,
            closable: true,
            addable: true,
        }
    }

    /// Builds a state from a fixed list of titles.
    ///
    /// This keeps fully static demo and app state possible without a runtime
    /// `push` loop.
    pub const fn from_titles(titles: [&'static str; M]) -> Self {
        Self {
            titles,
            len: M,
            selected: 0,
            closable: true,
            addable: true,
        }
    }

    /// Adds a tab. Returns `false` when the fixed capacity is full.
    pub fn push(&mut self, title: &'static str) -> bool {
        if self.len >= M {
            return false;
        }
        self.titles[self.len] = title;
        self.len += 1;
        true
    }

    /// Removes a tab, shifting the rest left and clamping the selection.
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        for slot in index..self.len.saturating_sub(1) {
            self.titles[slot] = self.titles[slot + 1];
        }
        self.len = self.len.saturating_sub(1);
        self.titles[self.len] = "";
        self.selected = self.selected.min(self.len.saturating_sub(1));
        true
    }

    /// Number of tabs.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when there are no tabs.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Title of tab `index`, or an empty string.
    pub fn title(&self, index: usize) -> &'static str {
        self.titles.get(index).copied().unwrap_or("")
    }

    /// Changes a tab title.
    pub fn set_title(&mut self, index: usize, title: &'static str) {
        if let Some(slot) = self.titles.get_mut(index) {
            *slot = title;
        }
    }
}

impl<const M: usize> Default for TabViewState<M> {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of [`Ui::tab_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabViewResponse {
    /// Interaction with the tab strip.
    pub response: Response,
    /// Tab closed this frame.
    pub closed: Option<usize>,
    /// The `+` button was pressed.
    pub added: bool,
    /// The content area below the strip.
    pub content: Rect,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A tab strip with optional close buttons, a `+` button and a content
    /// panel. `f` runs inside the content area.
    pub fn tab_view<const M: usize>(
        &mut self,
        state: &mut TabViewState<M>,
        rect: Rect,
        f: impl FnOnce(&mut Self, Rect),
    ) -> TabViewResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let mut closed = None;
        let mut added = false;

        let strip_height = metrics.control_height + 8;
        let (strip, content) = rect.split_top(strip_height);
        self.painter.fill_rect(strip, palette.surface_alt);
        self.painter.fill_rect(
            Rect::new(strip.x, strip.bottom_i32() - 1, strip.width, 1),
            palette.border,
        );

        let mut x = strip.x + 6;
        let close_size = (metrics.control_height - 16).max(10);
        for index in 0..state.len() {
            let title = state.title(index);
            let width = self
                .painter
                .text_width(title, metrics.font_scale)
                .saturating_add(metrics.padding_x * 2)
                .saturating_add(if state.closable { close_size + 8 } else { 0 })
                .max(72);
            let tab = Rect::new(
                x,
                strip.y + 5,
                width.max(0) as u32,
                (strip_height - 5) as u32,
            );
            if tab.x >= strip.right_i32() {
                break;
            }
            let active = state.selected == index;
            let hit = self.interact(
                id.child(index as u64 + 1),
                tab,
                Sense::HOVER.with(Sense::CLICK),
            );
            if hit.clicked && state.selected != index {
                state.selected = index;
                response.changed = true;
                response.clicked = true;
            }
            self.painter.fill_rounded_rect(
                tab,
                metrics.radius,
                if active {
                    palette.surface
                } else if hit.hovered {
                    palette.surface_hover
                } else {
                    palette.surface_alt
                },
            );
            if active {
                self.painter.fill_rounded_rect(
                    Rect::new(
                        tab.x + 8,
                        tab.bottom_i32() - 3,
                        tab.width.saturating_sub(16),
                        2,
                    ),
                    1,
                    palette.accent,
                );
            }
            let close_space = if state.closable { close_size + 8 } else { 0 };
            self.painter.draw_text_ellipsis(
                Rect::new(
                    tab.x + metrics.padding_x,
                    tab.y,
                    tab.width
                        .saturating_sub((metrics.padding_x * 2 + close_space) as u32),
                    tab.height,
                ),
                title,
                if active {
                    palette.text
                } else {
                    palette.text_muted
                },
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            if state.closable {
                let close = Rect::new(
                    tab.right_i32() - close_size - 6,
                    tab.y + (tab.height as i32 - close_size) / 2,
                    close_size as u32,
                    close_size as u32,
                );
                let close_hit = self.interact(
                    id.child(100 + index as u64),
                    close,
                    Sense::HOVER.with(Sense::CLICK),
                );
                if close_hit.hovered {
                    self.painter
                        .fill_rounded_rect(close, 3, palette.surface_active);
                }
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
                    closed = Some(index);
                }
            }
            x += tab.width as i32 + 4;
        }

        if state.addable {
            let add = Rect::new(
                x,
                strip.y + 5,
                metrics.control_height as u32,
                (strip_height - 5) as u32,
            );
            if add.x + add.width as i32 <= strip.right_i32() {
                let add_hit = self.interact(id.child(0xadd), add, Sense::click());
                if add_hit.hovered {
                    self.painter
                        .fill_rounded_rect(add.inset(3), 4, palette.surface_hover);
                }
                paint_icon(
                    &mut self.painter,
                    add.inset(metrics.control_height / 3),
                    Icon::Plus,
                    palette.text_muted,
                );
                if add_hit.clicked && state.push("New tab") {
                    state.selected = state.len().saturating_sub(1);
                    added = true;
                    response.changed = true;
                    response.clicked = true;
                }
            }
        }

        if let Some(index) = closed {
            state.remove(index);
            response.changed = true;
            response.clicked = true;
        }

        self.painter.fill_rect(content, palette.surface);
        self.painter
            .stroke_rect(content, metrics.border_width, palette.border);
        let inner = Padding::all(metrics.padding_x).apply(content);
        let clip = self.painter.push_clip(inner);
        self.column_with(inner, metrics.gap, |ui| f(ui, inner));
        self.painter.restore_clip(clip);

        TabViewResponse {
            response,
            closed,
            added,
            content: inner,
        }
    }
}

/// One top-level menu in a [`Ui::menu_bar`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuBarMenu<'a> {
    /// Menu label shown in the bar.
    pub label: &'a str,
    /// Commands in the dropdown.
    pub entries: &'a [MenuEntry<'a>],
}

impl<'a> MenuBarMenu<'a> {
    pub const fn new(label: &'a str, entries: &'a [MenuEntry<'a>]) -> Self {
        Self { label, entries }
    }
}

/// Persistent state for [`Ui::menu_bar`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MenuBarState {
    /// Index of the open menu.
    pub open: Option<usize>,
    /// Popup highlight state shared by the open menu.
    pub menu: MenuState,
}

impl MenuBarState {
    pub const fn new() -> Self {
        Self {
            open: None,
            menu: MenuState::new(),
        }
    }
}

/// The result of [`Ui::menu_bar`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuBarResponse {
    /// Interaction with the bar itself.
    pub response: Response,
    /// `(menu index, entry index)` chosen this frame.
    pub selected: Option<(usize, usize)>,
    /// A menu was dismissed.
    pub dismissed: bool,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A classic desktop menu bar (`File`, `Edit`, `View`, ...).
    ///
    /// The bar spans the available width in a column. Clicking a label opens
    /// its dropdown; clicking another label switches menus without closing.
    pub fn menu_bar(
        &mut self,
        state: &mut MenuBarState,
        menus: &[MenuBarMenu<'_>],
    ) -> MenuBarResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = if self.axis() == Axis::Vertical {
            self.remaining().0.max(1)
        } else {
            menus
                .iter()
                .map(|menu| {
                    self.painter
                        .text_width(menu.label, metrics.font_scale)
                        .saturating_add(metrics.padding_x * 2)
                })
                .sum::<i32>()
                .max(1)
        };
        let bar = self.allocate(width, metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, bar);
        let mut selected = None;
        let mut dismissed = false;

        self.painter.fill_rect(bar, palette.surface_alt);
        self.painter.fill_rect(
            Rect::new(bar.x, bar.bottom_i32() - 1, bar.width, 1),
            palette.border,
        );

        // Handle the open dropdown before the bar labels so Enter does not
        // activate the focused label (via `interact`) and close the menu
        // before the highlighted entry is chosen.
        let was_open = state.open;
        let mut keyboard_used = false;
        if let Some(index) = was_open {
            keyboard_used = self.key_pressed(Key::Up)
                || self.key_pressed(Key::Down)
                || self.key_pressed(Key::Home)
                || self.key_pressed(Key::End)
                || self.enter_pressed()
                || self.escape_pressed();
            if let Some(menu) = menus.get(index) {
                let (key_selected, key_dismissed) = self.menu_keys(&mut state.menu, menu.entries);
                selected = key_selected.map(|entry| (index, entry));
                dismissed = key_dismissed;
                if !state.menu.open {
                    state.open = None;
                }
            } else {
                state.open = None;
                state.menu.open = false;
            }
        }

        let mut x = bar.x;
        let mut open_anchor = Rect::new(0, 0, 0, 0);
        for (index, menu) in menus.iter().enumerate() {
            let label_width = self
                .painter
                .text_width(menu.label, metrics.font_scale)
                .saturating_add(metrics.padding_x * 2);
            let label = Rect::new(x, bar.y, label_width.max(0) as u32, bar.height);
            let hit = self.interact(id.child(index as u64 + 1), label, Sense::click());
            if hit.clicked && !keyboard_used {
                if state.open == Some(index) {
                    state.open = None;
                    state.menu.open = false;
                } else {
                    state.open = Some(index);
                    state.menu.open = true;
                    state.menu.highlighted = 0;
                }
                response.clicked = true;
                response.changed = true;
            }
            let active = state.open == Some(index);
            if active || hit.hovered {
                self.painter.fill_rect(
                    label.inset_xy(2, 3),
                    if active {
                        palette.surface_active
                    } else {
                        palette.surface_hover
                    },
                );
            }
            self.painter.draw_text_ellipsis(
                label,
                menu.label,
                palette.text,
                metrics.font_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
            if active {
                open_anchor = label;
            }
            x += label_width;
            if x >= bar.right_i32() {
                break;
            }
        }

        if let Some(open) = state.open {
            if let Some(menu) = menus.get(open) {
                let popup = self.menu_popup(
                    id.child(0xba5),
                    open_anchor,
                    &mut state.menu,
                    menu.entries,
                    open_anchor.width as i32,
                );
                if popup.selected.is_some() {
                    selected = popup.selected.map(|entry| (open, entry));
                }
                dismissed |= popup.dismissed;
                if !state.menu.open {
                    state.open = None;
                }
            } else {
                state.open = None;
                state.menu.open = false;
            }
        }
        if selected.is_some() {
            response.changed = true;
            response.clicked = true;
        }
        MenuBarResponse {
            response,
            selected,
            dismissed,
        }
    }
}

/// One button in a [`Ui::command_bar`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandItem<'a> {
    /// Command icon.
    pub icon: Icon,
    /// Command label.
    pub label: &'a str,
}

impl<'a> CommandItem<'a> {
    pub const fn new(icon: Icon, label: &'a str) -> Self {
        Self { icon, label }
    }
}

/// Persistent state for [`Ui::command_bar`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommandBarState {
    /// Overflow menu state.
    pub overflow: MenuState,
}

impl CommandBarState {
    pub const fn new() -> Self {
        Self {
            overflow: MenuState::new(),
        }
    }
}

/// The result of [`Ui::command_bar`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandBarResponse {
    /// Interaction with the bar.
    pub response: Response,
    /// Command activated this frame.
    pub activated: Option<usize>,
    /// Whether the overflow menu is open.
    pub overflow_open: bool,
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A compact command bar with icon buttons and an overflow menu.
    ///
    /// Commands that do not fit move into the trailing overflow menu. The
    /// overflow menu entries are generated from the hidden command labels, so
    /// `activated` is always an index into the original `items` slice.
    pub fn command_bar(
        &mut self,
        state: &mut CommandBarState,
        items: &[CommandItem<'_>],
    ) -> CommandBarResponse {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = if self.axis() == Axis::Vertical {
            self.remaining().0.max(1)
        } else {
            420
        };
        let rect = self.allocate(width, metrics.control_height + 8);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let mut activated = None;

        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface_alt);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );
        let inner = rect.inset_xy(4, 4);
        let overflow_size = metrics.control_height;
        let overflow = Rect::new(
            rect.right_i32() - 4 - overflow_size,
            rect.y + 4,
            overflow_size as u32,
            overflow_size as u32,
        );
        let mut x = inner.x;
        let mut visible = 0usize;
        for item in items {
            let item_width = self
                .painter
                .text_width(item.label, metrics.font_scale)
                .saturating_add(metrics.padding_x * 2 + 22);
            if x + item_width > overflow.x - 4 {
                break;
            }
            let button = Rect::new(x, inner.y, item_width as u32, inner.height);
            let hit = self.interact(
                id.child(visible as u64 + 1),
                button,
                Sense::HOVER.with(Sense::CLICK),
            );
            if hit.hovered {
                self.painter
                    .fill_rounded_rect(button, metrics.radius, palette.surface_hover);
            }
            paint_icon(
                &mut self.painter,
                Rect::new(
                    button.x + metrics.padding_x,
                    button.y + (button.height as i32 - 16) / 2,
                    16,
                    16,
                ),
                item.icon,
                palette.text,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(
                    button.x + metrics.padding_x + 20,
                    button.y,
                    button
                        .width
                        .saturating_sub((metrics.padding_x * 2 + 20) as u32),
                    button.height,
                ),
                item.label,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            if hit.clicked {
                activated = Some(visible);
                response.clicked = true;
                response.changed = true;
            }
            x += item_width + 2;
            visible += 1;
        }

        let hidden = items.len().saturating_sub(visible);
        let overflow_hit =
            self.interact(id.child(0xc0de), overflow, Sense::HOVER.with(Sense::CLICK));
        if overflow_hit.hovered || state.overflow.open {
            self.painter.fill_rounded_rect(
                overflow.inset(2),
                metrics.radius,
                palette.surface_hover,
            );
        }
        paint_icon(
            &mut self.painter,
            overflow.inset(8),
            if hidden > 0 {
                Icon::MoreHorizontal
            } else {
                Icon::ChevronDown
            },
            palette.text_muted,
        );
        if hidden > 0 && overflow_hit.clicked && activated.is_none() {
            state.overflow.open = !state.overflow.open;
            if state.overflow.open {
                state.overflow.highlighted = 0;
            }
        }

        if hidden > 0 {
            let first = items[visible].label;
            let mut entries = [MenuEntry::item(first); 32];
            let mut count = 0usize;
            for item in items.iter().skip(visible).take(32) {
                entries[count] = MenuEntry::item(item.label);
                count += 1;
            }
            let (key_selected, _) = self.menu_keys(&mut state.overflow, &entries[..count]);
            let popup = self.menu_popup(
                id.child(0xc0de),
                overflow,
                &mut state.overflow,
                &entries[..count],
                overflow.width as i32,
            );
            if let Some(entry) = key_selected.or(popup.selected) {
                activated = Some(visible + entry);
                response.clicked = true;
                response.changed = true;
            }
        }

        CommandBarResponse {
            response,
            activated,
            overflow_open: state.overflow.open,
        }
    }
}
