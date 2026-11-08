//! The Settings window: what the desktop session remembers from one boot to the next.
//!
//! The session owns the settings, because it's what they change. This app only asks: it
//! posts [`shell::request_setting`], the session applies and saves it, and republishes the
//! lot, which is what this window shows. Keyboard: Up and Down pick a row, Left and Right
//! change it. Pointer: the arrows at each end of a row.

use alloc::string::String;
use alloc::vec::Vec;

use feos_app::{shell, App, Context, WindowContext, WindowFrame};
use feos_console::eprintln;
use feos_desktop::{KeyboardLayout, ThemeName, WallpaperPreset};
use feos_gfx::{Canvas, Color, Rect};
use feos_gui::{Align, Event, Key, Painter, VerticalAlign};
use feos_sync::SpinLock;

const ROW_HEIGHT: i32 = 34;
const ROW_TOP: i32 = 44;
const MARGIN: i32 = 14;
const ARROW: i32 = 26;
const LABEL_WIDTH: i32 = 130;

const BACKGROUND: Color = Color::rgb(236, 233, 216);
const TEXT: Color = Color::rgb(0, 0, 0);
const MUTED: Color = Color::rgb(80, 80, 80);
const SELECTED: Color = Color::rgb(49, 106, 197);
const FIELD: Color = Color::rgb(255, 255, 255);
const EDGE: Color = Color::rgb(127, 157, 185);

/// One thing that can be set: a key in the session's settings, and its possible values.
struct Row {
    label: &'static str,
    key: &'static str,
    values: Vec<String>,
}

/// What the window shows and keeps between frames.
struct State {
    rows: Vec<Row>,
    selected: usize,
    /// The settings as last published, parsed into `values` once for each change.
    seen: String,
    current: Vec<usize>,
}

static STATE: SpinLock<Option<State>> = SpinLock::new(None);

/// Everything the session accepts, in a fixed order.
fn build_rows(modes: &str) -> Vec<Row> {
    let mut rows = alloc::vec![
        Row {
            label: "Colour scheme",
            key: "theme",
            values: ThemeName::ALL.iter().map(|theme| String::from(theme.name())).collect(),
        },
        Row {
            label: "Keyboard layout",
            key: "keyboard",
            values: KeyboardLayout::ALL.iter().map(|layout| String::from(layout.name())).collect(),
        },
        Row {
            label: "Wallpaper",
            key: "wallpaper",
            values: WallpaperPreset::ALL.iter().map(|preset| String::from(preset.name())).collect(),
        },
        Row {
            label: "Clock",
            key: "clock",
            values: alloc::vec![String::from("24"), String::from("12")],
        },
    ];
    // Only what this machine's display can do, once the session has said.
    let values: Vec<String> = modes.split_whitespace().map(String::from).collect();
    if !values.is_empty() {
        rows.push(Row {
            label: "Resolution",
            key: "resolution",
            values,
        });
    }
    rows
}

impl State {
    fn new() -> Self {
        let mut buffer = [0u8; shell::SETTINGS_MAX];
        let modes = String::from(shell::display_modes(&mut buffer));
        let rows = build_rows(&modes);
        let current = alloc::vec![0; rows.len()];
        Self {
            rows,
            selected: 0,
            seen: String::new(),
            current,
        }
    }

    /// Follows what the session last published, whoever changed it.
    fn refresh(&mut self) {
        let mut buffer = [0u8; shell::SETTINGS_MAX];
        let text = shell::settings(&mut buffer);
        if text == self.seen {
            return;
        }
        self.seen = String::from(text);
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if let Some(index) = self.rows.iter().position(|row| row.key == key) {
                if let Some(at) = self.rows[index].values.iter().position(|known| known == value) {
                    self.current[index] = at;
                }
            }
        }
    }

    /// Steps a row's value and asks the session for it.
    fn change(&mut self, row: usize, forward: bool) {
        let count = self.rows[row].values.len();
        if count == 0 {
            return;
        }
        let at = self.current[row];
        let next = if forward { (at + 1) % count } else { (at + count - 1) % count };
        // Shown at once. The session's answer arrives with a frame of its own.
        self.current[row] = next;
        // Stepping through screen sizes shouldn't try each one: Enter applies that.
        if self.rows[row].key != "resolution" {
            self.apply(row);
        }
    }

    /// Asks the session for what a row shows.
    fn apply(&self, row: usize) {
        let line = &self.rows[row];
        if let Some(value) = line.values.get(self.current[row]) {
            shell::request_setting(line.key, value);
        }
    }
}

fn row_rect(canvas: &Canvas<'_>, row: usize) -> Rect {
    Rect::new(
        MARGIN,
        ROW_TOP + row as i32 * ROW_HEIGHT,
        (canvas.width() as i32 - MARGIN * 2).max(0) as u32,
        (ROW_HEIGHT - 6) as u32,
    )
}

/// The two arrows of a row, and the value between them.
fn row_parts(row: Rect) -> (Rect, Rect, Rect) {
    let left_x = row.x + LABEL_WIDTH;
    let right_x = row.x + row.width as i32 - ARROW;
    let left = Rect::new(left_x, row.y, ARROW as u32, row.height);
    let right = Rect::new(right_x, row.y, ARROW as u32, row.height);
    let value = Rect::new(left_x + ARROW, row.y, (right_x - left_x - ARROW).max(0) as u32, row.height);
    (left, value, right)
}

