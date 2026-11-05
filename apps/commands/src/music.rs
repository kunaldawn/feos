//! The Music window: a list of the sound files on the system, with transport
//! controls. Decoding runs on [`AudioTask`], so nothing here can hold up the
//! desktop. Sound keeps playing while the window is minimized.

use core::fmt::Write;

use alloc::vec::Vec;

use feos_app::{App, Context, Platform, Processes, WindowContext, WindowFrame};
use feos_console::eprintln;
use feos_fs::{FileSystem, NodeId};
use feos_gfx::{Color, Point, Rect};
use feos_gui::{Align, Event, Key, Painter, RectExt, VerticalAlign};
use feos_sync::SpinLock;
use feos_text::Text;

use crate::audio_worker::{AudioFailure, AudioTask, Source, TrackInfo, MUSIC_PIPE};
use crate::player::is_audio_file;

const MAX_TRACKS: usize = 256;
const ROW_HEIGHT: i32 = 22;
const HEADER_HEIGHT: i32 = 74;
const BAR_HEIGHT: i32 = 46;
const CHECK_MS: u64 = 500;

const BG: Color = Color::rgb(244, 246, 250);
const PANEL: Color = Color::rgb(36, 58, 104);
const PANEL_TEXT: Color = Color::WHITE;
const PANEL_MUTED: Color = Color::rgb(180, 196, 226);
const ROW_SELECTED: Color = Color::rgb(190, 212, 248);
const ROW_PLAYING: Color = Color::rgb(216, 232, 255);
const TEXT: Color = Color::rgb(24, 28, 36);
const ACCENT: Color = Color::rgb(66, 126, 226);

/// What the window holds. The window owns the state, one at a time.
struct MusicState {
    tracks: Vec<NodeId>,
    cursor: usize,
    scroll: usize,
    current: Option<usize>,
    title: Text<96>,
    info: Option<TrackInfo>,
    error: Text<96>,
    playing: bool,
    position_ms: u64,
    processes: &'static dyn Processes,
    pid: Option<u32>,
    next_check: u64,
    /// Where the seek bar and volume slider were last drawn.
    seek: Rect,
    volume: Rect,
    last_row_click: (usize, u64),
}

fn collect(fs: &dyn FileSystem, out: &mut Vec<NodeId>) {
    for index in 0..fs.node_count() {
        if out.len() >= MAX_TRACKS {
            return;
        }
        let node = NodeId(index as u32);
        if fs.is_file(node) && fs.name(node).is_some_and(is_audio_file) {
            if out.try_reserve(1).is_err() {
                return;
            }
            out.push(node);
        }
    }
}

impl MusicState {
    fn new(fs: &dyn FileSystem, processes: &'static dyn Processes) -> Self {
        let mut tracks = Vec::new();
        collect(fs, &mut tracks);
        Self {
            tracks,
            cursor: 0,
            scroll: 0,
            current: None,
            title: Text::new(),
            info: None,
            error: Text::new(),
            playing: false,
            position_ms: 0,
            processes,
            pid: None,
            next_check: 0,
            seek: Rect::new(0, 0, 0, 0),
            volume: Rect::new(0, 0, 0, 0),
            last_row_click: (usize::MAX, 0),
        }
    }

    fn ensure_task(&mut self) -> bool {
        if self.pid.is_some() {
            return true;
        }
        let identity = MUSIC_PIPE.claim();
        let mut args = Text::<24>::new();
        let _ = write!(args, "music {identity}");
        match self.processes.spawn(&AudioTask, args.as_str()) {
            Ok(pid) => {
                self.pid = Some(pid);
                true
            }
            Err(_) => false,
        }
    }

