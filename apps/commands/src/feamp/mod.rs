//! FeAmp: a classic skinned music player, in the style of the late-90s
//! players that sat in a corner of everyone's desktop.
//!
//! One frameless window holds the three classic panels, stacked: the player
//! (LED time, scrolling title, spectrum, transport, volume and balance), the
//! ten-band equaliser, and the playlist. FeAmp draws its own title bars, so it
//! moves, shades, minimizes and resizes itself through
//! [`WindowRequest`](feos_app::WindowRequest).
//!
//! Sound comes from [`AudioTask`] on [`FEAMP_PIPE`], which also runs the
//! equaliser. The little spectrum and the [`vis`] window (MilkDrop presets)
//! both read what's playing from the mixer's scope.
//!
//! Keys, as they always were: Z X C V B for previous, play, pause, stop and
//! next; arrows seek and change the volume; S shuffle, R repeat, L add files;
//! Ctrl+D double size, Ctrl+W shade, Alt+G equaliser, Alt+E playlist, and
//! Ctrl+K (or Ctrl+Shift+K) the visualiser.

mod font;
pub mod skin;
pub mod tags;
pub mod vis;

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;

use feos_app::{shell, App, Context, Platform, Processes, WindowContext, WindowFrame, WindowRequest};
use feos_console::eprintln;
use feos_fs::{FileSystem, NodeId};
use feos_gfx::{Canvas, Point};
use feos_gui::{Event, Key, Modifiers, MouseButton};
use feos_milkdrop::Analyzer;
use feos_sync::SpinLock;
use feos_text::Text;

use crate::audio_worker::{AudioFailure, AudioTask, Source, TrackInfo, FEAMP_PIPE};
use crate::equalizer::{EqSettings, PRESETS, RANGE_DB};
use crate::player::is_audio_file;
use skin::{Control, Layout, PL_ROWS, PL_ROW_H, VIS_BARS, WIDTH};

/// Most tracks in the playlist.
const MAX_TRACKS: usize = 512;
/// How often the audio task is checked on.
const CHECK_MS: u64 = 500;
/// How long a message replaces the title.
const MESSAGE_MS: u64 = 2_000;
/// Two clicks this close are a double click.
const DOUBLE_CLICK_MS: u64 = 400;
/// Frames of the mixer's scope read for the spectrum.
const SCOPE_FRAMES: usize = 576;
/// The marquee moves a pixel this often.
const MARQUEE_MS: u64 = 45;

/// Whether FeAmp is playing or not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Nothing loaded.
    Stopped,
    /// Sound is coming out.
    Playing,
    /// Held where it was.
    Paused,
}

/// What the small display under the time shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MiniVis {
    /// Bars, with falling peaks.
    Spectrum,
    /// The waveform.
    Scope,
    /// Nothing.
    Off,
}

/// One playlist entry.
#[derive(Clone, Copy, Debug)]
pub struct Track {
    /// The file.
    pub node: NodeId,
    /// "Artist - Title" from its tag, or its file name.
    pub name: Text<96>,
    /// How long it lasts, or 0 if not known yet.
    pub duration_ms: u64,
    /// Its size in bytes.
    pub size: u64,
}

/// What the left button is holding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drag {
    /// The window, by a title bar, last seen at this screen point.
    Move(Point),
    /// A slider or the playlist's scroll bar.
    Slider(Control),
}

/// All of FeAmp's state.
pub struct Player {
    fs: &'static dyn FileSystem,
    processes: &'static dyn Processes,
    tracks: Vec<Track>,
    current: Option<usize>,
    cursor: usize,
    scroll: usize,
    status: Status,
    position_ms: u64,
    info: Option<TrackInfo>,
    pid: Option<u32>,
    next_check: u64,
    shuffle: bool,
    repeat: bool,
    eq: EqSettings,
    eq_auto: bool,
    eq_preset: usize,
    show_eq: bool,
    show_pl: bool,
    shaded: bool,
    double: bool,
    remaining: bool,
    scroll_title: bool,
    vis_mode: MiniVis,
    vis_open: bool,
    focused: bool,
    drag: Option<Drag>,
    pressed: Option<Control>,
    seek_preview: Option<f32>,
    message: Text<64>,
    message_until: u64,
    marquee_start: u64,
    now: u64,
    last_click: (Option<Control>, usize, u64),
    analyzer: Option<Analyzer>,
    scope: Vec<i16>,
    scope_count: u64,
    scope_line: [f32; 76],
    bars: [f32; VIS_BARS],
    peaks: [f32; VIS_BARS],
    rng: u64,
}

impl Player {
    /// A player with every sound file on the system in its playlist.
    pub fn new(fs: &'static dyn FileSystem, processes: &'static dyn Processes) -> Self {
        let mut scope = Vec::new();
        if scope.try_reserve_exact(SCOPE_FRAMES * 2).is_ok() {
            scope.resize(SCOPE_FRAMES * 2, 0);
        }
        let mut player = Self {
            fs,
            processes,
            tracks: Vec::new(),
            current: None,
            cursor: 0,
            scroll: 0,
            status: Status::Stopped,
            position_ms: 0,
            info: None,
            pid: None,
            next_check: 0,
            shuffle: false,
            repeat: true,
            eq: EqSettings::FLAT,
            eq_auto: false,
            eq_preset: 0,
            show_eq: true,
            show_pl: true,
            shaded: false,
            double: false,
            remaining: false,
            scroll_title: true,
            vis_mode: MiniVis::Spectrum,
            vis_open: false,
            focused: true,
            drag: None,
            pressed: None,
            seek_preview: None,
            message: Text::new(),
            message_until: 0,
            marquee_start: 0,
            now: 0,
            last_click: (None, 0, 0),
            analyzer: Analyzer::new(48_000),
            scope,
            scope_count: 0,
            scope_line: [0.0; 76],
            bars: [0.0; VIS_BARS],
            peaks: [0.0; VIS_BARS],
            rng: 0x2545_f491_4f6c_dd1d,
        };
        player.scan();
        player
    }

