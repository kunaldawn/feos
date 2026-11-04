//! Desktop composition patterns.
//!
//! These components sit above the primitives: avatar groups, property grids,
//! log and code viewers, chat threads, title bars, selector bars, sheets and
//! command palettes. They follow the same immediate-mode rules as the rest of
//! the toolkit: caller-owned state, no allocation and no hidden widget tree.

use feos_gfx::{Color, Rect};

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::response::{Response, Sense};
use crate::state::ScrollState;
use crate::text::{FixedBuf, TextBuffer};
use crate::theme::Palette;
use crate::ui::Ui;

use super::feedback::ConfirmResult;
use super::icons::{paint_icon, Icon};
use super::inputs::{MenuEntry, MenuResponse, MenuState};

/// Presence shown by an avatar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presence {
    /// Grey / signed out.
    #[default]
    Offline,
    /// Green / available.
    Online,
    /// Amber / away.
    Away,
    /// Red / busy.
    Busy,
    /// Red ring / do not disturb.
    DoNotDisturb,
}

impl Presence {
    /// The colour of the status dot.
    pub const fn color(self, palette: Palette) -> Color {
        match self {
            Self::Offline => palette.text_disabled,
            Self::Online => palette.success,
            Self::Away => palette.warning,
            Self::Busy | Self::DoNotDisturb => palette.danger,
        }
    }

    /// A short human-readable label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Offline => "Offline",
            Self::Online => "Online",
            Self::Away => "Away",
            Self::Busy => "Busy",
            Self::DoNotDisturb => "Do not disturb",
        }
    }
}

/// One line in a [`Ui::diff_view`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLine<'a> {
    /// Whether the line was added, removed or unchanged.
    pub kind: DiffKind,
    /// The line contents, without the leading marker.
    pub text: &'a str,
}

impl<'a> DiffLine<'a> {
    pub const fn context(text: &'a str) -> Self {
        Self {
            kind: DiffKind::Context,
            text,
        }
    }

    pub const fn added(text: &'a str) -> Self {
        Self {
            kind: DiffKind::Added,
            text,
        }
    }

    pub const fn removed(text: &'a str) -> Self {
        Self {
            kind: DiffKind::Removed,
            text,
        }
    }
}

/// Line classification used by [`DiffLine`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffKind {
    #[default]
    Context,
    Added,
    Removed,
}

/// One message in a comment or chat thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatMessage<'a> {
    /// Display name.
    pub author: &'a str,
    /// Message body.
    pub body: &'a str,
    /// Timestamp or relative time.
    pub time: &'a str,
    /// Initials when no avatar image exists.
    pub initials: &'a str,
    /// The message belongs to the current user, so it is right-aligned.
    pub outgoing: bool,
}

impl<'a> ChatMessage<'a> {
    pub const fn new(author: &'a str, body: &'a str) -> Self {
        Self {
            author,
            body,
            time: "",
            initials: "",
            outgoing: false,
        }
    }

    pub const fn with_time(mut self, time: &'a str) -> Self {
        self.time = time;
        self
    }

    pub const fn with_initials(mut self, initials: &'a str) -> Self {
        self.initials = initials;
        self
    }

    pub const fn outgoing(mut self) -> Self {
        self.outgoing = true;
        self
    }
}

/// Persistent state for [`Ui::command_palette`].
pub struct CommandPaletteState {
    /// Whether the palette is visible.
    pub open: bool,
    /// Highlighted result.
    pub highlighted: usize,
    /// The query typed into the palette.
    pub query: TextBuffer<64>,
}

impl CommandPaletteState {
    pub const fn new() -> Self {
        Self {
            open: false,
            highlighted: 0,
            query: TextBuffer::new(),
        }
    }

    /// Opens the palette with a fresh query.
    pub fn show(&mut self) {
        self.open = true;
        self.highlighted = 0;
        self.query.clear();
    }

    /// Closes the palette.
    pub fn hide(&mut self) {
        self.open = false;
    }
}

impl Default for CommandPaletteState {
    fn default() -> Self {
        Self::new()
    }
}

/// Horizontal swipe offset for [`Ui::swipe_actions`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwipeState {
    /// `0` is fully closed; negative values reveal actions on the right.
    pub offset: i32,
}

impl SwipeState {
    pub const fn new() -> Self {
        Self { offset: 0 }
    }

    pub fn close(&mut self) {
        self.offset = 0;
    }
}

/// Result of [`Ui::swipe_actions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwipeResponse<R> {
    /// Interaction with the swipe surface.
    pub response: Response,
    /// The action the user activated, if any.
    pub action: Option<usize>,
    /// The value returned by the content closure.
    pub inner: R,
}

/// State for [`Ui::pull_to_refresh`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PullState {
    /// Current pull distance in pixels.
    pub distance: i32,
    /// `true` once the pull is far enough to trigger a refresh.
    pub armed: bool,
    /// `true` while the caller is refreshing.
    pub refreshing: bool,
}

impl PullState {
    pub const fn new() -> Self {
        Self {
            distance: 0,
            armed: false,
            refreshing: false,
        }
    }

    /// Resets the pull after a refresh completes.
    pub fn finish(&mut self) {
        self.distance = 0;
        self.armed = false;
        self.refreshing = false;
    }
}

/// Zoom value used by [`Ui::zoom_control`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomState {
    /// Current zoom factor, normally between `0.25` and `4.0`.
    pub level: f32,
}

impl ZoomState {
    pub const fn new(level: f32) -> Self {
        Self { level }
    }
}