    fn open_node(&mut self, fs: &dyn FileSystem, node: NodeId) {
        self.current = self.tracks.iter().position(|&track| track == node);
        if let Some(index) = self.current {
            self.cursor = index;
        }
        self.title.set(fs.name(node).unwrap_or("?"));
        self.info = None;
        self.error.clear();
        self.position_ms = 0;
        if !self.ensure_task() {
            self.error.set("No process is free to play this file.");
            self.playing = false;
            return;
        }
        self.playing = true;
        MUSIC_PIPE.open(node, Source::File, false);
    }

    fn play_index(&mut self, fs: &dyn FileSystem, index: usize) {
        if let Some(&node) = self.tracks.get(index) {
            self.open_node(fs, node);
        }
    }

    fn toggle(&mut self, fs: &dyn FileSystem) {
        if self.current.is_none() {
            self.play_index(fs, self.cursor);
            return;
        }
        self.playing = !self.playing;
        MUSIC_PIPE.set_paused(!self.playing);
    }

    fn stop(&mut self) {
        self.playing = false;
        self.position_ms = 0;
        MUSIC_PIPE.close_media();
        self.current = None;
        self.info = None;
    }

    fn step(&mut self, fs: &dyn FileSystem, delta: isize) {
        if self.tracks.is_empty() {
            return;
        }
        let count = self.tracks.len() as isize;
        let from = self.current.unwrap_or(self.cursor) as isize;
        self.play_index(fs, (from + delta).rem_euclid(count) as usize);
    }

    fn seek_fraction(&mut self, fraction: f32) {
        if let Some(info) = self.info.filter(|info| info.duration_ms > 0) {
            let target = (info.duration_ms as f64 * fraction.clamp(0.0, 1.0) as f64) as u64;
            self.position_ms = target;
            MUSIC_PIPE.seek(target);
        }
    }

    fn tick(&mut self, fs: &dyn FileSystem, now: u64) {
        if let Some(pid) = self.pid {
            if now >= self.next_check {
                self.next_check = now + CHECK_MS;
                if self.processes.detail(pid).is_none() {
                    self.pid = None;
                    if self.current.is_some() {
                        self.error.set("The audio task has stopped.");
                        self.playing = false;
                    }
                }
            }
        }
        let report = MUSIC_PIPE.report();
        if let Some(info) = report.info {
            self.info = Some(info);
        }
        if let Some(failure) = report.failure {
            self.playing = false;
            let mut text = Text::<96>::new();
            match failure {
                AudioFailure::Mixer(error) => {
                    let _ = write!(text, "No sound output: {error:?}");
                }
                AudioFailure::Codec(error) => {
                    let _ = write!(text, "{error}");
                }
                AudioFailure::Unreadable => {
                    text.set("The file could not be read.");
                }
                AudioFailure::NoTrack => {
                    text.set("There's no sound in this file.");
                }
            }
            self.error = text;
        }
        if self.playing {
            self.position_ms = MUSIC_PIPE.position_ms();
        }
        if report.ended {
            self.step(fs, 1);
        }
    }

    fn click(&mut self, fs: &dyn FileSystem, point: Point, size: (i32, i32), now: u64) {
        let (width, height) = size;
        if contains(self.seek, point) {
            let fraction = (point.x - self.seek.x) as f32 / self.seek.width.max(1) as f32;
            self.seek_fraction(fraction);
            return;
        }
        if contains(self.volume, point) {
            let fraction = (point.x - self.volume.x) as f32 / self.volume.width.max(1) as f32;
            set_volume_fraction(fraction);
            return;
        }
        if point.y >= height - BAR_HEIGHT {
            match button_at(point, height) {
                Some(0) => self.step(fs, -1),
                Some(1) => self.toggle(fs),
                Some(2) => self.stop(),
                Some(3) => self.step(fs, 1),
                Some(4) => feos_mixer::set_muted(!feos_mixer::muted()),
                _ => {}
            }
            return;
        }
        if point.y >= HEADER_HEIGHT && point.x < width {
            let index = self.scroll + ((point.y - HEADER_HEIGHT) / ROW_HEIGHT) as usize;
            if index < self.tracks.len() {
                self.cursor = index;
                let (last, at) = self.last_row_click;
                if last == index && now.saturating_sub(at) < 420 {
                    self.play_index(fs, index);
                }
                self.last_row_click = (index, now);
            }
        }
    }