    /// Where the panels are, for what's showing.
    pub fn layout(&self) -> Layout {
        Layout::new(self.shaded, self.show_eq, self.show_pl)
    }

    fn scale(&self) -> i32 {
        if self.double {
            2
        } else {
            1
        }
    }

    /// The window's size for the panels showing.
    pub fn window_size(&self) -> (u32, u32) {
        let scale = self.scale() as u32;
        (WIDTH as u32 * scale, self.layout().height as u32 * scale)
    }

    // The playlist ------------------------------------------------------------

    /// Adds every sound file on the system that isn't in the list already.
    fn scan(&mut self) -> usize {
        let before = self.tracks.len();
        let mut found = Vec::new();
        let tracks = &self.tracks;
        walk_files(self.fs, self.fs.root(), |node, name| {
            if is_audio_file(name) && !tracks.iter().any(|track| track.node == node) && found.try_reserve(1).is_ok() {
                found.push(node);
            }
        });
        let mut added: Vec<Track> = Vec::new();
        for node in found {
            if self.tracks.len() + added.len() >= MAX_TRACKS || added.try_reserve(1).is_err() {
                break;
            }
            added.push(self.describe(node));
        }
        added.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        if self.tracks.try_reserve(added.len()).is_ok() {
            self.tracks.extend(added);
        }
        self.tracks.len() - before
    }

    /// What the playlist shows for `node`.
    fn describe(&self, node: NodeId) -> Track {
        let file_name = self.fs.name(node).unwrap_or("?");
        let size = self.fs.size(node) as u64;
        let mut name = Text::<96>::new();
        let mut duration_ms = 0;
        let mut head = Vec::new();
        let want = tags::TAG_READ_MAX.min(size as usize);
        if head.try_reserve_exact(want).is_ok() {
            head.resize(want, 0);
            let got = self.fs.read(node, 0, &mut head);
            head.truncate(got);
            let found = tags::read_id3(&head);
            if !found.title.is_empty() {
                if !found.artist.is_empty() {
                    let _ = write!(name, "{} - {}", found.artist.as_str(), found.title.as_str());
                } else {
                    name.set(found.title.as_str());
                }
            }
            let lower_mp3 = file_name.len() > 4 && file_name[file_name.len() - 4..].eq_ignore_ascii_case(".mp3");
            if lower_mp3 {
                duration_ms = tags::mp3_duration_ms(&head, size).unwrap_or(0);
            }
        }
        if name.is_empty() {
            // Untagged: the file's own name, extension and all, so two
            // `tone`s in different formats can be told apart.
            name.set(file_name);
        }
        Track {
            node,
            name,
            duration_ms,
            size,
        }
    }

    /// Adds `node` if it's not there, and returns where it is.
    fn add(&mut self, node: NodeId) -> Option<usize> {
        if let Some(index) = self.tracks.iter().position(|track| track.node == node) {
            return Some(index);
        }
        if self.tracks.len() >= MAX_TRACKS || self.tracks.try_reserve(1).is_err() {
            return None;
        }
        let track = self.describe(node);
        self.tracks.push(track);
        Some(self.tracks.len() - 1)
    }

    fn remove(&mut self, index: usize) {
        if index >= self.tracks.len() {
            return;
        }
        self.tracks.remove(index);
        match self.current {
            Some(current) if current == index => {
                self.stop();
            }
            Some(current) if current > index => self.current = Some(current - 1),
            _ => {}
        }
        self.cursor = self.cursor.min(self.tracks.len().saturating_sub(1));
        self.keep_cursor_visible();
    }

    fn sort(&mut self) {
        let current = self.current.map(|index| self.tracks[index].node);
        self.tracks.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        self.current = current.and_then(|node| self.tracks.iter().position(|track| track.node == node));
    }