fn draw(canvas: &mut Canvas<'_>, state: &State) {
    let bounds = Rect::new(0, 0, canvas.width() as u32, canvas.height() as u32);
    canvas.fill_rect(bounds, BACKGROUND);
    let rows: Vec<Rect> = (0..state.rows.len()).map(|row| row_rect(canvas, row)).collect();
    let mut painter = Painter::new(canvas);
    painter.draw_text(MARGIN, 14, "Settings", TEXT, 2);
    for (index, (row, area)) in state.rows.iter().zip(rows).enumerate() {
        let focused = index == state.selected;
        let label = Rect::new(area.x, area.y, (LABEL_WIDTH - 6) as u32, area.height);
        painter.draw_text_ellipsis(
            label,
            row.label,
            if focused { SELECTED } else { TEXT },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        let (left, value, right) = row_parts(area);
        painter.fill_rect(value, FIELD);
        painter.stroke_rect(value, 1, if focused { SELECTED } else { EDGE });
        let shown = row.values.get(state.current[index]).map_or("", String::as_str);
        painter.draw_text_centered(value, shown, TEXT, 1);
        for (rect, arrow) in [(left, "<"), (right, ">")] {
            painter.fill_rect(rect, FIELD);
            painter.stroke_rect(rect, 1, EDGE);
            painter.draw_text_centered(rect, arrow, TEXT, 1);
        }
    }
    let hint = Rect::new(MARGIN, ROW_TOP + state.rows.len() as i32 * ROW_HEIGHT + 4, 400, 18);
    painter.draw_text_ellipsis(
        hint,
        "Saved at once. Enter applies a resolution.",
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
}

fn handle_keys(state: &mut State, context: &WindowContext<'_, '_>) {
    let count = state.rows.len();
    let mut pressed = Vec::new();
    context.dispatch_keys(|event| {
        if let Event::KeyDown(key, _) = event {
            pressed.push(key);
        }
    });
    for key in pressed {
        match key {
            Key::Up if count > 0 => state.selected = (state.selected + count - 1) % count,
            Key::Down if count > 0 => state.selected = (state.selected + 1) % count,
            Key::Left if count > 0 => state.change(state.selected, false),
            Key::Right if count > 0 => state.change(state.selected, true),
            Key::Enter if count > 0 && state.rows[state.selected].key == "resolution" => {
                state.apply(state.selected);
            }
            _ => {}
        }
    }
}

fn handle_pointer(state: &mut State, context: &WindowContext<'_, '_>) {
    let mut clicks = Vec::new();
    context.dispatch_pointer(|report| {
        if report.left == Some(true) {
            clicks.push((report.x as i32, report.y as i32));
        }
    });
    for (x, y) in clicks {
        for row in 0..state.rows.len() {
            let (left, value, right) = row_parts(row_rect(context.canvas, row));
            let hit = |rect: Rect| {
                x >= rect.x
                    && x < rect.x + rect.width as i32
                    && y >= rect.y
                    && y < rect.y + rect.height as i32
            };
            if hit(left) {
                state.selected = row;
                state.change(row, false);
            } else if hit(right) {
                state.selected = row;
                state.change(row, true);
            } else if hit(value) && state.rows[row].key == "resolution" {
                state.selected = row;
                state.apply(row);
            }
        }
    }
}

/// The `settings` application.
pub struct Settings;

impl App for Settings {
    fn name(&self) -> &'static str {
        "settings"
    }

    fn usage(&self) -> &'static str {
        "settings"
    }

    fn description(&self) -> &'static str {
        "change and keep the desktop's settings"
    }

    fn windowed(&self) -> bool {
        true
    }

    fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
        *STATE.lock() = Some(State::new());
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        let mut guard = STATE.lock();
        let Some(state) = guard.as_mut() else {
            return WindowFrame::Close;
        };
        state.refresh();
        handle_keys(state, context);
        handle_pointer(state, context);
        draw(context.canvas, state);
        WindowFrame::Continue
    }

    fn close_window(&self) {
        *STATE.lock() = None;
    }

    fn run(&self, _context: &mut Context<'_>) -> i32 {
        eprintln!("settings: open it from the desktop");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_can_be_stepped_around_and_comes_back() {
        let mut state = State {
            rows: build_rows("800x600 1024x768"),
            selected: 0,
            seen: String::new(),
            current: alloc::vec![0; 5],
        };
        assert_eq!(state.rows.len(), 5);
        for row in 0..state.rows.len() {
            let start = state.current[row];
            state.change(row, false);
            state.change(row, true);
            assert_eq!(state.current[row], start);
            for _ in 0..state.rows[row].values.len() {
                state.change(row, true);
            }
            assert_eq!(state.current[row], start);
        }
        while shell::take_request().is_some() {}
    }

    #[test]
    fn no_modes_means_no_resolution_row() {
        assert!(build_rows("").iter().all(|row| row.key != "resolution"));
    }
}
