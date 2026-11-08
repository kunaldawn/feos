//! Advanced inputs: multiline and masked text, range sliders, knobs, tag
//! editors, drop zones, shortcut recorders, cascading lists, transfer lists
//! and mention fields.

use core::fmt::Write as _;

use feos_gfx::{Color, Rect};

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::input::{Event, Key, Modifiers};
use crate::math::{cos, sin};
use crate::response::{Response, Sense};
use crate::text::{format_f32, FixedBuf, TextBuffer};
use crate::ui::Ui;

use super::icons::{paint_icon, Icon};
use super::inputs::AutoSuggestState;

/// A password field. The buffer is edited exactly like a text field, but every
/// character is drawn as `*`.
pub fn password_box<'a, 'b, 's, const N: usize, const M: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    buffer: &mut TextBuffer<M>,
    placeholder: &str,
) -> Response {
    let rect = ui.allocate(220, ui.theme.metrics.control_height);
    ui.text_input_at_masked(buffer, rect, placeholder, 0, 0)
}

/// A multiline text editor with a fixed number of visible rows.
///
/// Enter inserts a newline, Home/End move to the start and end of the current
/// line, and Up/Down move between lines. Long lines are clipped horizontally.
pub fn text_area<'a, 'b, 's, const N: usize, const M: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    buffer: &mut TextBuffer<M>,
    rows: usize,
    placeholder: &str,
) -> Response {
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let line_height = ui.painter.line_height(metrics.font_scale);
    let rows = rows.max(1) as i32;
    let rect = ui.allocate(
        ui.remaining().0.max(1),
        line_height * rows + metrics.padding_y * 2,
    );
    let id = ui.next_id();
    let mut response = ui.interact(id, rect, Sense::focus());
    if response.hovered {
        ui.set_cursor(crate::cursor::CursorShape::Text);
    }
    if response.focused {
        let mut changed = false;
        for event in ui.input.events() {
            match event {
                Event::Text(ch) => {
                    if !ch.is_control() && buffer.insert(ch) {
                        changed = true;
                    }
                }
                Event::KeyDown(Key::Enter, _) => {
                    let _ = buffer.insert_str("\n");
                    changed = true;
                }
                Event::KeyDown(Key::Backspace, _) => changed |= buffer.backspace(),
                Event::KeyDown(Key::Delete, _) => changed |= buffer.delete(),
                Event::KeyDown(Key::Left, modifiers) => {
                    buffer.move_left(modifiers.contains(Modifiers::SHIFT))
                }
                Event::KeyDown(Key::Right, modifiers) => {
                    buffer.move_right(modifiers.contains(Modifiers::SHIFT))
                }
                Event::KeyDown(Key::Home, modifiers) => {
                    let start = line_start(buffer.as_str(), buffer.cursor());
                    buffer.set_cursor(start);
                    let _ = modifiers;
                }
                Event::KeyDown(Key::End, modifiers) => {
                    let end = line_end(buffer.as_str(), buffer.cursor());
                    buffer.set_cursor(end);
                    let _ = modifiers;
                }
                Event::KeyDown(Key::Up, _) => {
                    if let Some(target) = vertical_target(buffer.as_str(), buffer.cursor(), -1) {
                        buffer.set_cursor(target);
                    }
                }
                Event::KeyDown(Key::Down, _) => {
                    if let Some(target) = vertical_target(buffer.as_str(), buffer.cursor(), 1) {
                        buffer.set_cursor(target);
                    }
                }
                _ => {}
            }
        }
        response.changed = changed;
    }

    let fill = if response.focused {
        palette.surface
    } else {
        palette.surface_alt
    };
    ui.painter.fill_rounded_rect(rect, metrics.radius, fill);
    ui.painter.stroke_rounded_rect(
        rect,
        metrics.radius,
        if response.focused {
            2
        } else {
            metrics.border_width
        },
        if response.focused {
            palette.focus
        } else if response.hovered {
            palette.border_strong
        } else {
            palette.border
        },
    );

    let inner = nav_inner(rect, metrics.padding_x, metrics.padding_y);
    let text = buffer.as_str();
    let content_height = text.lines().count().max(1) as i32 * line_height;
    let max_scroll = (content_height - inner.height as i32).max(0);
    let mut scroll = ui.state.scroll_offset(id);
    let (caret_line, caret_column) = line_column(text, buffer.cursor());
    let caret_top = caret_line as i32 * line_height;
    if caret_top - scroll > inner.height as i32 - line_height {
        scroll = caret_top - inner.height as i32 + line_height;
    }
    if caret_top - scroll < 0 {
        scroll = caret_top;
    }
    scroll = scroll.clamp(0, max_scroll);
    ui.state.set_scroll(id, scroll, content_height);

    let clip = ui.painter.push_clip(inner);
    if text.is_empty() {
        ui.painter.draw_text(
            inner.x,
            inner.y,
            placeholder,
            palette.text_disabled,
            metrics.font_scale,
        );
    } else {
        for (index, line) in text.lines().enumerate() {
            ui.painter.draw_text(
                inner.x,
                inner.y + index as i32 * line_height - scroll,
                line,
                palette.text,
                metrics.font_scale,
            );
        }
    }
    if response.focused {
        let advance = 8 * metrics.font_scale as i32;
        let caret_x = inner.x + caret_column as i32 * advance;
        let caret_y = inner.y + caret_top - scroll;
        if caret_y >= inner.y - line_height && caret_y <= inner.bottom_i32() {
            ui.painter.fill_rect(
                Rect::new(caret_x, caret_y, 2, line_height as u32),
                palette.focus,
            );
        }
    }
    ui.painter.restore_clip(clip);
    response
}