impl Default for ZoomState {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A stacked row of initials. The rightmost avatars are clipped to fit.
    pub fn avatar_group(&mut self, initials: &[&str], colors: &[Color], size: i32) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let size = size.max(18);
        let overlap = (size / 3).max(4);
        let count = initials.len().max(1);
        let width = size + (count as i32 - 1) * (size - overlap);
        let rect = self.allocate_sized(width, size);
        let clip = self.painter.push_clip(rect);
        for (index, label) in initials.iter().enumerate() {
            let slot = Rect::new(
                rect.x + index as i32 * (size - overlap),
                rect.y,
                size as u32,
                size as u32,
            );
            let color = colors
                .get(index)
                .copied()
                .unwrap_or_else(|| avatar_tint(palette, index));
            self.painter
                .fill_circle(slot.center().x, slot.center().y, (size / 2) as u32, color);
            self.painter.circle(
                slot.center().x,
                slot.center().y,
                (size / 2) as u32,
                palette.surface,
            );
            self.painter.draw_text_ellipsis(
                slot,
                label,
                palette.accent_text,
                metrics.small_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
        }
        self.painter.restore_clip(clip);
        rect
    }

    /// An avatar with a presence dot.
    pub fn presence_badge(
        &mut self,
        initials: &str,
        color: Color,
        presence: Presence,
        size: i32,
    ) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let size = size.max(18);
        let rect = self.allocate_sized(size, size);
        self.painter
            .fill_circle(rect.center().x, rect.center().y, (size / 2) as u32, color);
        self.painter.draw_text_ellipsis(
            rect,
            initials,
            palette.accent_text,
            metrics.small_scale,
            Align::Center,
            VerticalAlign::Middle,
        );
        let dot = (size / 3).max(6) as u32;
        let dot_rect = Rect::new(
            rect.right_i32() - dot as i32,
            rect.bottom_i32() - dot as i32,
            dot,
            dot,
        );
        self.painter.fill_circle(
            dot_rect.center().x,
            dot_rect.center().y,
            dot / 2,
            palette.surface,
        );
        self.painter.fill_circle(
            dot_rect.center().x,
            dot_rect.center().y,
            (dot / 2).saturating_sub(1),
            presence.color(palette),
        );
        rect
    }

    /// A bordered group with a title cut into the top edge.
    pub fn group_box<R>(&mut self, rect: Rect, title: &str, f: impl FnOnce(&mut Self) -> R) -> R {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let label_width = self
            .painter
            .text_width(title, metrics.small_scale)
            .saturating_add(10);
        let inner = rect.inset_each(
            metrics.padding_x,
            metrics.padding_y + 4,
            metrics.padding_x,
            metrics.padding_y,
        );
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius.min(6),
            metrics.border_width,
            palette.border,
        );
        self.painter.fill_rect(
            Rect::new(rect.x + 10, rect.y - 1, label_width as u32, 3),
            palette.background,
        );
        self.painter.draw_text_ellipsis(
            Rect::new(rect.x + 16, rect.y - 8, label_width as u32, 16),
            title,
            palette.text_muted,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        let (_, value) = self.column_with(inner, metrics.gap, f);
        value
    }

    /// A two-column label/value grid.
    pub fn property_grid(&mut self, rect: Rect, rows: &[(&str, &str)]) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        if rect.is_empty() || rows.is_empty() {
            return rect;
        }
        let row_height = metrics.row_height.max(20);
        let label_width = (rect.width as i32 / 3).clamp(70, 180);
        let clip = self.painter.push_clip(rect);
        let mut y = rect.y;
        for (index, (label, value)) in rows.iter().enumerate() {
            if y + row_height > rect.bottom_i32() {
                break;
            }
            if index % 2 == 0 {
                self.painter.fill_rounded_rect(
                    Rect::new(rect.x, y, rect.width, row_height as u32),
                    3,
                    palette.surface_alt,
                );
            }
            self.painter.draw_text_ellipsis(
                Rect::new(rect.x + 8, y, label_width as u32, row_height as u32),
                label,
                palette.text_muted,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(
                    rect.x + label_width + 8,
                    y,
                    rect.width.saturating_sub((label_width + 16).max(0) as u32),
                    row_height as u32,
                ),
                value,
                palette.text,
                metrics.small_scale,
                Align::End,
                VerticalAlign::Middle,
            );
            y += row_height;
        }
        self.painter.restore_clip(clip);
        Rect::new(
            rect.x,
            rect.y,
            rect.width,
            (rows.len() as i32 * row_height).min(rect.height as i32) as u32,
        )
    }

    /// A scrollable, monospaced log viewer with a compact line gutter.
    pub fn log_view(&mut self, rect: Rect, lines: &[&str], state: &mut ScrollState) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::HOVER.with(Sense::SCROLL));
        let line_height = (metrics.small_scale as i32 * 10 + 2).max(14);
        state.viewport_height = rect.height as i32;
        state.content_height = lines.len() as i32 * line_height;
        if response.hovered && response.scroll != 0 {
            state.scroll_by(-response.scroll * line_height);
            response.changed = true;
        }
        state.clamp();
        self.painter
            .fill_rounded_rect(rect, metrics.radius.min(4), palette.background);
        self.painter.stroke_rect(rect, 1, palette.border);
        let clip = self.painter.push_clip(rect.inset(1));
        let first = (state.offset / line_height).max(0) as usize;
        let offset = state.offset % line_height;
        for (visible, line) in lines.iter().skip(first).enumerate() {
            let y = rect.y + 3 + visible as i32 * line_height - offset;
            if y > rect.bottom_i32() {
                break;
            }
            if y + line_height < rect.y {
                continue;
            }
            let number = first + visible + 1;
            self.painter.draw_text_aligned(
                Rect::new(rect.x + 4, y, 28, line_height as u32),
                log_number(number),
                palette.text_disabled,
                metrics.small_scale,
                Align::End,
                VerticalAlign::Middle,
            );
            let color = if line.starts_with("ERR") || line.starts_with("ERROR") {
                palette.danger
            } else if line.starts_with("WARN") {
                palette.warning
            } else {
                palette.text
            };
            self.painter.draw_text_ellipsis(
                Rect::new(
                    rect.x + 38,
                    y,
                    rect.width.saturating_sub(42),
                    line_height as u32,
                ),
                line,
                color,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
        }
        self.painter.restore_clip(clip);
        self.annotated_scrollbar(
            Rect::new(
                rect.right_i32() - metrics.scrollbar_width - 2,
                rect.y + 2,
                metrics.scrollbar_width as u32,
                rect.height.saturating_sub(4),
            ),
            rect.height as i32,
            state.content_height,
            &mut state.offset,
            &[],
        );
        response
    }

    /// A framed code block with a line-number gutter.
    pub fn code_block(&mut self, rect: Rect, code: &str) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        self.painter
            .fill_rounded_rect(rect, metrics.radius.min(6), palette.background);
        self.painter
            .stroke_rounded_rect(rect, metrics.radius.min(6), 1, palette.border);
        let line_height = (metrics.small_scale as i32 * 10 + 2).max(14);
        let clip = self.painter.push_clip(rect.inset(1));
        let mut y = rect.y + 6;
        for (index, line) in code.lines().enumerate() {
            if y + line_height > rect.bottom_i32() {
                break;
            }
            self.painter.draw_text_aligned(
                Rect::new(rect.x + 6, y, 26, line_height as u32),
                log_number(index + 1),
                palette.text_disabled,
                metrics.small_scale,
                Align::End,
                VerticalAlign::Middle,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(
                    rect.x + 38,
                    y,
                    rect.width.saturating_sub(44),
                    line_height as u32,
                ),
                line,
                palette.text,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            y += line_height;
        }
        self.painter.restore_clip(clip);
        rect
    }

    /// A side-by-side style unified diff. Added and removed lines are tinted.
    pub fn diff_view(&mut self, rect: Rect, lines: &[DiffLine<'_>]) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let line_height = (metrics.small_scale as i32 * 10 + 2).max(14);
        self.painter
            .fill_rounded_rect(rect, metrics.radius.min(4), palette.background);
        self.painter.stroke_rect(rect, 1, palette.border);
        let clip = self.painter.push_clip(rect.inset(1));
        let mut y = rect.y + 4;
        for line in lines {
            if y + line_height > rect.bottom_i32() {
                break;
            }
            let (color, marker, tint) = match line.kind {
                DiffKind::Context => (palette.text, " ", None),
                DiffKind::Added => (palette.success, "+", Some(palette.success)),
                DiffKind::Removed => (palette.danger, "-", Some(palette.danger)),
            };
            let row = Rect::new(
                rect.x + 1,
                y,
                rect.width.saturating_sub(2),
                line_height as u32,
            );
            if let Some(tint) = tint {
                self.painter.fill_rect_alpha(row, tint, 18);
            }
            self.painter.draw_text(
                rect.x + 8,
                y + (line_height - 8 * metrics.small_scale as i32) / 2,
                marker,
                color,
                metrics.small_scale,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(
                    rect.x + 22,
                    y,
                    rect.width.saturating_sub(26),
                    line_height as u32,
                ),
                line.text,
                color,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            y += line_height;
        }
        self.painter.restore_clip(clip);
        rect
    }

    /// A stacked discussion thread with avatars, authors and timestamps.
    pub fn comment_thread(&mut self, messages: &[ChatMessage<'_>]) -> Rect {
        self.thread(messages, false)
    }

    /// A chat transcript. Messages marked `outgoing` are aligned right.
    pub fn chat_thread(&mut self, messages: &[ChatMessage<'_>]) -> Rect {
        self.thread(messages, true)
    }

    fn thread(&mut self, messages: &[ChatMessage<'_>], bubbles: bool) -> Rect {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = self.remaining().0.max(220);
        let avatar = 28;
        let mut total = 0i32;
        for message in messages {
            let body_width = (width - avatar * 2 - 24).max(80);
            let height = self
                .painter
                .wrapped_text_height(body_width, message.body, metrics.small_scale)
                .max(metrics.small_scale as i32 * 8);
            total += height + 18 + metrics.gap;
        }
        let rect = self.allocate_sized(width, total.max(metrics.row_height));
        let clip = self.painter.push_clip(rect);
        let mut y = rect.y;
        for (index, message) in messages.iter().enumerate() {
            let height = self
                .painter
                .wrapped_text_height(width - avatar * 2 - 24, message.body, metrics.small_scale)
                .max(metrics.small_scale as i32 * 8);
            let outgoing = bubbles && message.outgoing;
            let avatar_color = avatar_tint(palette, index);
            let avatar_rect = if outgoing {
                Rect::new(rect.right_i32() - avatar, y, avatar as u32, avatar as u32)
            } else {
                Rect::new(rect.x, y, avatar as u32, avatar as u32)
            };
            self.painter.fill_circle(
                avatar_rect.center().x,
                avatar_rect.center().y,
                (avatar / 2) as u32,
                avatar_color,
            );
            let initials = if message.initials.is_empty() {
                first_initials(message.author)
            } else {
                message.initials
            };
            self.painter.draw_text_ellipsis(
                avatar_rect,
                initials,
                palette.accent_text,
                metrics.small_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
            let text_x = if outgoing {
                rect.x
            } else {
                rect.x + avatar + 10
            };
            let text_width = width - avatar * 2 - 24;
            let header = Rect::new(text_x.max(rect.x), y, text_width.max(40) as u32, 16);
            self.painter.draw_text_ellipsis(
                header,
                message.author,
                palette.text,
                metrics.small_scale,
                if outgoing { Align::End } else { Align::Start },
                VerticalAlign::Top,
            );
            if !message.time.is_empty() {
                self.painter.draw_text_ellipsis(
                    Rect::new(
                        if outgoing {
                            text_x.max(rect.x)
                        } else {
                            text_x + text_width / 2
                        },
                        y,
                        text_width as u32 / 2,
                        16,
                    ),
                    message.time,
                    palette.text_disabled,
                    metrics.small_scale,
                    Align::End,
                    VerticalAlign::Top,
                );
            }
            let body_rect = Rect::new(
                text_x.max(rect.x),
                y + 18,
                (text_width.max(40) - if outgoing { avatar + 4 } else { 0 }) as u32,
                height as u32,
            );
            if bubbles {
                let bubble = body_rect.inset_each(-6, -3, -6, -3);
                self.painter.fill_rounded_rect(
                    bubble,
                    8,
                    if outgoing {
                        palette.accent
                    } else {
                        palette.surface_alt
                    },
                );
            }
            self.painter.draw_text_wrapped(
                body_rect,
                message.body,
                if bubbles && outgoing {
                    palette.accent_text
                } else {
                    palette.text
                },
                metrics.small_scale,
            );
            y += height + 24 + metrics.gap;
            let _ = index;
        }
        self.painter.restore_clip(clip);
        rect
    }

    /// A centered result/empty page with an icon, copy and one action.
    pub fn result_page(
        &mut self,
        rect: Rect,
        icon: Icon,
        title: &str,
        message: &str,
        action: &str,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let icon_size = 56.min(rect.width as i32 / 2).max(24);
        let icon_rect = Rect::new(
            rect.center().x - icon_size / 2,
            rect.y + (rect.height as i32 / 2 - 90).max(metrics.padding_y),
            icon_size as u32,
            icon_size as u32,
        );
        paint_icon(&mut self.painter, icon_rect, icon, palette.accent);
        self.painter.draw_text_ellipsis(
            Rect::new(rect.x, icon_rect.bottom_i32() + 12, rect.width, 24),
            title,
            palette.text,
            metrics.heading_scale,
            Align::Center,
            VerticalAlign::Top,
        );
        self.painter.draw_text_wrapped(
            Rect::new(
                rect.x + metrics.padding_x * 2,
                icon_rect.bottom_i32() + 44,
                rect.width.saturating_sub((metrics.padding_x * 4) as u32),
                60,
            ),
            message,
            palette.text_muted,
            metrics.small_scale,
        );
        if !action.is_empty() {
            let width = self
                .painter
                .text_width(action, metrics.font_scale)
                .saturating_add(metrics.padding_x * 2 + 12)
                .max(96);
            let button = Rect::new(
                rect.center().x - width / 2,
                icon_rect.bottom_i32() + 104,
                width as u32,
                metrics.control_height as u32,
            );
            let hit = self.button_at(button, action, true);
            response.hovered = hit.hovered;
            response.clicked = hit.clicked;
            response.changed = hit.clicked;
        }
        response
    }

    /// A horizontal selector bar with an accent underline under the active
    /// item. This is a selector bar, or pivot.
    pub fn selector_bar(&mut self, selected: &mut usize, labels: &[&str]) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(self.remaining().0.max(120), metrics.control_height + 4);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let mut x = rect.x;
        for (index, label) in labels.iter().enumerate() {
            let width = self
                .painter
                .text_width(label, metrics.font_scale)
                .saturating_add(metrics.padding_x * 2 + 12);
            if x + width > rect.right_i32() {
                break;
            }
            let tab = Rect::new(x, rect.y, width as u32, rect.height);
            let hit = self.interact(id.child(index as u64 + 1), tab, Sense::click());
            let active = *selected == index;
            if active {
                self.painter.fill_rounded_rect(
                    Rect::new(tab.x + 4, tab.y, tab.width.saturating_sub(8), tab.height),
                    4,
                    palette.surface_alt,
                );
            } else if hit.hovered {
                self.painter.fill_rounded_rect(
                    Rect::new(tab.x + 4, tab.y, tab.width.saturating_sub(8), tab.height),
                    4,
                    palette.surface_hover,
                );
            }
            self.painter.draw_text_ellipsis(
                tab,
                label,
                if active {
                    palette.text
                } else {
                    palette.text_muted
                },
                metrics.font_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
            if active {
                self.painter.fill_rounded_rect(
                    Rect::new(
                        tab.x + 8,
                        tab.bottom_i32() - 3,
                        tab.width.saturating_sub(16),
                        3,
                    ),
                    2,
                    palette.accent,
                );
            }
            if hit.clicked && !active {
                *selected = index;
                response.changed = true;
                response.clicked = true;
            }
            response.hovered |= hit.hovered;
            x += width;
        }
        response
    }

    /// A Fluent pivot: a compact row of text headers with an underline.
    pub fn pivot(&mut self, selected: &mut usize, labels: &[&str]) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(self.remaining().0.max(120), metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let mut x = rect.x;
        for (index, label) in labels.iter().enumerate() {
            let width = self
                .painter
                .text_width(label, metrics.font_scale)
                .saturating_add(metrics.padding_x * 2);
            if x + width > rect.right_i32() {
                break;
            }
            let tab = Rect::new(x, rect.y, width as u32, rect.height);
            let hit = self.interact(id.child(index as u64 + 1), tab, Sense::click());
            let active = *selected == index;
            self.painter.draw_text_ellipsis(
                tab,
                label,
                if active || hit.hovered {
                    palette.text
                } else {
                    palette.text_muted
                },
                metrics.font_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
            if active {
                self.painter.fill_rect(
                    Rect::new(
                        tab.x + 10,
                        tab.bottom_i32() - 2,
                        tab.width.saturating_sub(20),
                        2,
                    ),
                    palette.accent,
                );
            }
            if hit.clicked && !active {
                *selected = index;
                response.changed = true;
                response.clicked = true;
            }
            response.hovered |= hit.hovered;
            x += width;
        }
        response
    }

    /// A command-bar overflow button. The menu is drawn late, like
    /// [`Ui::menu_button`](crate::Ui::menu_button).
    pub fn command_bar_flyout(
        &mut self,
        state: &mut MenuState,
        label: &str,
        entries: &[MenuEntry<'_>],
    ) -> MenuResponse {
        self.menu_button(state, label, entries)
    }

    /// A Fluent title bar with an app glyph, title, subtitle and window
    /// buttons. `response.clicked` reports a close-button click.
    pub fn title_bar(&mut self, rect: Rect, title: &str, subtitle: &str) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        if rect.is_empty() {
            return response;
        }
        self.painter.fill_rect(rect, palette.surface_alt);
        self.painter.fill_rect(
            Rect::new(rect.x, rect.bottom_i32() - 1, rect.width, 1),
            palette.border,
        );
        let glyph = Rect::new(rect.x + 8, rect.y + (rect.height as i32 - 18) / 2, 18, 18);
        paint_icon(&mut self.painter, glyph, Icon::Grid, palette.accent);
        self.painter.draw_text_ellipsis(
            Rect::new(glyph.right_i32() + 10, rect.y, rect.width / 2, rect.height),
            title,
            palette.text,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        if !subtitle.is_empty() {
            self.painter.draw_text_ellipsis(
                Rect::new(glyph.right_i32() + 10, rect.y, rect.width / 2, rect.height),
                subtitle,
                palette.text_muted,
                metrics.small_scale,
                Align::End,
                VerticalAlign::Middle,
            );
        }
        let button = (rect.height as i32).clamp(18, 30);
        let mut x = rect.right_i32() - button - 4;
        for (index, icon) in [Icon::Minus, Icon::Grid, Icon::Close].iter().enumerate() {
            let hit_rect = Rect::new(
                x,
                rect.y + (rect.height as i32 - button) / 2,
                button as u32,
                button as u32,
            );
            let hit = self.interact(id.child(index as u64 + 1), hit_rect, Sense::click());
            if hit.hovered {
                self.painter.fill_rounded_rect(
                    hit_rect,
                    3,
                    if index == 2 {
                        palette.danger
                    } else {
                        palette.surface_hover
                    },
                );
            }
            paint_icon(
                &mut self.painter,
                hit_rect,
                *icon,
                if index == 2 && hit.hovered {
                    palette.accent_text
                } else {
                    palette.text_muted
                },
            );
            if hit.clicked && index == 2 {
                response.clicked = true;
                response.changed = true;
            }
            x -= button + 2;
        }
        response
    }

    /// A circular floating action button.
    pub fn float_button(&mut self, icon: Icon) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let size = (metrics.control_height + 12).max(44);
        let rect = self.allocate_sized(size, size);
        let id = self.next_id();
        let response = self.interact(id, rect, Sense::click());
        let radius = (size / 2) as u32;
        self.painter.shadow(rect, radius, 6, 110);
        self.painter.fill_circle(
            rect.center().x,
            rect.center().y,
            radius,
            if response.pressed {
                palette.accent.lerp(Color::BLACK, 45)
            } else if response.hovered {
                palette.accent_hover
            } else {
                palette.accent
            },
        );
        paint_icon(
            &mut self.painter,
            rect.inset((size / 4).max(6)),
            icon,
            palette.accent_text,
        );
        if response.focused {
            self.painter
                .circle(rect.center().x, rect.center().y, radius + 2, palette.focus);
        }
        response
    }

    /// A small "back to top" affordance. Returns a click when pressed.
    pub fn back_to_top(&mut self, visible: bool) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let size = (metrics.control_height + 6).max(38);
        let rect = self.allocate_sized(size, size);
        if !visible {
            return Response::empty(self.next_id(), rect);
        }
        let id = self.next_id();
        let response = self.interact(id, rect, Sense::click());
        self.painter.shadow(rect, (size / 2) as u32, 5, 90);
        self.painter.fill_circle(
            rect.center().x,
            rect.center().y,
            (size / 2) as u32,
            if response.hovered {
                palette.surface_hover
            } else {
                palette.surface_alt
            },
        );
        self.painter.circle(
            rect.center().x,
            rect.center().y,
            (size / 2) as u32,
            palette.border_strong,
        );
        paint_icon(
            &mut self.painter,
            rect.inset(size / 4),
            Icon::ArrowUp,
            palette.text,
        );
        response
    }

    /// A modal bottom sheet. Draw it last so it floats above the page.
    ///
    /// Returns `None` when the sheet is closed.
    pub fn bottom_sheet<R>(
        &mut self,
        rect: Rect,
        open: &mut bool,
        title: &str,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if !*open || rect.is_empty() {
            return None;
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let height = (rect.height as i32 * 2 / 5).clamp(120, 320);
        let (sheet, _) = rect.split_bottom(height);
        let scrim = self.interact(id, rect, Sense::HOVER.with(Sense::CLICK));
        self.painter.fill_rect_alpha(rect, palette.overlay, 150);
        if scrim.clicked && !self.pointer_over(sheet) {
            *open = false;
            return None;
        }
        self.painter.shadow(sheet, 10, 12, 140);
        self.painter.fill_rounded_rect(
            Rect::new(sheet.x, sheet.y, sheet.width, sheet.height + 12),
            10,
            palette.surface,
        );
        self.painter.fill_rounded_rect(
            Rect::new(sheet.center().x - 16, sheet.y + 6, 32, 3),
            2,
            palette.border_strong,
        );
        let header = Rect::new(
            sheet.x + metrics.padding_x,
            sheet.y + 14,
            sheet.width.saturating_sub((metrics.padding_x * 2) as u32),
            24,
        );
        self.painter.draw_text_ellipsis(
            header,
            title,
            palette.text,
            metrics.heading_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        let close = Rect::new(
            header.right_i32() - 22,
            header.y + (header.height as i32 - 18) / 2,
            18,
            18,
        );
        let close_hit = self.interact(id.child(1), close, Sense::click());
        paint_icon(
            &mut self.painter,
            close,
            Icon::Close,
            if close_hit.hovered {
                palette.text
            } else {
                palette.text_muted
            },
        );
        if close_hit.clicked {
            *open = false;
            return None;
        }
        let body = Rect::new(
            sheet.x + metrics.padding_x,
            sheet.y + 46,
            sheet.width.saturating_sub((metrics.padding_x * 2) as u32),
            sheet.height.saturating_sub(54),
        );
        let clip = self.painter.push_clip(body);
        let (_, value) = self.column_with(body, metrics.gap, f);
        self.painter.restore_clip(clip);
        Some(value)
    }

    /// A compact confirmation popover anchored below a control.
    ///
    /// Set `open` to `false` when the returned result is finished.
    pub fn popconfirm(&mut self, anchor: Rect, open: &mut bool, message: &str) -> ConfirmResult {
        let mut result = ConfirmResult::default();
        if !*open {
            return result;
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let width = (anchor.width as i32).clamp(210, 300);
        let height = 86;
        let x = anchor.center().x - width / 2;
        let y = anchor.bottom_i32() + 6;
        let rect = Rect::new(x, y, width as u32, height as u32);
        self.painter.shadow(rect, 8, 8, 120);
        self.painter.fill_rounded_rect(rect, 8, palette.surface);
        self.painter
            .stroke_rounded_rect(rect, 8, 1, palette.border_strong);
        paint_icon(
            &mut self.painter,
            Rect::new(rect.x + 10, rect.y + 10, 16, 16),
            Icon::Warning,
            palette.warning,
        );
        self.painter.draw_text_wrapped(
            Rect::new(rect.x + 32, rect.y + 8, rect.width.saturating_sub(42), 34),
            message,
            palette.text,
            metrics.small_scale,
        );
        let button_width = 72;
        let cancel = Rect::new(
            rect.right_i32() - button_width * 2 - 16,
            rect.bottom_i32() - metrics.control_height - 8,
            button_width as u32,
            metrics.control_height as u32,
        );
        let confirm = Rect::new(
            rect.right_i32() - button_width - 8,
            cancel.y,
            button_width as u32,
            metrics.control_height as u32,
        );
        if self.button_at(cancel, "Cancel", false).clicked {
            result.cancelled = true;
            result.closed = true;
            *open = false;
        }
        if self.button_at(confirm, "Yes", true).clicked {
            result.confirmed = true;
            result.closed = true;
            *open = false;
        }
        result
    }

    /// A centered command palette with a search field, filtered commands and
    /// keyboard navigation. Draw it last in the frame.
    ///
    /// Returns the chosen command index. `state.open` must be toggled by the
    /// caller (usually in response to a global shortcut).
    pub fn command_palette(
        &mut self,
        state: &mut CommandPaletteState,
        commands: &[&str],
    ) -> Option<usize> {
        if !state.open {
            return None;
        }
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let screen = self.canvas_rect();
        let width = (screen.width as i32 * 2 / 3).clamp(280, 620);
        let panel = Rect::new(
            screen.center().x - width / 2,
            screen.y + (screen.height as i32 / 5).max(20),
            width as u32,
            (screen.height as i32 / 2).clamp(180, 380) as u32,
        );
        let id = self.next_id();
        let scrim = self.interact(id, screen, Sense::HOVER.with(Sense::CLICK));
        self.painter.fill_rect_alpha(screen, palette.overlay, 140);
        if scrim.clicked && !self.pointer_over(panel) {
            state.open = false;
            return None;
        }
        self.painter.shadow(panel, 10, 14, 150);
        self.painter.fill_rounded_rect(panel, 10, palette.surface);
        self.painter
            .stroke_rounded_rect(panel, 10, 1, palette.border_strong);
        let field = Rect::new(
            panel.x + metrics.padding_x,
            panel.y + metrics.padding_y,
            panel.width.saturating_sub((metrics.padding_x * 2) as u32),
            metrics.control_height as u32,
        );
        let search = self.text_input_at(&mut state.query, field, "Type a command...", 0, 0);
        if self.escape_pressed() {
            state.open = false;
            return None;
        }
        let list = Rect::new(
            panel.x + 6,
            field.bottom_i32() + 6,
            panel.width.saturating_sub(12),
            (panel.bottom_i32() - field.bottom_i32() - 12).max(0) as u32,
        );
        let matches: usize = commands
            .iter()
            .filter(|command| palette_matches(state.query.as_str(), command))
            .count();
        if matches == 0 {
            self.painter.draw_text_centered(
                list,
                "No matching commands",
                palette.text_muted,
                metrics.small_scale,
            );
            return None;
        }
        if self.key_pressed(crate::Key::Down) {
            state.highlighted = (state.highlighted + 1).min(matches - 1);
        }
        if self.key_pressed(crate::Key::Up) {
            state.highlighted = state.highlighted.saturating_sub(1);
        }
        state.highlighted = state.highlighted.min(matches - 1);
        let clip = self.painter.push_clip(list);
        let mut y = list.y;
        let mut visible = 0usize;
        let mut chosen = None;
        for (index, command) in commands.iter().enumerate() {
            if !palette_matches(state.query.as_str(), command) {
                continue;
            }
            if y + metrics.row_height > list.bottom_i32() {
                break;
            }
            let row = Rect::new(
                list.x + 2,
                y,
                list.width.saturating_sub(4),
                metrics.row_height as u32,
            );
            let hit = self.interact(
                id.child(index as u64 + 1),
                row,
                Sense::HOVER.with(Sense::CLICK),
            );
            if state.highlighted == visible {
                self.painter
                    .fill_rounded_rect(row, 4, palette.surface_active);
            } else if hit.hovered {
                self.painter
                    .fill_rounded_rect(row, 4, palette.surface_hover);
            }
            self.painter.draw_text_ellipsis(
                Rect::new(
                    row.x + metrics.padding_x,
                    row.y,
                    row.width.saturating_sub((metrics.padding_x * 2) as u32),
                    row.height,
                ),
                command,
                palette.text,
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            if hit.clicked
                || (search.focused && state.highlighted == visible && self.enter_pressed())
            {
                chosen = Some(index);
            }
            y += metrics.row_height;
            visible += 1;
        }
        self.painter.restore_clip(clip);
        if chosen.is_some() {
            state.open = false;
        }
        chosen
    }

    /// A row that reveals actions when dragged to the left. Click an action to
    /// receive its index in [`SwipeResponse::action`].
    pub fn swipe_actions<R>(
        &mut self,
        rect: Rect,
        state: &mut SwipeState,
        actions: &[&str],
        f: impl FnOnce(&mut Self) -> R,
    ) -> SwipeResponse<R> {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let response = self.interact(id, rect, Sense::drag());
        let action_width = 64;
        let max_offset = -(action_width * actions.len() as i32);
        if response.dragged {
            state.offset = (state.offset + response.drag_delta.x).clamp(max_offset, 0);
        }
        if !response.active {
            state.offset = state.offset.clamp(max_offset, 0);
        }
        let content = Rect::new(rect.x, rect.y, rect.width, rect.height);
        let inner = {
            let clip = self.painter.push_clip(content);
            let value = f(self);
            self.painter.restore_clip(clip);
            value
        };
        let mut action = None;
        if state.offset < 0 {
            let reveal = (-state.offset).min(-max_offset);
            let start_x = rect.right_i32() - reveal;
            self.painter.fill_rect(
                Rect::new(start_x, rect.y, reveal as u32, rect.height),
                palette.surface_alt,
            );
            for (index, label) in actions.iter().enumerate() {
                let x = rect.right_i32() - (index as i32 + 1) * action_width;
                if x < start_x {
                    break;
                }
                let slot = Rect::new(x, rect.y, action_width as u32, rect.height);
                let hit = self.interact(
                    id.child(index as u64 + 1),
                    slot,
                    Sense::HOVER.with(Sense::CLICK),
                );
                self.painter.fill_rect(
                    slot,
                    if hit.hovered {
                        palette.danger
                    } else {
                        palette.surface_alt
                    },
                );
                self.painter.draw_text_ellipsis(
                    slot,
                    label,
                    if hit.hovered {
                        palette.accent_text
                    } else {
                        palette.text
                    },
                    metrics.small_scale,
                    Align::Center,
                    VerticalAlign::Middle,
                );
                if hit.clicked {
                    action = Some(index);
                    state.close();
                }
            }
        }
        SwipeResponse {
            response,
            action,
            inner,
        }
    }

    /// A pull-to-refresh header. Drag down on the surface; once the threshold
    /// is crossed the response reports `changed` and `state.refreshing` is set.
    pub fn pull_to_refresh(&mut self, rect: Rect, state: &mut PullState) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::drag());
        let trigger = 56;
        if response.dragged && !state.refreshing {
            state.distance = (state.distance + response.drag_delta.y).clamp(0, 96);
            state.armed = state.distance >= trigger;
        }
        if !response.active {
            if state.armed && !state.refreshing {
                state.refreshing = true;
                response.changed = true;
                response.clicked = true;
            }
            if !state.refreshing {
                state.distance = 0;
                state.armed = false;
            }
        }
        if state.distance > 0 || state.refreshing {
            let height = state.distance.min(trigger + 16);
            let area = Rect::new(rect.x, rect.y, rect.width, height as u32);
            self.painter.fill_rect_alpha(area, palette.surface_alt, 180);
            let center = Rect::new(rect.center().x - 8, area.bottom_i32() - 22, 16, 16);
            let color = if state.armed {
                palette.accent
            } else {
                palette.text_muted
            };
            paint_icon(
                &mut self.painter,
                center,
                if state.refreshing {
                    Icon::Refresh
                } else if state.armed {
                    Icon::ArrowUp
                } else {
                    Icon::ArrowDown
                },
                color,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(rect.x, area.y + 4, rect.width, 16),
                if state.refreshing {
                    "Refreshing..."
                } else if state.armed {
                    "Release to refresh"
                } else {
                    "Pull to refresh"
                },
                palette.text_muted,
                metrics.small_scale,
                Align::Center,
                VerticalAlign::Top,
            );
        }
        response
    }

    /// A slim scrollbar with optional annotations (search hits, bookmarks,
    /// errors) drawn as coloured marks on the track.
    pub fn annotated_scrollbar(
        &mut self,
        rect: Rect,
        viewport_height: i32,
        content_height: i32,
        offset: &mut i32,
        marks: &[f32],
    ) -> Response {
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::drag());
        if rect.is_empty() {
            return response;
        }
        let content = content_height.max(viewport_height).max(1);
        let max_offset = (content - viewport_height).max(0);
        let thumb_height = ((viewport_height as f32 / content as f32) * rect.height as f32)
            .max(18.0)
            .min(rect.height as f32) as i32;
        if response.active && self.pointer_down() && rect.height as i32 > thumb_height {
            let fraction = ((self.pointer().y - rect.y - thumb_height / 2) as f32
                / (rect.height - thumb_height as u32) as f32)
                .clamp(0.0, 1.0);
            let next = (fraction * max_offset as f32) as i32;
            if next != *offset {
                *offset = next;
                response.changed = true;
            }
        }
        *offset = (*offset).clamp(0, max_offset);
        self.painter
            .fill_rounded_rect(rect, rect.width / 2, palette.scrollbar_track);
        let thumb_y = if max_offset == 0 {
            rect.y
        } else {
            rect.y
                + ((*offset as f32 / max_offset as f32)
                    * (rect.height as i32 - thumb_height) as f32) as i32
        };
        let thumb = Rect::new(rect.x, thumb_y, rect.width, thumb_height as u32);
        self.painter.fill_rounded_rect(
            thumb,
            rect.width / 2,
            if response.hovered || response.active {
                palette.scrollbar_hover
            } else {
                palette.scrollbar_thumb
            },
        );
        for mark in marks {
            let y = rect.y + ((*mark).clamp(0.0, 1.0) * rect.height as f32) as i32;
            self.painter.fill_rect(
                Rect::new(rect.x, y.saturating_sub(1), rect.width, 3),
                palette.warning,
            );
        }
        response
    }

    /// A zoom control with minus / reset / plus buttons and a slider.
    pub fn zoom_control(&mut self, zoom: &mut ZoomState, min: f32, max: f32) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let (low, high) = if min <= max { (min, max) } else { (max, min) };
        let rect = self.allocate_sized(220, metrics.control_height);
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        let button = metrics.control_height.min(rect.height as i32);
        let minus = Rect::new(rect.x, rect.y, button as u32, button as u32);
        let plus = Rect::new(
            rect.right_i32() - button,
            rect.y,
            button as u32,
            button as u32,
        );
        let minus_hit = self.interact(id.child(1), minus, Sense::click());
        let plus_hit = self.interact(id.child(2), plus, Sense::click());
        let step = ((high - low) / 20.0).max(0.01);
        if minus_hit.clicked {
            zoom.level = (zoom.level - step).max(low);
            response.changed = true;
        }
        if plus_hit.clicked {
            zoom.level = (zoom.level + step).min(high);
            response.changed = true;
        }
        for (area, icon, color) in [
            (minus, Icon::Minus, palette.text),
            (plus, Icon::Plus, palette.text),
        ] {
            self.painter
                .fill_rounded_rect(area, metrics.radius.min(5), palette.surface_alt);
            paint_icon(&mut self.painter, area.inset(8), icon, color);
        }
        let track = Rect::new(
            minus.right_i32() + 8,
            rect.y + rect.height as i32 / 2 - 2,
            (plus.x - minus.right_i32() - 16).max(1) as u32,
            4,
        );
        if track.width > 10 {
            self.painter
                .fill_rounded_rect(track, 2, palette.surface_active);
            let fraction = ((zoom.level - low) / (high - low).max(f32::EPSILON)).clamp(0.0, 1.0);
            let knob_x = track.x + (track.width as f32 * fraction) as i32;
            self.painter.fill_rounded_rect(
                Rect::new(
                    track.x,
                    track.y,
                    (knob_x - track.x).max(0) as u32,
                    track.height,
                ),
                2,
                palette.accent,
            );
            self.painter
                .fill_circle(knob_x, track.y + 2, 6, palette.accent);
        }
        let mut text = [0u8; 8];
        let text = FixedBuf::new(&mut text);
        let label = {
            let mut sink = text;
            let _ = core::fmt::Write::write_fmt(
                &mut sink,
                format_args!("{}%", (zoom.level * 100.0) as i32),
            );
            sink.into_str()
        };
        self.painter.draw_text_ellipsis(
            Rect::new(track.x, rect.y, track.width, rect.height),
            label,
            palette.text_muted,
            metrics.small_scale,
            Align::Center,
            VerticalAlign::Middle,
        );
        response
    }
}

fn avatar_tint(palette: Palette, index: usize) -> Color {
    const TINTS: [u8; 4] = [0, 32, 64, 96];
    let tint = TINTS[index % TINTS.len()];
    palette.accent.lerp(palette.success, tint)
}

fn first_initials(name: &str) -> &str {
    let mut end = name.len().min(2);
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

fn log_number(value: usize) -> &'static str {
    // Small fixed table avoids allocating for line numbers in viewers.
    const NUMBERS: &[&str] = &[
        "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
        "17", "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "28", "29", "30", "31",
        "32", "33", "34", "35", "36", "37", "38", "39", "40", "41", "42", "43", "44", "45", "46",
        "47", "48", "49", "50", "51", "52", "53", "54", "55", "56", "57", "58", "59", "60", "61",
        "62", "63", "64",
    ];
    NUMBERS.get(value.saturating_sub(1)).copied().unwrap_or("+")
}

fn palette_matches(query: &str, command: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let bytes = command.as_bytes();
    let mut start = 0usize;
    for part in query.split_whitespace() {
        let needle = part.as_bytes();
        let mut found = false;
        while start + needle.len() <= bytes.len() {
            if bytes[start..start + needle.len()].eq_ignore_ascii_case(needle) {
                found = true;
                start += needle.len();
                break;
            }
            start += 1;
        }
        if !found {
            return false;
        }
    }
    true
}