    fn keep_cursor_visible(&mut self) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + PL_ROWS {
            self.scroll = self.cursor + 1 - PL_ROWS;
        }
        self.scroll = self.scroll.min(self.tracks.len().saturating_sub(PL_ROWS));
    }

    /// The playlist's total length, of what's known.
    pub fn total_ms(&self) -> u64 {
        self.tracks.iter().map(|track| track.duration_ms).sum()
    }

    // Playback ----------------------------------------------------------------

    fn ensure_task(&mut self) -> bool {
        if self.pid.is_some() {
            return true;
        }
        let identity = FEAMP_PIPE.claim();
        let mut args = Text::<24>::new();
        let _ = write!(args, "feamp {identity}");
        match self.processes.spawn(&AudioTask, args.as_str()) {
            Ok(pid) => {
                self.pid = Some(pid);
                FEAMP_PIPE.set_eq(self.eq);
                true
            }
            Err(_) => false,
        }
    }

    fn play_index(&mut self, index: usize) {
        let Some(track) = self.tracks.get(index).copied() else {
            return;
        };
        if !self.ensure_task() {
            self.say("NO PROCESS IS FREE TO PLAY THIS");
            return;
        }
        self.current = Some(index);
        self.cursor = index;
        self.keep_cursor_visible();
        self.info = None;
        self.position_ms = 0;
        self.status = Status::Playing;
        self.marquee_start = self.now;
        FEAMP_PIPE.set_eq(self.eq);
        FEAMP_PIPE.open(track.node, Source::File, false);
    }

    fn play(&mut self) {
        match (self.status, self.current) {
            (Status::Paused, Some(_)) => self.pause(),
            (_, Some(index)) => self.play_index(index),
            (_, None) => self.play_index(self.cursor),
        }
    }

    fn pause(&mut self) {
        match self.status {
            Status::Playing => {
                self.status = Status::Paused;
                FEAMP_PIPE.set_paused(true);
            }
            Status::Paused => {
                self.status = Status::Playing;
                FEAMP_PIPE.set_paused(false);
            }
            Status::Stopped => {}
        }
    }

    fn stop(&mut self) {
        self.status = Status::Stopped;
        self.position_ms = 0;
        FEAMP_PIPE.close_media();
    }

    fn random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// Moves `delta` tracks along, or to a random one in shuffle. Returns
    /// whether there was somewhere to go.
    fn step(&mut self, delta: isize, wrap: bool) -> bool {
        let count = self.tracks.len();
        if count == 0 {
            return false;
        }
        let from = self.current.unwrap_or(self.cursor);
        let next = if self.shuffle && count > 1 {
            let mut pick = (self.random() % count as u64) as usize;
            if pick == from {
                pick = (pick + 1) % count;
            }
            pick
        } else {
            let target = from as isize + delta;
            if !wrap && (target < 0 || target >= count as isize) {
                return false;
            }
            target.rem_euclid(count as isize) as usize
        };
        self.play_index(next);
        true
    }

    fn seek_to(&mut self, ms: u64) {
        if self.status == Status::Stopped {
            return;
        }
        let duration = self.duration_ms();
        let ms = if duration > 0 { ms.min(duration.saturating_sub(500)) } else { ms };
        self.position_ms = ms;
        FEAMP_PIPE.seek(ms);
    }

    /// The playing track's length.
    pub fn duration_ms(&self) -> u64 {
        let known = self.current.and_then(|index| self.tracks.get(index)).map_or(0, |track| track.duration_ms);
        if known > 0 {
            known
        } else {
            self.info.map_or(0, |info| info.duration_ms)
        }
    }

    /// The playing file's average bitrate.
    pub fn bitrate_kbps(&self) -> u64 {
        let duration = self.duration_ms();
        let size = self.current.and_then(|index| self.tracks.get(index)).map_or(0, |track| track.size);
        (size * 8).checked_div(duration).unwrap_or(0)
    }

    /// The master volume, 0 to 1.
    pub fn volume_fraction(&self) -> f32 {
        feos_mixer::volume() as f32 / feos_mixer::MAX_VOLUME as f32
    }

    fn set_volume(&mut self, fraction: f32) {
        let level = (fraction.clamp(0.0, 1.0) * feos_mixer::MAX_VOLUME as f32 + 0.5) as u8;
        feos_mixer::set_volume(level);
        feos_mixer::set_muted(false);
        let mut text = Text::<64>::new();
        let _ = write!(text, "VOLUME: {}%", level as u32 * 100 / feos_mixer::MAX_VOLUME as u32);
        self.say(text.as_str());
    }

    fn set_balance(&mut self, balance: f32) {
        // It snaps to the middle, as it always did.
        let balance = if balance.abs() < 0.08 { 0.0 } else { balance.clamp(-1.0, 1.0) };
        self.eq.balance = balance;
        FEAMP_PIPE.set_eq(self.eq);
        let mut text = Text::<64>::new();
        let percent = (balance.abs() * 100.0 + 0.5) as u32;
        match balance {
            b if b < 0.0 => {
                let _ = write!(text, "BALANCE: {percent}% LEFT");
            }
            b if b > 0.0 => {
                let _ = write!(text, "BALANCE: {percent}% RIGHT");
            }
            _ => {
                let _ = write!(text, "BALANCE: CENTER");
            }
        }
        self.say(text.as_str());
    }

    fn set_band(&mut self, band: Option<usize>, db: f32) {
        let db = if db.abs() < 0.6 { 0.0 } else { db.clamp(-RANGE_DB, RANGE_DB) };
        match band {
            Some(band) => self.eq.bands_db[band] = db,
            None => self.eq.preamp_db = db,
        }
        if !self.eq.enabled {
            self.eq.enabled = true;
        }
        FEAMP_PIPE.set_eq(self.eq);
        let mut text = Text::<64>::new();
        match band {
            Some(band) => {
                let hz = crate::equalizer::BAND_HZ[band] as u32;
                if hz >= 1000 {
                    let _ = write!(text, "EQ: {}KHZ: {:+.1} DB", hz / 1000, db);
                } else {
                    let _ = write!(text, "EQ: {}HZ: {:+.1} DB", hz, db);
                }
            }
            None => {
                let _ = write!(text, "EQ: PREAMP: {:+.1} DB", db);
            }
        }
        self.say(text.as_str());
    }

    /// Shows `text` in place of the title for a moment.
    fn say(&mut self, text: &str) {
        self.message.set(text);
        self.message_until = self.now + MESSAGE_MS;
    }

    /// Keeps up with the audio task, and the sound for the visualiser.
    pub fn tick(&mut self, now: u64) {
        self.now = now;
        if let Some(pid) = self.pid {
            if now >= self.next_check {
                self.next_check = now + CHECK_MS;
                if self.processes.detail(pid).is_none() {
                    self.pid = None;
                    if self.status != Status::Stopped {
                        self.status = Status::Stopped;
                        self.say("THE AUDIO TASK HAS STOPPED");
                    }
                }
            }
        }
        let report = FEAMP_PIPE.report();
        if let Some(info) = report.info {
            self.info = Some(info);
            if let Some(track) = self.current.and_then(|index| self.tracks.get_mut(index)) {
                if track.duration_ms == 0 {
                    track.duration_ms = info.duration_ms;
                }
            }
        }
        if let Some(failure) = report.failure {
            let mut text = Text::<64>::new();
            match failure {
                AudioFailure::Mixer(error) => {
                    let _ = write!(text, "NO SOUND OUTPUT: {error:?}");
                }
                AudioFailure::Codec(error) => {
                    let _ = write!(text, "CAN'T DECODE: {error}");
                }
                AudioFailure::Unreadable => {
                    text.set("THE FILE COULD NOT BE READ");
                }
                AudioFailure::NoTrack => {
                    text.set("THERE'S NO SOUND IN THIS FILE");
                }
            }
            self.status = Status::Stopped;
            self.message = text;
            self.message_until = now + MESSAGE_MS * 2;
        }
        if self.status == Status::Playing {
            self.position_ms = FEAMP_PIPE.position_ms();
        }
        if report.ended && self.status == Status::Playing && !self.step(1, self.repeat) {
            self.stop();
        }
    }

    /// Reads what's being heard, for the small display.
    fn listen(&mut self) {
        let fresh = if self.scope.is_empty() {
            false
        } else {
            let count = feos_mixer::scope(&mut self.scope);
            let fresh = count != self.scope_count;
            self.scope_count = count;
            fresh
        };
        let hearing = fresh && self.status == Status::Playing;
        if hearing {
            if let Some(analyzer) = self.analyzer.as_mut() {
                analyzer.update(&self.scope, 20.0);
            }
        }
        // Bars on a log scale of frequency, from ~50 Hz to ~19 kHz.
        let spectrum = self.analyzer.as_ref().map(|analyzer| &analyzer.spectrum[..]);
        for bar in 0..VIS_BARS {
            let edge = |i: usize| feos_math::pow(400.0, i as f64 / VIS_BARS as f64) as usize;
            let (low, high) = (edge(bar).max(1), edge(bar + 1).max(edge(bar).max(1) + 1));
            let level = match (hearing, spectrum) {
                (true, Some(spectrum)) => {
                    let most = spectrum[low.min(511)..high.min(512)].iter().fold(0.0f32, |a, b| a.max(*b));
                    // A gentle tilt up for the treble, which carries less energy.
                    (feos_math::f32::sqrt(most) * (16.0 + bar as f32 * 0.8)).min(16.0)
                }
                _ => 0.0,
            };
            self.bars[bar] = level.max(self.bars[bar] - 1.4);
            if self.bars[bar] >= self.peaks[bar] {
                self.peaks[bar] = self.bars[bar];
            } else {
                self.peaks[bar] = (self.peaks[bar] - 0.35).max(0.0);
            }
        }
        for (column, value) in self.scope_line.iter_mut().enumerate() {
            *value = if hearing {
                let at = column * SCOPE_FRAMES / 76 * 2;
                (self.scope[at] as f32 + self.scope[at + 1] as f32) / 65536.0
            } else {
                *value * 0.6
            };
        }
    }

    /// The marquee's text: a message, or the playing track.
    pub fn marquee_text<const N: usize>(&self, out: &mut Text<N>) {
        if self.now < self.message_until {
            out.set(self.message.as_str());
            return;
        }
        let index = self.current.unwrap_or(self.cursor);
        match self.tracks.get(index) {
            Some(track) => {
                let _ = write!(out, "{}. {}", index + 1, track.name.as_str());
                let duration = if self.current == Some(index) { self.duration_ms() } else { track.duration_ms };
                if duration > 0 {
                    let _ = write!(out, " (");
                    skin::write_time(out, duration);
                    let _ = write!(out, ")");
                }
                let _ = write!(out, "  ***  ");
            }
            None => {
                out.set("FEAMP 1.0 - PRESS ADD OR DROP SOME MUSIC IN  ***  ");
            }
        }
    }

    /// How far the marquee has scrolled, for text `width` in a box `room` wide.
    pub fn marquee_offset(&self, width: i32, room: i32) -> i32 {
        if width <= room || !self.scroll_title || self.now < self.message_until {
            return 0;
        }
        ((self.now.saturating_sub(self.marquee_start) / MARQUEE_MS) % width as u64) as i32
    }

    // Input --------------------------------------------------------------------

    /// The left button went down at `local` (1x panel coordinates), `screen`
    /// on the screen.
    fn press(&mut self, local: Point, screen: Point, request: &mut WindowRequest) {
        let layout = self.layout();
        let Some(control) = skin::hit(&layout, local.x, local.y) else {
            return;
        };
        let (last_control, last_row, last_at) = self.last_click;
        let row = self.row_at(&layout, local.y);
        let double = last_control == Some(control) && last_row == row && self.now.saturating_sub(last_at) < DOUBLE_CLICK_MS;
        self.last_click = (Some(control), row, self.now);
        match control {
            Control::MainTitle | Control::EqTitle | Control::PlTitle => {
                if double && control == Control::MainTitle {
                    self.shaded = !self.shaded;
                } else {
                    self.drag = Some(Drag::Move(screen));
                }
            }
            Control::Volume | Control::Balance | Control::Seek | Control::Preamp | Control::Band(_) | Control::PlScroll => {
                self.pressed = Some(control);
                self.drag = Some(Drag::Slider(control));
                self.slide(control, local);
            }
            Control::PlRows => {
                if row < self.tracks.len() {
                    self.cursor = row;
                    if double {
                        self.play_index(row);
                    }
                }
            }
            _ => self.pressed = Some(control),
        }
        let _ = request;
    }

    fn row_at(&self, layout: &Layout, y: i32) -> usize {
        let Some(top) = layout.pl_y else {
            return usize::MAX;
        };
        let offset = y - top - skin::PL_LIST.1;
        if offset < 0 {
            return usize::MAX;
        }
        self.scroll + (offset / PL_ROW_H) as usize
    }

    /// A slider follows the pointer.
    fn slide(&mut self, control: Control, local: Point) {
        let layout = self.layout();
        match control {
            Control::Volume => {
                let (x, _, w, _) = skin::VOLUME;
                self.set_volume(skin::slider_fraction(x, w, skin::SLIDER_THUMB, local.x));
            }
            Control::Balance => {
                let (x, _, w, _) = skin::BALANCE;
                self.set_balance(skin::slider_fraction(x, w, skin::SLIDER_THUMB, local.x) * 2.0 - 1.0);
            }
            Control::Seek => {
                let duration = self.duration_ms();
                if self.status != Status::Stopped && duration > 0 {
                    let (x, _, w, _) = skin::SEEK;
                    let fraction = skin::slider_fraction(x, w, skin::SEEK_THUMB, local.x);
                    self.seek_preview = Some(fraction);
                    let mut text = Text::<64>::new();
                    let _ = write!(text, "SEEK TO: ");
                    skin::write_time(&mut text, (duration as f32 * fraction) as u64);
                    let _ = write!(text, "/");
                    skin::write_time(&mut text, duration);
                    let _ = write!(text, " ({}%)", (fraction * 100.0) as u32);
                    self.say(text.as_str());
                }
            }
            Control::Preamp | Control::Band(_) => {
                let Some(top) = layout.eq_y else {
                    return;
                };
                let fraction = skin::slider_fraction(top + skin::EQ_SLIDER_TOP, skin::EQ_SLIDER_H, skin::EQ_THUMB, local.y);
                let db = RANGE_DB - fraction * 2.0 * RANGE_DB;
                let band = match control {
                    Control::Band(band) => Some(band as usize),
                    _ => None,
                };
                self.set_band(band, db);
            }
            Control::PlScroll => {
                let Some(top) = layout.pl_y else {
                    return;
                };
                let (_, ly, _, lh) = skin::PL_LIST;
                let fraction = skin::slider_fraction(top + ly, lh, 18, local.y);
                let most = self.tracks.len().saturating_sub(PL_ROWS);
                self.scroll = (most as f32 * fraction + 0.5) as usize;
            }
            _ => {}
        }
    }

    /// The left button came up.
    fn release(&mut self, local: Point, request: &mut WindowRequest) -> bool {
        let drag = self.drag.take();
        let pressed = self.pressed.take();
        if let Some(Drag::Slider(Control::Seek)) = drag {
            if let Some(fraction) = self.seek_preview.take() {
                let target = (self.duration_ms() as f32 * fraction) as u64;
                self.seek_to(target);
                self.message_until = 0;
            }
            return false;
        }
        if drag.is_some() {
            return false;
        }
        let Some(pressed) = pressed else {
            return false;
        };
        if skin::hit(&self.layout(), local.x, local.y) != Some(pressed) {
            return false;
        }
        self.activate(pressed, request)
    }

    /// Does what a button does. Returns `true` to close FeAmp.
    fn activate(&mut self, control: Control, request: &mut WindowRequest) -> bool {
        match control {
            Control::Close => return true,
            Control::Minimize => *request = WindowRequest::Minimize,
            Control::Shade => self.shaded = !self.shaded,
            Control::Menu | Control::Logo | Control::Clutter(0) => {
                self.say("FEAMP 1.0 - IT REALLY WHIPS THE CRAB'S SHELL!")
            }
            Control::Clutter(1) => {
                self.scroll_title = !self.scroll_title;
                self.marquee_start = self.now;
                self.say(if self.scroll_title { "TITLE SCROLLING: ON" } else { "TITLE SCROLLING: OFF" });
            }
            Control::Clutter(2) => self.file_info(),
            Control::Clutter(3) => self.double = !self.double,
            Control::Clutter(_) => self.open_vis(),
            Control::Time => self.remaining = !self.remaining,
            Control::MiniVis => {
                self.vis_mode = match self.vis_mode {
                    MiniVis::Spectrum => MiniVis::Scope,
                    MiniVis::Scope => MiniVis::Off,
                    MiniVis::Off => MiniVis::Spectrum,
                }
            }
            Control::EqToggle => self.show_eq = !self.show_eq,
            Control::PlToggle => self.show_pl = !self.show_pl,
            Control::Prev => {
                self.step(-1, true);
            }
            Control::Play => self.play(),
            Control::Pause => self.pause(),
            Control::Stop => self.stop(),
            Control::Next => {
                self.step(1, true);
            }
            Control::Eject | Control::PlAdd => self.add_files(),
            Control::Shuffle => {
                self.shuffle = !self.shuffle;
                self.say(if self.shuffle { "SHUFFLE: ON" } else { "SHUFFLE: OFF" });
            }
            Control::Repeat => {
                self.repeat = !self.repeat;
                self.say(if self.repeat { "REPEAT: ON" } else { "REPEAT: OFF" });
            }
            Control::EqClose => self.show_eq = false,
            Control::PlClose => self.show_pl = false,
            Control::EqOn => {
                self.eq.enabled = !self.eq.enabled;
                FEAMP_PIPE.set_eq(self.eq);
                self.say(if self.eq.enabled { "EQUALIZER: ON" } else { "EQUALIZER: OFF" });
            }
            Control::EqAuto => {
                self.eq_auto = !self.eq_auto;
                self.say(if self.eq_auto { "EQ AUTO-LOAD: ON" } else { "EQ AUTO-LOAD: OFF" });
            }
            Control::EqPresets => {
                self.eq_preset = (self.eq_preset + 1) % PRESETS.len();
                let (name, bands) = PRESETS[self.eq_preset];
                self.eq.bands_db = bands;
                self.eq.enabled = true;
                FEAMP_PIPE.set_eq(self.eq);
                let mut text = Text::<64>::new();
                let _ = write!(text, "EQ PRESET: {}", name);
                self.say(text.as_str());
            }
            Control::PlRem => self.remove(self.cursor),
            Control::PlSel => {
                if let Some(current) = self.current {
                    self.cursor = current;
                    self.keep_cursor_visible();
                }
            }
            Control::PlMisc => {
                self.sort();
                self.say("PLAYLIST SORTED BY TITLE");
            }
            Control::PlList => {
                self.stop();
                self.tracks.clear();
                self.current = None;
                self.cursor = 0;
                self.scroll = 0;
                self.say("PLAYLIST CLEARED");
            }
            _ => {}
        }
        false
    }

    fn add_files(&mut self) {
        let added = self.scan();
        let mut text = Text::<64>::new();
        let _ = write!(text, "ADDED {added} FILE{}", if added == 1 { "" } else { "S" });
        self.say(text.as_str());
    }

    fn file_info(&mut self) {
        let mut text = Text::<64>::new();
        match (self.info, self.current) {
            (Some(info), Some(_)) => {
                let _ = write!(
                    text,
                    "{} {}HZ {} CH {}KBPS",
                    info.codec,
                    info.sample_rate,
                    info.channels,
                    self.bitrate_kbps()
                );
            }
            _ => {
                text.set("NOTHING IS PLAYING");
            }
        }
        self.say(text.as_str());
    }

    fn open_vis(&mut self) {
        if shell::request_open("milkdrop", "") {
            self.say("STARTING MILKDROP...");
        }
    }

    fn wheel(&mut self, local: Point, delta: i32) {
        let layout = self.layout();
        if matches!(skin::hit(&layout, local.x, local.y), Some(Control::PlRows | Control::PlScroll)) {
            let most = self.tracks.len().saturating_sub(PL_ROWS);
            self.scroll = if delta > 0 {
                self.scroll.saturating_sub(3)
            } else {
                (self.scroll + 3).min(most)
            };
        } else {
            self.set_volume(self.volume_fraction() + delta as f32 * 0.05);
        }
    }

    /// A key went down. Returns `true` to close.
    fn key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        let ctrl = modifiers.contains(Modifiers::CTRL);
        let alt = modifiers.contains(Modifiers::ALT);
        match key {
            Key::Character(ch) => match (ch.to_ascii_lowercase(), ctrl, alt) {
                ('z', false, false) => {
                    self.step(-1, true);
                }
                ('x', false, false) => self.play(),
                ('c', false, false) => self.pause(),
                ('v', false, false) => self.stop(),
                ('b', false, false) => {
                    self.step(1, true);
                }
                ('s', false, false) => {
                    let _ = self.activate(Control::Shuffle, &mut WindowRequest::None);
                }
                ('r', false, false) => {
                    let _ = self.activate(Control::Repeat, &mut WindowRequest::None);
                }
                ('l', false, false) => self.add_files(),
                ('d', true, _) => self.double = !self.double,
                ('w', true, _) => self.shaded = !self.shaded,
                ('k', true, _) => self.open_vis(),
                ('g', _, true) => self.show_eq = !self.show_eq,
                ('e', _, true) => self.show_pl = !self.show_pl,
                _ => {}
            },
            Key::Left => self.seek_to(self.position_ms.saturating_sub(5000)),
            Key::Right => self.seek_to(self.position_ms + 5000),
            Key::Up if ctrl => {
                self.cursor = self.cursor.saturating_sub(1);
                self.keep_cursor_visible();
            }
            Key::Down if ctrl => {
                self.cursor = (self.cursor + 1).min(self.tracks.len().saturating_sub(1));
                self.keep_cursor_visible();
            }
            Key::Up => self.set_volume(self.volume_fraction() + 0.05),
            Key::Down => self.set_volume(self.volume_fraction() - 0.05),
            Key::PageUp => {
                self.cursor = self.cursor.saturating_sub(PL_ROWS);
                self.keep_cursor_visible();
            }
            Key::PageDown => {
                self.cursor = (self.cursor + PL_ROWS).min(self.tracks.len().saturating_sub(1));
                self.keep_cursor_visible();
            }
            Key::Home => {
                self.cursor = 0;
                self.keep_cursor_visible();
            }
            Key::End => {
                self.cursor = self.tracks.len().saturating_sub(1);
                self.keep_cursor_visible();
            }
            Key::Enter => self.play_index(self.cursor),
            Key::Delete => self.remove(self.cursor),
            Key::Space => self.pause(),
            Key::F4 if alt => return true,
            _ => {}
        }
        false
    }

    /// One frame of the window. Returns whether to close, and what to ask the
    /// desktop for.
    fn frame(&mut self, context: &mut WindowContext<'_, '_>, pixels: &mut Vec<u32>) -> bool {
        let now = context.now_ms;
        self.tick(now);
        self.focused = context.keyboard_active;
        self.vis_open = vis::is_open();
        let origin = context.origin;
        let scale = self.scale();
        let local = |pos: Point| Point::new((pos.x - origin.x) / scale, (pos.y - origin.y) / scale);
        let mut request = WindowRequest::None;
        let mut close = false;
        let mut moved = (0, 0);
        if context.pointer_active {
            for event in context.input.events() {
                match event {
                    Event::PointerDown { pos, button: MouseButton::Left } => {
                        self.press(local(pos), pos, &mut request);
                    }
                    Event::PointerMoved(pos) => match self.drag {
                        Some(Drag::Move(last)) => {
                            moved.0 += pos.x - last.x;
                            moved.1 += pos.y - last.y;
                            self.drag = Some(Drag::Move(pos));
                        }
                        Some(Drag::Slider(control)) => self.slide(control, local(pos)),
                        None => {}
                    },
                    Event::PointerUp { pos, button: MouseButton::Left } => {
                        close |= self.release(local(pos), &mut request);
                    }
                    Event::PointerWheel(delta) => {
                        let pointer = context.input.pointer();
                        self.wheel(local(pointer), delta);
                    }
                    _ => {}
                }
            }
        } else if self.drag.is_some() && !context.input.pointer_down() {
            self.drag = None;
            self.pressed = None;
        }
        context.dispatch_keys(|event| {
            if let Event::KeyDown(key, modifiers) = event {
                close |= self.key(key, modifiers);
            }
        });
        if moved != (0, 0) {
            request = WindowRequest::Move { dx: moved.0, dy: moved.1 };
        }
        self.listen();

        // Draw at 1x, then show it at the window's scale.
        let layout = self.layout();
        let (width, height) = (WIDTH as usize, layout.height as usize);
        if pixels.len() != width * height {
            pixels.clear();
            if pixels.try_reserve_exact(width * height).is_err() {
                return close;
            }
            pixels.resize(width * height, 0);
        }
        if let Some(mut canvas) = Canvas::new(pixels, width, height, width) {
            skin::draw(self, &mut canvas, now);
        }
        present(context.canvas, pixels, width, height, self.scale() as usize);

        let (want_width, want_height) = self.window_size();
        if (context.canvas.width() as u32, context.canvas.height() as u32) != (want_width, want_height)
            && request == WindowRequest::None
        {
            request = WindowRequest::Resize {
                width: want_width,
                height: want_height,
            };
        }
        context.request = request;
        close
    }
}