    fn key(&mut self, fs: &dyn FileSystem, key: Key, visible: usize) {
        match key {
            Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Down => self.cursor = (self.cursor + 1).min(self.tracks.len().saturating_sub(1)),
            Key::Enter => self.play_index(fs, self.cursor),
            Key::Space => self.toggle(fs),
            Key::Character('s') => self.stop(),
            Key::Character('n') => self.step(fs, 1),
            Key::Character('p') => self.step(fs, -1),
            Key::Character('m') => feos_mixer::set_muted(!feos_mixer::muted()),
            Key::Character('+') | Key::Character('=') => {
                feos_mixer::set_volume(feos_mixer::volume().saturating_add(10))
            }
            Key::Character('-') => feos_mixer::set_volume(feos_mixer::volume().saturating_sub(10)),
            Key::Left => {
                let target = self.position_ms.saturating_sub(5000);
                self.position_ms = target;
                MUSIC_PIPE.seek(target);
            }
            Key::Right => {
                let target = self.position_ms.saturating_add(5000);
                self.position_ms = target;
                MUSIC_PIPE.seek(target);
            }
            _ => {}
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if visible > 0 && self.cursor >= self.scroll + visible {
            self.scroll = self.cursor + 1 - visible;
        }
    }

    fn draw(&mut self, painter: &mut Painter<'_, '_>, width: i32, height: i32) {
        painter.fill_rect(Rect::new(0, 0, width as u32, height as u32), BG);
        painter.fill_rect(Rect::new(0, 0, width as u32, HEADER_HEIGHT as u32), PANEL);
        let title = if self.current.is_some() {
            self.title.as_str()
        } else {
            "Nothing playing"
        };
        painter.draw_text_ellipsis(
            Rect::new(12, 8, (width - 24).max(0) as u32, 20),
            title,
            PANEL_TEXT,
            2,
            Align::Start,
            VerticalAlign::Middle,
        );
        let mut detail = Text::<96>::new();
        if !self.error.is_empty() {
            detail.set(self.error.as_str());
        } else if let Some(info) = self.info {
            let _ = write!(
                detail,
                "{}  {} Hz  {} channel(s)",
                info.codec, info.sample_rate, info.channels
            );
        }
        painter.draw_text_ellipsis(
            Rect::new(12, 32, (width - 24).max(0) as u32, 14),
            detail.as_str(),
            PANEL_MUTED,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );

        // Seek bar, with the times beside it.
        let duration = self.info.map_or(0, |info| info.duration_ms);
        let mut time = Text::<32>::new();
        write_time(&mut time, self.position_ms);
        let _ = write!(time, " / ");
        write_time(&mut time, duration);
        let time_width = painter.text_width(time.as_str(), 1) + 8;
        painter.draw_text(width - 8 - time_width + 8, 52, time.as_str(), PANEL_TEXT, 1);
        self.seek = Rect::new(12, 54, (width - 32 - time_width).max(0) as u32, 8);
        painter.fill_rect(self.seek, Color::rgb(70, 94, 144));
        if duration > 0 {
            let fill = (self.seek.width as u64 * self.position_ms.min(duration) / duration) as u32;
            painter.fill_rect(Rect::new(self.seek.x, self.seek.y, fill, 8), ACCENT);
        }

        // The list.
        let rows = list_rows(height);
        for row in 0..rows {
            let index = self.scroll + row;
            let Some(&node) = self.tracks.get(index) else {
                break;
            };
            let y = HEADER_HEIGHT + row as i32 * ROW_HEIGHT;
            let rect = Rect::new(0, y, width as u32, ROW_HEIGHT as u32);
            if self.current == Some(index) {
                painter.fill_rect(rect, ROW_PLAYING);
            }
            if self.cursor == index {
                painter.fill_rect(rect, ROW_SELECTED);
            }
            let name = NAMES.with(node);
            painter.draw_text_ellipsis(
                Rect::new(12, y, (width - 24).max(0) as u32, ROW_HEIGHT as u32),
                name.as_str(),
                TEXT,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
        }
        if self.tracks.is_empty() {
            painter.draw_text_centered(
                Rect::new(0, HEADER_HEIGHT + 20, width as u32, 20),
                "No sound files on this system",
                TEXT,
                1,
            );
        }

        // Transport.
        let top = height - BAR_HEIGHT;
        painter.fill_rect(Rect::new(0, top, width as u32, BAR_HEIGHT as u32), PANEL);
        let labels = ["|<", if self.playing { "||" } else { ">" }, "[]", ">|"];
        for (index, label) in labels.into_iter().enumerate() {
            let rect = button_rect(index, height);
            painter.fill_rect(rect, Color::rgb(64, 88, 140));
            painter.draw_text_centered(rect, label, PANEL_TEXT, 2);
        }
        let mute = button_rect(4, height);
        painter.fill_rect(mute, Color::rgb(64, 88, 140));
        painter.draw_text_centered(
            mute,
            if feos_mixer::muted() { "Off" } else { "On" },
            PANEL_TEXT,
            1,
        );
        self.volume = Rect::new(mute.right_i32() + 10, top + 19, (width - mute.right_i32() - 22).max(0) as u32, 8);
        painter.fill_rect(self.volume, Color::rgb(70, 94, 144));
        let fill = self.volume.width * feos_mixer::volume() as u32 / feos_mixer::MAX_VOLUME as u32;
        painter.fill_rect(Rect::new(self.volume.x, self.volume.y, fill, 8), ACCENT);
    }
}

fn set_volume_fraction(fraction: f32) {
    let level = (fraction.clamp(0.0, 1.0) * feos_mixer::MAX_VOLUME as f32) as u8;
    feos_mixer::set_volume(level);
    feos_mixer::set_muted(false);
}

fn contains(rect: Rect, point: Point) -> bool {
    !rect.is_empty()
        && point.x >= rect.x
        && point.x < rect.right_i32()
        && point.y >= rect.y - 4
        && point.y < rect.bottom_i32() + 4
}

fn list_rows(height: i32) -> usize {
    ((height - HEADER_HEIGHT - BAR_HEIGHT).max(0) / ROW_HEIGHT) as usize
}

fn button_rect(index: usize, height: i32) -> Rect {
    Rect::new(8 + index as i32 * 44, height - BAR_HEIGHT + 7, 38, 32)
}

fn button_at(point: Point, height: i32) -> Option<usize> {
    (0..5).find(|&index| {
        let rect = button_rect(index, height);
        point.x >= rect.x
            && point.x < rect.right_i32()
            && point.y >= rect.y
            && point.y < rect.bottom_i32()
    })
}

fn write_time(out: &mut Text<32>, ms: u64) {
    let seconds = ms / 1000;
    let _ = write!(out, "{}:{:02}", seconds / 60, seconds % 60);
}

/// Looks names up for the list, which only has a node.
struct Names(SpinLock<Option<&'static dyn FileSystem>>);

impl Names {
    fn with(&self, node: NodeId) -> Text<96> {
        let mut text = Text::new();
        if let Some(fs) = *self.0.lock() {
            text.set(fs.name(node).unwrap_or("?"));
        }
        text
    }
}

static NAMES: Names = Names(SpinLock::new(None));
static WINDOW: SpinLock<Option<MusicState>> = SpinLock::new(None);

/// The Music application.
pub struct Music;

impl App for Music {
    fn name(&self) -> &'static str {
        "music"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["music-player"]
    }

    fn usage(&self) -> &'static str {
        "music [file]"
    }

    fn description(&self) -> &'static str {
        "play sound files in a window"
    }