fn nav_inner(rect: Rect, x: i32, y: i32) -> Rect {
    Rect::new(
        rect.x + x,
        rect.y + y,
        rect.width.saturating_sub((x * 2) as u32),
        rect.height.saturating_sub((y * 2) as u32),
    )
}

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor.min(text.len())..]
        .find('\n')
        .map(|index| cursor + index)
        .unwrap_or(text.len())
}

fn line_column(text: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(text.len());
    let line = text[..cursor].matches('\n').count();
    let start = line_start(text, cursor);
    (line, text[start..cursor].chars().count())
}

fn vertical_target(text: &str, cursor: usize, delta: i32) -> Option<usize> {
    let (line, column) = line_column(text, cursor);
    let target_line = line as i32 + delta;
    if target_line < 0 {
        return None;
    }
    let mut start = 0usize;
    for _ in 0..target_line {
        let next = text[start..].find('\n')? + start + 1;
        start = next;
    }
    let end = line_end(text, start);
    let target: StringlessCursor = StringlessCursor {
        text,
        start,
        end,
        column,
    };
    Some(target.resolve())
}

struct StringlessCursor<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    column: usize,
}

impl StringlessCursor<'_> {
    fn resolve(self) -> usize {
        let mut index = self.start;
        for ch in self.text[self.start..self.end].chars().take(self.column) {
            index += ch.len_utf8();
        }
        index.min(self.end)
    }
}

/// A two-thumb range value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeValue {
    pub start: f32,
    pub end: f32,
}

impl RangeValue {
    pub const fn new(start: f32, end: f32) -> Self {
        Self { start, end }
    }

    pub fn clamp(&mut self, min: f32, max: f32) {
        let (low, high) = if min <= max { (min, max) } else { (max, min) };
        self.start = self.start.clamp(low, high);
        self.end = self.end.clamp(low, high);
        if self.start > self.end {
            core::mem::swap(&mut self.start, &mut self.end);
        }
    }
}

/// Which range-slider thumb is being dragged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RangeSliderState {
    /// `0` for the start thumb, `1` for the end thumb, `2` when idle.
    pub active: u8,
}

impl RangeSliderState {
    pub const fn new() -> Self {
        Self { active: 2 }
    }
}

/// A horizontal slider with independent start and end thumbs.
pub fn range_slider<'a, 'b, 's, const N: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    state: &mut RangeSliderState,
    value: &mut RangeValue,
    min: f32,
    max: f32,
) -> Response {
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let rect = ui.allocate(220, metrics.control_height);
    let id = ui.next_id();
    let mut response = ui.interact(id, rect, Sense::drag());
    value.clamp(min, max);
    let low = min.min(max);
    let high = max.max(min);
    let range = (high - low).max(f32::EPSILON);

    let knob_radius = (metrics.control_height / 2 - 6).clamp(5, 10);
    let track = Rect::new(
        rect.x + knob_radius,
        rect.y + rect.height as i32 / 2 - 2,
        rect.width.saturating_sub((knob_radius * 2) as u32),
        4,
    );

    if response.active && ui.pointer_down() {
        if state.active > 1 {
            let fraction = if track.width == 0 {
                0.0
            } else {
                ((ui.pointer().x - track.x) as f32 / track.width as f32).clamp(0.0, 1.0)
            };
            let pointer_value = low + fraction * range;
            state.active =
                if (pointer_value - value.start).abs() <= (pointer_value - value.end).abs() {
                    0
                } else {
                    1
                };
        }
        let fraction = if track.width == 0 {
            0.0
        } else {
            ((ui.pointer().x - track.x) as f32 / track.width as f32).clamp(0.0, 1.0)
        };
        let next = low + fraction * range;
        if state.active == 0 && next != value.start {
            value.start = next.min(value.end);
            response.changed = true;
        } else if state.active == 1 && next != value.end {
            value.end = next.max(value.start);
            response.changed = true;
        }
    } else if !ui.pointer_down() {
        state.active = 2;
    }

    let start_fraction = ((value.start - low) / range).clamp(0.0, 1.0);
    let end_fraction = ((value.end - low) / range).clamp(0.0, 1.0);
    let start_x = track.x + (track.width as f32 * start_fraction) as i32;
    let end_x = track.x + (track.width as f32 * end_fraction) as i32;
    ui.painter
        .fill_rounded_rect(track, 2, palette.surface_active);
    if end_x > start_x {
        ui.painter.fill_rounded_rect(
            Rect::new(start_x, track.y, (end_x - start_x) as u32, track.height),
            2,
            palette.accent,
        );
    }
    for x in [start_x, end_x] {
        ui.painter
            .fill_circle(x, track.y + 2, knob_radius as u32, palette.text);
        ui.painter
            .circle(x, track.y + 2, knob_radius as u32, palette.accent);
    }
    if response.focused {
        ui.painter
            .stroke_rounded_rect(track.inset(-2), track.height / 2 + 2, 1, palette.focus);
    }
    response
}