/// Most nodes a walk visits, so a huge disk can't stall a window.
const WALK_LIMIT: usize = 100_000;

/// Top-level directories a walk of the whole system leaves out: `/rom` is the
/// built-in image again when the root is a disk seeded from it, so its files
/// would all be listed twice, and `/dev` holds devices.
const WALK_SKIP: [&str; 2] = ["rom", "dev"];

/// Calls `visit` with every file under `start`, and its name. It walks the
/// tree, rather than counting node IDs, as a mounted volume's IDs needn't be
/// dense. From the root, it leaves out [`WALK_SKIP`].
pub(crate) fn walk_files(fs: &dyn FileSystem, start: NodeId, mut visit: impl FnMut(NodeId, &str)) {
    let mut stack = Vec::new();
    if stack.try_reserve(64).is_err() {
        return;
    }
    let root = fs.root();
    stack.push(start);
    let mut seen = 0;
    while let Some(dir) = stack.pop() {
        for index in 0..fs.child_count(dir) {
            seen += 1;
            if seen > WALK_LIMIT {
                return;
            }
            let Some(node) = fs.child_at(dir, index) else {
                continue;
            };
            if fs.is_file(node) {
                if let Some(name) = fs.name(node) {
                    visit(node, name);
                }
            } else if fs.is_dir(node) && stack.try_reserve(1).is_ok() {
                if dir == root && fs.name(node).is_some_and(|name| WALK_SKIP.contains(&name)) {
                    continue;
                }
                stack.push(node);
            }
        }
    }
}