    fn foreground(&self) -> bool {
        true
    }

    fn windowed(&self) -> bool {
        true
    }

    fn animates(&self) -> bool {
        true
    }

    fn animation_period_ms(&self) -> u32 {
        100
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        let Some(fs) = context.platform.filesystem else {
            return false;
        };
        *NAMES.0.lock() = Some(fs);
        let mut state = MusicState::new(fs, context.platform.processes);
        let path = context.args.trim();
        if !path.is_empty() {
            match fs.resolve(path).filter(|&node| fs.is_file(node)) {
                Some(node) => {
                    if !state.tracks.contains(&node) && state.tracks.try_reserve(1).is_ok() {
                        state.tracks.push(node);
                    }
                    state.open_node(fs, node);
                }
                None => {
                    state.error.set("The requested file was not found.");
                }
            }
        }
        *WINDOW.lock() = Some(state);
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        let Some(fs) = context.platform.filesystem else {
            return WindowFrame::Close;
        };
        let mut guard = WINDOW.lock();
        let Some(state) = guard.as_mut() else {
            return WindowFrame::Close;
        };
        let now = context.now_ms;
        let width = context.canvas.width() as i32;
        let height = context.canvas.height() as i32;
        let visible = list_rows(height);
        context.dispatch_keys(|event| {
            if let Event::KeyDown(key, _) = event {
                state.key(fs, key, visible);
            }
        });
        context.dispatch_pointer(|report| {
            let point = Point::new(report.x as i32, report.y as i32);
            if report.left == Some(true) {
                state.click(fs, point, (width, height), now);
            }
            if report.wheel != 0 {
                let max = state.tracks.len().saturating_sub(visible);
                state.scroll = if report.wheel > 0 {
                    state.scroll.saturating_sub(3)
                } else {
                    (state.scroll + 3).min(max)
                };
            }
        });
        state.tick(fs, now);
        let mut painter = Painter::new(context.canvas);
        state.draw(&mut painter, width, height);
        WindowFrame::Continue
    }