/// A rotary knob. Drag vertically to change the value.
pub fn knob<'a, 'b, 's, const N: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    value: &mut f32,
    min: f32,
    max: f32,
    label: &str,
) -> Response {
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let size = (metrics.control_height + 26).max(48);
    let rect = ui.allocate_sized(size, size + 18);
    let dial = Rect::new(rect.x, rect.y, size as u32, size as u32);
    let id = ui.next_id();
    let mut response = ui.interact(id, dial, Sense::drag());
    let (low, high) = if min <= max { (min, max) } else { (max, min) };
    let range = (high - low).max(f32::EPSILON);
    if response.active && ui.pointer_down() {
        let fraction =
            1.0 - ((ui.pointer().y - dial.y) as f32 / dial.height.max(1) as f32).clamp(0.0, 1.0);
        let next = low + fraction * range;
        if next != *value {
            *value = next;
            response.changed = true;
        }
    }
    let value_now = value.clamp(low, high);
    let fraction = ((value_now - low) / range).clamp(0.0, 1.0);
    ui.painter.fill_circle(
        dial.center().x,
        dial.center().y,
        (size / 2) as u32,
        palette.surface_alt,
    );
    ui.painter.circle(
        dial.center().x,
        dial.center().y,
        (size / 2) as u32,
        palette.border_strong,
    );
    let angle = -core::f32::consts::PI * 0.75 + fraction * core::f32::consts::PI * 1.5;
    let inner = (size / 2 - 8) as f32;
    let x = dial.center().x + (cos(angle) * inner) as i32;
    let y = dial.center().y + (sin(angle) * inner) as i32;
    ui.painter
        .line(dial.center().x, dial.center().y, x, y, palette.accent);
    ui.painter
        .fill_circle(dial.center().x, dial.center().y, 3, palette.accent);
    let mut buffer = [0u8; 16];
    let text = format_f32(value_now, 1, &mut buffer);
    ui.painter.draw_text_ellipsis(
        Rect::new(rect.x, dial.bottom_i32() + 4, rect.width, 16),
        text,
        palette.text,
        metrics.small_scale,
        Align::Center,
        VerticalAlign::Top,
    );
    if !label.is_empty() {
        ui.painter.draw_text_ellipsis(
            Rect::new(rect.x, dial.y + dial.height as i32 - 18, rect.width, 14),
            label,
            palette.text_muted,
            metrics.small_scale,
            Align::Center,
            VerticalAlign::Top,
        );
    }
    response
}

/// A fixed-capacity set of UTF-8 tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagSet<const N: usize = 8, const W: usize = 24> {
    tags: [[u8; W]; N],
    lengths: [usize; N],
    len: usize,
}

impl<const N: usize, const W: usize> TagSet<N, W> {
    pub const fn new() -> Self {
        Self {
            tags: [[0; W]; N],
            lengths: [0; N],
            len: 0,
        }
    }

    pub fn push(&mut self, tag: &str) -> bool {
        if self.len >= N || W == 0 {
            return false;
        }
        let mut end = tag.len().min(W);
        while end > 0 && !tag.is_char_boundary(end) {
            end -= 1;
        }
        self.tags[self.len][..end].copy_from_slice(&tag.as_bytes()[..end]);
        self.lengths[self.len] = end;
        self.len += 1;
        true
    }

    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        for slot in index..self.len.saturating_sub(1) {
            self.tags[slot] = self.tags[slot + 1];
            self.lengths[slot] = self.lengths[slot + 1];
        }
        self.len = self.len.saturating_sub(1);
        self.lengths[self.len] = 0;
        true
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        let length = *self.lengths.get(index)?;
        let bytes = self.tags.get(index)?;
        core::str::from_utf8(&bytes[..length]).ok()
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.lengths = [0; N];
    }
}

impl<const N: usize, const W: usize> Default for TagSet<N, W> {
    fn default() -> Self {
        Self::new()
    }
}