/// Copies the 1x image to the window, `scale` times the size.
fn present(canvas: &mut Canvas<'_>, pixels: &[u32], width: usize, height: usize, scale: usize) {
    let (target_width, target_height, stride) = (canvas.width(), canvas.height(), canvas.stride());
    let out = canvas.pixels_mut();
    for y in 0..target_height {
        let row = &mut out[y * stride..y * stride + target_width];
        let source_y = y / scale;
        if source_y >= height {
            row.fill(0);
            continue;
        }
        let source = &pixels[source_y * width..(source_y + 1) * width];
        for (x, pixel) in row.iter_mut().enumerate() {
            let source_x = x / scale;
            *pixel = if source_x < width { source[source_x] } else { 0 };
        }
    }
}

struct Window {
    player: Box<Player>,
    pixels: Vec<u32>,
}

static WINDOW: SpinLock<Option<Window>> = SpinLock::new(None);
/// FeAmp is open: the visualiser's window uses this to say where to start.
static OPEN: AtomicBool = AtomicBool::new(false);

/// The FeAmp application.
pub struct FeAmp;

impl App for FeAmp {
    fn name(&self) -> &'static str {
        "feamp"
    }

    fn usage(&self) -> &'static str {
        "feamp [file]"
    }

    fn description(&self) -> &'static str {
        "a classic skinned music player, with MilkDrop visualisation"
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
        50
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        let Some(fs) = context.platform.filesystem else {
            return false;
        };
        let mut player = Box::new(Player::new(fs, context.platform.processes));
        player.now = context.now_ms;
        player.say("WELCOME TO FEAMP!");
        let path = context.args.trim();
        if !path.is_empty() {
            match fs.resolve(path).filter(|&node| fs.is_file(node)) {
                Some(node) => {
                    if let Some(index) = player.add(node) {
                        player.play_index(index);
                    }
                }
                None => player.say("THE FILE WAS NOT FOUND"),
            }
        }
        *WINDOW.lock() = Some(Window {
            player,
            pixels: Vec::new(),
        });
        OPEN.store(true, Ordering::Relaxed);
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        // Out of the lock for the frame: a failure with a lock held stops the
        // machine, and this way it's only this window's.
        let Some(mut window) = WINDOW.lock().take() else {
            return WindowFrame::Close;
        };
        let close = window.player.frame(context, &mut window.pixels);
        *WINDOW.lock() = Some(window);
        if close {
            WindowFrame::Close
        } else {
            WindowFrame::Continue
        }
    }

    fn close_window(&self) {
        FEAMP_PIPE.shut_down();
        *WINDOW.lock() = None;
        OPEN.store(false, Ordering::Relaxed);
    }

    fn background_tick(&self, _platform: &Platform, now_ms: u64) {
        if let Some(window) = WINDOW.try_lock().as_mut().and_then(|guard| guard.as_mut()) {
            window.player.tick(now_ms);
        }
    }

    fn accept_drop(&self, payload: &str) -> bool {
        let Some(mut guard) = WINDOW.try_lock() else {
            return false;
        };
        let Some(window) = guard.as_mut() else {
            return false;
        };
        let player = &mut window.player;
        match player.fs.resolve(payload).filter(|&node| player.fs.is_file(node)) {
            Some(node) => match player.add(node) {
                Some(index) => {
                    player.play_index(index);
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    fn run(&self, _context: &mut Context<'_>) -> i32 {
        eprintln!("feamp: this is a desktop window; open it from the Start menu, or `play <file>` here");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_doubles_each_pixel() {
        let source = [1u32, 2, 3, 4];
        let mut out = [0u32; 16];
        let mut canvas = Canvas::new(&mut out, 4, 4, 4).unwrap();
        present(&mut canvas, &source, 2, 2, 2);
        assert_eq!(out, [1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
    }

    struct NoProcesses;
    impl Processes for NoProcesses {
        fn spawn(&self, _app: &'static dyn App, _args: &str) -> Result<u32, feos_app::SpawnError> {
            Ok(7)
        }
        fn kill(&self, _pid: u32) -> bool {
            false
        }
        fn current(&self) -> u32 {
            1
        }
        fn snapshot(&self, _out: &mut [feos_app::ProcessInfo]) -> usize {
            0
        }
    }
    static PROCESSES: NoProcesses = NoProcesses;

    /// A filesystem with the demo tracks in it, as the image has.
    fn music_fs() -> &'static dyn FileSystem {
        use feos_fs::WritableFileSystem;
        extern crate std;
        let mut fs = feos_fs::vfs::RamFs::new(16 << 20);
        let root = fs.root();
        let media = fs.create_dir(root, "media").unwrap();
        let music = fs.create_dir(media, "music").unwrap();
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/music");
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let bytes = std::fs::read(entry.path()).unwrap();
            let node = fs.create_file(music, &entry.file_name().to_string_lossy()).unwrap();
            fs.write(node, 0, &bytes).unwrap();
        }
        std::boxed::Box::leak(std::boxed::Box::new(fs))
    }

    #[test]
    fn a_walk_skips_the_rom_copy_and_devices() {
        use feos_fs::WritableFileSystem;
        extern crate std;
        let mut fs = feos_fs::vfs::RamFs::new(1 << 20);
        let root = fs.root();
        let mut make = |path: &[&str]| {
            let mut dir = root;
            for name in &path[..path.len() - 1] {
                dir = fs.lookup(dir, name).unwrap_or_else(|| fs.create_dir(dir, name).unwrap());
            }
            fs.create_file(dir, path[path.len() - 1]).unwrap()
        };
        let song = make(&["media", "music", "song.mp3"]);
        make(&["rom", "media", "music", "song.mp3"]);
        make(&["dev", "audio.mp3"]);
        let deeper = make(&["home", "rom", "kept.mp3"]);
        let mut seen = std::vec::Vec::new();
        walk_files(&fs, fs.root(), |node, _| seen.push(node));
        seen.sort();
        let mut want = std::vec![song, deeper];
        want.sort();
        assert_eq!(seen, want);
        // Asked to start inside, it goes there.
        let rom = fs.resolve("/rom").unwrap();
        let mut count = 0;
        walk_files(&fs, rom, |_, _| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn reads_the_demo_tracks_and_draws_a_playing_player() {
        let mut player = Player::new(music_fs(), &PROCESSES);
        assert_eq!(player.tracks.len(), 2);
        assert_eq!(player.tracks[0].name.as_str(), "The FeOS Band - Ferrous Dreams");
        assert!((60_000..66_000).contains(&player.tracks[1].duration_ms), "{}", player.tracks[1].duration_ms);
        // As if a song were a while in.
        player.current = Some(1);
        player.status = Status::Playing;
        player.position_ms = 83_000;
        player.now = 1_000;
        player.info = Some(TrackInfo { codec: "MP3", sample_rate: 44_100, channels: 2, duration_ms: 62_000 });
        player.eq.enabled = true;
        player.eq.bands_db = PRESETS[1].1;
        for (bar, height) in player.bars.iter_mut().enumerate() {
            *height = 15.0 - (bar as f32 * 0.7) - (bar % 3) as f32 * 2.0;
            player.peaks[bar] = *height + 2.0;
        }
        let layout = player.layout();
        let mut pixels = std::vec![0u32; WIDTH as usize * layout.height as usize];
        let mut canvas = Canvas::new(&mut pixels, WIDTH as usize, layout.height as usize, WIDTH as usize).unwrap();
        skin::draw(&player, &mut canvas, 1_000);
        assert!(player.bitrate_kbps() > 100 && player.bitrate_kbps() < 160, "{}", player.bitrate_kbps());
        // `FEAMP_PREVIEW=/tmp/feamp.ppm cargo test ...` saves it, to look at.
        extern crate std;
        if let Ok(path) = std::env::var("FEAMP_PREVIEW") {
            let mut ppm = std::format!("P6\n{} {}\n255\n", WIDTH, layout.height).into_bytes();
            for pixel in &pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
    }

    #[test]
    fn the_default_window_fits_a_small_screen() {
        let layout = Layout::new(false, true, true);
        assert!(layout.height <= 768 - 40);
        assert_eq!(layout.height, skin::MAIN_H + skin::EQ_H + skin::PL_H);
    }
}