    fn close_window(&self) {
        MUSIC_PIPE.shut_down();
        *WINDOW.lock() = None;
    }

    fn background_tick(&self, platform: &Platform, now_ms: u64) {
        let (Some(fs), Some(mut guard)) = (platform.filesystem, WINDOW.try_lock()) else {
            return;
        };
        if let Some(state) = guard.as_mut() {
            state.tick(fs, now_ms);
        }
    }

    fn accept_drop(&self, payload: &str) -> bool {
        let (Some(mut guard), Some(fs)) = (WINDOW.try_lock(), *NAMES.0.lock()) else {
            return false;
        };
        let Some(state) = guard.as_mut() else {
            return false;
        };
        match fs.resolve(payload).filter(|&node| fs.is_file(node)) {
            Some(node) => {
                if !state.tracks.contains(&node) && state.tracks.try_reserve(1).is_ok() {
                    state.tracks.push(node);
                }
                state.open_node(fs, node);
                true
            }
            None => false,
        }
    }

    fn run(&self, _context: &mut Context<'_>) -> i32 {
        eprintln!("music: this is a desktop window; use `play <file>` here");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_do_not_overlap() {
        let (a, b) = (button_rect(0, 400), button_rect(1, 400));
        assert!(a.right_i32() <= b.x);
        assert_eq!(button_at(Point::new(a.x + 1, a.y + 1), 400), Some(0));
        assert_eq!(button_at(Point::new(0, 0), 400), None);
    }

    #[test]
    fn times_read_as_minutes_and_seconds() {
        let mut text = Text::<32>::new();
        write_time(&mut text, 83_000);
        assert_eq!(text.as_str(), "1:23");
    }

    #[test]
    fn a_small_window_lists_nothing_rather_than_going_negative() {
        assert_eq!(list_rows(10), 0);
        assert_eq!(list_rows(HEADER_HEIGHT + BAR_HEIGHT + ROW_HEIGHT * 3), 3);
    }
}