/// A tag editor with removable chips and a draft text field.
pub fn tag_input<'a, 'b, 's, const N: usize, const M: usize, const T: usize, const W: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    tags: &mut TagSet<T, W>,
    draft: &mut TextBuffer<M>,
    placeholder: &str,
) -> Response {
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let rect = ui.allocate(ui.remaining().0.max(220), metrics.control_height);
    let id = ui.next_id();
    let mut response = ui.interact(id, rect, Sense::HOVER);
    let mut x = rect.x + metrics.padding_x;
    let close = 10;
    let mut removed = None;
    let mut shown = 0usize;
    for index in 0..tags.len() {
        let Some(tag) = tags.get(index) else {
            continue;
        };
        let width = ui
            .painter
            .text_width(tag, metrics.small_scale)
            .saturating_add(20 + close);
        if x + width > rect.right_i32() - 130 {
            break;
        }
        let chip = Rect::new(
            x,
            rect.y + 5,
            width.max(0) as u32,
            rect.height.saturating_sub(10),
        );
        ui.painter
            .fill_rounded_rect(chip, chip.height / 2, palette.surface_active);
        ui.painter.draw_text_ellipsis(
            Rect::new(
                chip.x + 8,
                chip.y,
                chip.width.saturating_sub(20),
                chip.height,
            ),
            tag,
            palette.text,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        let close_rect = Rect::new(
            chip.right_i32() - close - 6,
            chip.y + (chip.height as i32 - close) / 2,
            close as u32,
            close as u32,
        );
        let hit = ui.interact(
            id.child(index as u64 + 1),
            close_rect,
            Sense::HOVER.with(Sense::CLICK),
        );
        paint_icon(&mut ui.painter, close_rect, Icon::Close, palette.text_muted);
        if hit.clicked {
            removed = Some(index);
        }
        x += width + 4;
        shown += 1;
    }
    if shown < tags.len() {
        let mut buffer = [0u8; 12];
        let label = format_u8(tags.len() - shown, &mut buffer);
        ui.painter.draw_text_aligned(
            Rect::new(x, rect.y, 34, rect.height),
            label,
            palette.text_muted,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    if let Some(index) = removed {
        tags.remove(index);
        response.changed = true;
        response.clicked = true;
    }

    let field_width = 120.min((rect.width as i32 / 3).max(80));
    let field = Rect::new(
        rect.right_i32() - metrics.padding_x - field_width,
        rect.y,
        field_width as u32,
        rect.height,
    );
    let field_response = ui.text_input_at(draft, field, placeholder, 0, 0);
    response.changed |= field_response.changed;
    if field_response.submitted && !draft.is_empty() && tags.push(draft.as_str()) {
        draft.clear();
        response.changed = true;
        response.clicked = true;
    }
    if field_response.changed
        && draft.is_empty()
        && !tags.is_empty()
        && ui.key_pressed(Key::Backspace)
    {
        tags.remove(tags.len() - 1);
        response.changed = true;
    }
    response
}

fn format_u8(value: usize, out: &mut [u8]) -> &str {
    let mut sink = FixedBuf::new(out);
    let _ = write!(&mut sink, "+{}", value);
    sink.into_str()
}

/// A drop zone for files. The toolkit has no drag-and-drop events yet, so the
/// zone reports hover and click (browse) like a large button.
pub fn file_dropzone<'a, 'b, 's, const N: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    rect: Rect,
    title: &str,
    hint: &str,
) -> Response {
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let id = ui.next_id();
    let mut response = ui.interact(id, rect, Sense::HOVER.with(Sense::CLICK));
    let tint = if response.hovered {
        palette.surface_hover
    } else {
        palette.surface_alt
    };
    ui.painter.fill_rounded_rect(rect, metrics.radius, tint);
    dashed_border(ui, rect, palette.border_strong);
    let icon = 28;
    paint_icon(
        &mut ui.painter,
        Rect::new(
            rect.center().x - icon / 2,
            rect.y + metrics.padding_y + 4,
            icon as u32,
            icon as u32,
        ),
        Icon::Upload,
        palette.accent,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(rect.x, rect.y + 40, rect.width, 18),
        title,
        palette.text,
        metrics.font_scale,
        Align::Center,
        VerticalAlign::Top,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(rect.x, rect.y + 62, rect.width, 16),
        hint,
        palette.text_muted,
        metrics.small_scale,
        Align::Center,
        VerticalAlign::Top,
    );
    let button_width = 92;
    let button = Rect::new(
        rect.center().x - button_width / 2,
        rect.bottom_i32() - metrics.padding_y - metrics.control_height,
        button_width as u32,
        metrics.control_height as u32,
    );
    let browse = ui.button_at(button, "Browse", false);
    if browse.clicked {
        response.clicked = true;
        response.changed = true;
    }
    response
}

/// Draws a dashed one-pixel outline around `rect`.
///
/// The dash pattern is fixed at 6 px on and 4 px off, and the corners are left
/// square so the helper works for drop zones and selection marquees.
pub fn dashed_rect<'a, 'b, 's, const N: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    rect: Rect,
    color: Color,
) {
    const DASH: i32 = 6;
    const GAP: i32 = 4;
    const PERIOD: i32 = DASH + GAP;
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let right = rect.right_i32() - 1;
    let bottom = rect.bottom_i32() - 1;
    let mut x = rect.x;
    while x <= right {
        let end = (x + DASH).min(right + 1);
        ui.painter.hline(x, end, rect.y, color);
        ui.painter.hline(x, end, bottom, color);
        x += PERIOD;
    }
    let mut y = rect.y;
    while y <= bottom {
        let end = (y + DASH).min(bottom + 1);
        ui.painter.vline(rect.x, y, end, color);
        ui.painter.vline(right, y, end, color);
        y += PERIOD;
    }
}

fn dashed_border<'a, 'b, 's, const N: usize>(ui: &mut Ui<'a, 'b, 's, N>, rect: Rect, color: Color) {
    dashed_rect(ui, rect, color);
}

/// A recorded keyboard shortcut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: Modifiers,
    /// `None` means the recorder is empty.
    pub key: Option<Key>,
}

impl Hotkey {
    pub const fn new() -> Self {
        Self {
            modifiers: Modifiers::NONE,
            key: None,
        }
    }

    pub const fn is_empty(self) -> bool {
        self.key.is_none()
    }

    pub fn clear(&mut self) {
        self.modifiers = Modifiers::NONE;
        self.key = None;
    }
}

impl Default for Hotkey {
    fn default() -> Self {
        Self::new()
    }
}

/// A shortcut recorder. Focus it and press a key combination.
pub fn hotkey_input<'a, 'b, 's, const N: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    hotkey: &mut Hotkey,
) -> Response {
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let rect = ui.allocate(220, metrics.control_height);
    let id = ui.next_id();
    let mut response = ui.interact(id, rect, Sense::focus());
    if response.focused {
        for event in ui.input.events() {
            match event {
                Event::KeyDown(key, modifiers) => match key {
                    Key::Tab | Key::Escape => {}
                    _ => {
                        hotkey.key = Some(key);
                        hotkey.modifiers = modifiers;
                        response.changed = true;
                    }
                },
                Event::Text(ch) if !ch.is_control() => {
                    hotkey.key = Some(Key::Character(ch));
                    hotkey.modifiers = ui.input.modifiers();
                    response.changed = true;
                }
                _ => {}
            }
        }
    }
    ui.painter.fill_rounded_rect(
        rect,
        metrics.radius,
        if response.focused {
            palette.surface
        } else {
            palette.surface_alt
        },
    );
    ui.painter.stroke_rounded_rect(
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
    let mut x = rect.x + metrics.padding_x;
    let mut parts = 0usize;
    for (label, enabled) in [
        ("Ctrl", hotkey.modifiers.contains(Modifiers::CTRL)),
        ("Shift", hotkey.modifiers.contains(Modifiers::SHIFT)),
        ("Alt", hotkey.modifiers.contains(Modifiers::ALT)),
        ("Super", hotkey.modifiers.contains(Modifiers::SUPER)),
    ] {
        if !enabled {
            continue;
        }
        let width = ui.painter.text_width(label, metrics.small_scale) + 12;
        let chip = Rect::new(
            x,
            rect.y + 6,
            width.max(0) as u32,
            rect.height.saturating_sub(12),
        );
        ui.painter
            .fill_rounded_rect(chip, 4, palette.surface_active);
        ui.painter.draw_text_ellipsis(
            chip,
            label,
            palette.text,
            metrics.small_scale,
            Align::Center,
            VerticalAlign::Middle,
        );
        x += width + 4;
        parts += 1;
    }
    if let Some(key) = hotkey.key {
        let label = key_label(key);
        let width = ui.painter.text_width(label, metrics.small_scale) + 12;
        if x + width < rect.right_i32() - 30 {
            let chip = Rect::new(
                x,
                rect.y + 6,
                width.max(0) as u32,
                rect.height.saturating_sub(12),
            );
            ui.painter.fill_rounded_rect(chip, 4, palette.surface_hover);
            ui.painter.draw_text_ellipsis(
                chip,
                label,
                palette.text,
                metrics.small_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
            parts += 1;
        }
    }
    if parts == 0 {
        ui.painter.draw_text_ellipsis(
            Rect::new(
                rect.x + metrics.padding_x,
                rect.y,
                rect.width.saturating_sub(40),
                rect.height,
            ),
            if response.focused {
                "Press a shortcut"
            } else {
                "No shortcut"
            },
            palette.text_disabled,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    let clear = Rect::new(
        rect.right_i32() - 24,
        rect.y + (rect.height as i32 - 16) / 2,
        16,
        16,
    );
    let clear_hit = ui.interact(id.child(0xc1ea4), clear, Sense::HOVER.with(Sense::CLICK));
    if !hotkey.is_empty() {
        paint_icon(
            &mut ui.painter,
            clear,
            Icon::Close,
            if clear_hit.hovered {
                palette.text
            } else {
                palette.text_muted
            },
        );
        if clear_hit.clicked {
            hotkey.clear();
            response.changed = true;
        }
    }
    response
}

fn key_label(key: Key) -> &'static str {
    match key {
        Key::Tab => "Tab",
        Key::Enter => "Enter",
        Key::Escape => "Esc",
        Key::Space => "Space",
        Key::Backspace => "Backspace",
        Key::Delete => "Delete",
        Key::Insert => "Insert",
        Key::Left => "Left",
        Key::Right => "Right",
        Key::Up => "Up",
        Key::Down => "Down",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        Key::Character(_) => "Key",
    }
}

/// Persistent state for [`cascader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CascaderState<const N: usize = 4> {
    pub selected: [usize; N],
    pub depth: usize,
}

impl<const N: usize> CascaderState<N> {
    pub const fn new() -> Self {
        Self {
            selected: [0; N],
            depth: 0,
        }
    }
}

impl<const N: usize> Default for CascaderState<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// A multi-column cascading list. Click a row to select it; `depth` reports
/// which column was last activated.
pub fn cascader<'a, 'b, 's, const N: usize, const C: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    rect: Rect,
    state: &mut CascaderState<C>,
    columns: &[&[&str]],
) -> Response {
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let id = ui.next_id();
    let mut response = Response::empty(id, rect);
    if columns.is_empty() || rect.is_empty() {
        return response;
    }
    let column_width = rect.width as i32 / columns.len() as i32;
    let row = metrics.row_height;
    let clip = ui.painter.push_clip(rect);
    for (column, items) in columns.iter().enumerate() {
        let x = rect.x + column as i32 * column_width;
        let column_rect = Rect::new(x, rect.y, column_width.max(0) as u32, rect.height);
        ui.painter
            .fill_rect(column_rect.inset(1), palette.surface_alt);
        for (index, item) in items.iter().enumerate() {
            let y = rect.y + 4 + index as i32 * row;
            if y + row > rect.bottom_i32() {
                break;
            }
            let row_rect = Rect::new(
                column_rect.x + 2,
                y,
                column_rect.width.saturating_sub(4),
                row as u32,
            );
            let hit = ui.interact(
                id.child((column as u64) * 128 + index as u64 + 1),
                row_rect,
                Sense::HOVER.with(Sense::CLICK),
            );
            let selected = state.selected.get(column).copied().unwrap_or(0) == index;
            if selected {
                ui.painter
                    .fill_rounded_rect(row_rect.inset_xy(2, 1), 4, palette.surface_active);
            } else if hit.hovered {
                ui.painter
                    .fill_rounded_rect(row_rect.inset_xy(2, 1), 4, palette.surface_hover);
            }
            ui.painter.draw_text_ellipsis(
                Rect::new(
                    row_rect.x + metrics.padding_x,
                    row_rect.y,
                    row_rect
                        .width
                        .saturating_sub((metrics.padding_x * 2) as u32),
                    row_rect.height,
                ),
                item,
                if selected {
                    palette.accent
                } else {
                    palette.text
                },
                metrics.font_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            if hit.clicked {
                if let Some(slot) = state.selected.get_mut(column) {
                    *slot = index;
                }
                state.depth = column + 1;
                response.changed = true;
                response.clicked = true;
            }
        }
        if column + 1 < columns.len() {
            ui.painter.vline(
                column_rect.right_i32() - 1,
                rect.y,
                rect.bottom_i32(),
                palette.border,
            );
        }
    }
    ui.painter.restore_clip(clip);
    response
}

/// Which items of a [`TransferState`] are on the right-hand side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferState<const N: usize = 32> {
    included: [bool; N],
}

impl<const N: usize> TransferState<N> {
    pub const fn new() -> Self {
        Self {
            included: [false; N],
        }
    }

    pub fn is_included(&self, index: usize) -> bool {
        self.included.get(index).copied().unwrap_or(false)
    }

    pub fn set(&mut self, index: usize, value: bool) -> bool {
        if let Some(slot) = self.included.get_mut(index) {
            let changed = *slot != value;
            *slot = value;
            changed
        } else {
            false
        }
    }

    pub fn toggle(&mut self, index: usize) -> bool {
        let value = !self.is_included(index);
        self.set(index, value)
    }

    pub fn count(&self) -> usize {
        self.included.iter().filter(|slot| **slot).count()
    }

    pub fn clear(&mut self) {
        self.included = [false; N];
    }
}

impl<const N: usize> Default for TransferState<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// A two-pane transfer list. Click a row to move it between the panes.
pub fn transfer_list<'a, 'b, 's, const N: usize, const M: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    rect: Rect,
    items: &[&str],
    state: &mut TransferState<M>,
) -> Response {
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let id = ui.next_id();
    let mut response = Response::empty(id, rect);
    if rect.is_empty() {
        return response;
    }
    let gap = 30;
    let column_width = ((rect.width as i32 - gap) / 2).max(20);
    let left = Rect::new(rect.x, rect.y, column_width as u32, rect.height);
    let right = Rect::new(
        rect.right_i32() - column_width,
        rect.y,
        column_width as u32,
        rect.height,
    );
    for (side, area, included) in [(0u64, left, false), (1u64, right, true)] {
        ui.painter
            .fill_rounded_rect(area, metrics.radius.min(6), palette.surface_alt);
        ui.painter.stroke_rounded_rect(
            area,
            metrics.radius.min(6),
            metrics.border_width,
            palette.border,
        );
        let mut y = area.y + 4;
        for (index, item) in items.iter().enumerate() {
            if state.is_included(index) != included {
                continue;
            }
            if y + metrics.row_height > area.bottom_i32() {
                break;
            }
            let row = Rect::new(
                area.x + 4,
                y,
                area.width.saturating_sub(8),
                metrics.row_height as u32,
            );
            let hit = ui.interact(
                id.child(side * 0x1000 + index as u64 + 1),
                row,
                Sense::HOVER.with(Sense::CLICK),
            );
            if hit.hovered {
                ui.painter
                    .fill_rounded_rect(row.inset_xy(2, 1), 4, palette.surface_hover);
            }
            ui.painter.draw_text_ellipsis(
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
            if hit.clicked && state.toggle(index) {
                response.changed = true;
                response.clicked = true;
            }
            y += metrics.row_height;
        }
    }
    let arrow_x = left.right_i32() + gap / 2;
    ui.painter.draw_text_centered(
        Rect::new(arrow_x - 8, rect.y, 16, rect.height),
        ">",
        palette.text_muted,
        metrics.font_scale,
    );
    response
}

/// A text field with an `@mention` suggestion popup.
pub fn mention_box<'a, 'b, 's, const N: usize, const M: usize>(
    ui: &mut Ui<'a, 'b, 's, N>,
    state: &mut AutoSuggestState,
    buffer: &mut TextBuffer<M>,
    people: &[&str],
) -> crate::components::inputs::AutoSuggestResponse {
    use crate::components::inputs::AutoSuggestResponse;
    let metrics = ui.theme.metrics;
    let palette = ui.theme.palette;
    let mut response = ui.search_box(buffer, "Type @ to mention");
    let rect = response.rect;
    let mut query_buf = [0u8; 64];
    let mut query_len = 0usize;
    let raw = buffer.as_str();
    let has_query = if let Some(index) = raw.rfind('@') {
        let tail = &raw[index + 1..];
        let mut end = tail.len().min(query_buf.len());
        while end > 0 && !tail.is_char_boundary(end) {
            end -= 1;
        }
        query_buf[..end].copy_from_slice(&tail.as_bytes()[..end]);
        query_len = end;
        true
    } else {
        false
    };
    let query = core::str::from_utf8(&query_buf[..query_len]).unwrap_or("");
    let mut selected = None;
    let mut matches = 0usize;
    if has_query {
        matches = people
            .iter()
            .filter(|person| person_prefix_matches(query, person))
            .count();
        if response.focused {
            if ui.key_pressed(Key::Down) {
                state.highlighted = (state.highlighted + 1).min(matches.saturating_sub(1));
            }
            if ui.key_pressed(Key::Up) {
                state.highlighted = state.highlighted.saturating_sub(1);
            }
            if ui.escape_pressed() {
                state.open = false;
            }
            if ui.enter_pressed() && matches > 0 {
                if let Some((index, person)) = people
                    .iter()
                    .enumerate()
                    .filter(|(_, person)| person_prefix_matches(query, person))
                    .nth(state.highlighted)
                {
                    let mut out = [0u8; 96];
                    let mut sink = FixedBuf::new(&mut out);
                    let _ = write!(&mut sink, "@{} ", person);
                    buffer.set(sink.as_str());
                    state.open = false;
                    selected = Some(index);
                    response.changed = true;
                    response.submitted = true;
                }
            }
        }
        if matches > 0 && response.changed {
            state.open = true;
            state.highlighted = state.highlighted.min(matches - 1);
        }
    }
    if !has_query || !state.open || matches == 0 {
        return AutoSuggestResponse { response, selected };
    }
    let row_height = metrics.row_height;
    let visible = matches.min(5) as i32;
    let popup = Rect::new(
        rect.x,
        (rect.bottom_i32() + 2).min(ui.canvas_rect().bottom_i32()),
        rect.width,
        (visible * row_height + 4).max(0) as u32,
    );
    let id = ui.next_id();
    ui.painter.shadow(popup, 6, 8, 140);
    ui.painter.fill_rounded_rect(popup, 6, palette.surface);
    ui.painter
        .stroke_rounded_rect(popup, 6, metrics.border_width, palette.border_strong);
    let clip = ui.painter.push_clip(popup.inset(1));
    let mut y = popup.y + 2;
    let mut row_index = 0usize;
    for (index, person) in people.iter().enumerate() {
        if !person_prefix_matches(query, person) {
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
        let hit = ui.interact(
            id.child(index as u64 + 1),
            row,
            Sense::HOVER.with(Sense::CLICK),
        );
        if hit.hovered {
            state.highlighted = row_index;
        }
        if state.highlighted == row_index {
            ui.painter
                .fill_rounded_rect(row.inset_xy(2, 1), 4, palette.surface_active);
        }
        ui.painter.draw_text_ellipsis(
            row,
            person,
            palette.text,
            metrics.font_scale,
            Align::Start,
            VerticalAlign::Middle,
        );
        if hit.clicked {
            let mut out = [0u8; 96];
            let mut sink = FixedBuf::new(&mut out);
            let _ = write!(&mut sink, "@{} ", person);
            buffer.set(sink.as_str());
            state.open = false;
            selected = Some(index);
            response.changed = true;
            response.clicked = true;
        }
        y += row_height;
        row_index += 1;
    }
    ui.painter.restore_clip(clip);
    AutoSuggestResponse { response, selected }
}

fn person_prefix_matches(prefix: &str, name: &str) -> bool {
    prefix.is_empty()
        || name
            .as_bytes()
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Inherent methods mirroring the advanced free functions.
///
/// The free functions are useful inside generic helpers; the methods keep the
/// usual `ui.component(...)` calling convention.
impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A password field. See [`password_box`].
    pub fn password_box<const M: usize>(
        &mut self,
        buffer: &mut TextBuffer<M>,
        placeholder: &str,
    ) -> Response {
        password_box(self, buffer, placeholder)
    }

    /// A multiline text editor. See [`text_area`].
    pub fn text_area<const M: usize>(
        &mut self,
        buffer: &mut TextBuffer<M>,
        rows: usize,
        placeholder: &str,
    ) -> Response {
        text_area(self, buffer, rows, placeholder)
    }

    /// A two-thumb range slider. See [`range_slider`].
    pub fn range_slider(
        &mut self,
        state: &mut RangeSliderState,
        value: &mut RangeValue,
        min: f32,
        max: f32,
    ) -> Response {
        range_slider(self, state, value, min, max)
    }

    /// A rotary knob. See [`knob`].
    pub fn knob(&mut self, value: &mut f32, min: f32, max: f32, label: &str) -> Response {
        knob(self, value, min, max, label)
    }

    /// A tag editor. See [`tag_input`].
    pub fn tag_input<const M: usize, const T: usize, const W: usize>(
        &mut self,
        tags: &mut TagSet<T, W>,
        draft: &mut TextBuffer<M>,
        placeholder: &str,
    ) -> Response {
        tag_input(self, tags, draft, placeholder)
    }

    /// A file drop zone. See [`file_dropzone`].
    pub fn file_dropzone(&mut self, rect: Rect, title: &str, hint: &str) -> Response {
        file_dropzone(self, rect, title, hint)
    }

    /// Draw a dashed outline. See [`dashed_rect`].
    pub fn dashed_rect(&mut self, rect: Rect, color: Color) {
        crate::components::advanced::dashed_rect(self, rect, color)
    }

    /// A shortcut recorder. See [`hotkey_input`].
    pub fn hotkey_input(&mut self, hotkey: &mut Hotkey) -> Response {
        hotkey_input(self, hotkey)
    }

    /// A cascading multi-column selector. See [`cascader`].
    pub fn cascader<const C: usize>(
        &mut self,
        rect: Rect,
        state: &mut CascaderState<C>,
        columns: &[&[&str]],
    ) -> Response {
        cascader(self, rect, state, columns)
    }

    /// A two-pane transfer list. See [`transfer_list`].
    pub fn transfer_list<const M: usize>(
        &mut self,
        rect: Rect,
        items: &[&str],
        state: &mut TransferState<M>,
    ) -> Response {
        transfer_list(self, rect, items, state)
    }

    /// A text field with mention suggestions. See [`mention_box`].
    pub fn mention_box<const M: usize>(
        &mut self,
        state: &mut AutoSuggestState,
        buffer: &mut TextBuffer<M>,
        people: &[&str],
    ) -> crate::components::inputs::AutoSuggestResponse {
        mention_box(self, state, buffer, people)
    }
}
