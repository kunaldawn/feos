//! VLC-style video player for feos.
//!
//! The player scans the read-only filesystem for supported media, decodes
//! frames with `feos-video`, and provides a VLC-inspired interface: menus,
//! transport controls, a seek bar, playlist, fullscreen mode, aspect modes,
//! media information and keyboard/mouse control.
//!
//! H.264 containers use `feos-h264`, which needs `alloc`; the kernel's static
//! first-fit heap makes that possible. GIF, MJPEG, uncompressed and RLE video
//! decode without additional allocations.

use core::fmt::Write;

use alloc::vec;
use alloc::vec::Vec;

use feos_app::{App, Context, WindowContext, WindowFrame};
use feos_console::println;
use feos_fs::{FileSystem, NodeId};
use feos_gfx::{Canvas, Color, Point, Rect};
use feos_gui::{
    paint_icon, Align, Event, Icon, Input, Key, Modifiers, Painter, RectExt, VerticalAlign,
};
use feos_hal::PointerEvent;
use feos_keys::SerialKeys;
use feos_sync::SpinLock;
use feos_text::Text;
use feos_video::{
    decode_frame as decode_video_frame, info as video_info, scratch_len, ContainerFormat,
    VideoError, VideoInfo,
};

use crate::files::format_size;

const MAX_PLAYLIST: usize = 64;
const MAX_OPEN: usize = 128;
const MAX_PATH: usize = 256;
const MAX_MEDIA_BYTES: usize = 4 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const PLAYLIST_WIDTH: i32 = 300;
const TITLE_HEIGHT: i32 = 30;
const MENU_HEIGHT: i32 = 22;
const TOOLBAR_HEIGHT: i32 = 46;
const CONTROLS_HEIGHT: i32 = 62;
const STATUS_HEIGHT: i32 = 20;

const BG: Color = Color::rgb(20, 20, 20);
const PANEL: Color = Color::rgb(32, 32, 32);
const PANEL_LIGHT: Color = Color::rgb(44, 44, 44);
const BAR_TOP: Color = Color::rgb(48, 48, 48);
const BAR_BOTTOM: Color = Color::rgb(26, 26, 26);
const BORDER: Color = Color::rgb(70, 70, 70);
const TEXT: Color = Color::rgb(238, 238, 238);
const MUTED: Color = Color::rgb(164, 164, 164);
const DISABLED: Color = Color::rgb(106, 106, 106);
const ORANGE: Color = Color::rgb(255, 136, 0);
const ORANGE_LIGHT: Color = Color::rgb(255, 168, 48);
const ORANGE_DARK: Color = Color::rgb(184, 88, 0);
const VIDEO_BG: Color = Color::BLACK;
const HOVER: Color = Color::rgb(60, 60, 60);
const ERROR: Color = Color::rgb(255, 112, 112);

/// The video player application.
pub struct Player;

impl App for Player {
    fn name(&self) -> &'static str {
        "player"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["vlc", "video", "media", "media-player", "movies"]
    }

    fn usage(&self) -> &'static str {
        "player [path]"
    }

    fn description(&self) -> &'static str {
        "play video files with a VLC-style media player"
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

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        let Some(fs) = context.platform.filesystem else {
            return false;
        };
        open_player(fs, context)
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        let Some(fs) = context.platform.filesystem else {
            return WindowFrame::Close;
        };
        frame_player(fs, context)
    }

    fn close_window(&self) {
        close_player();
    }

    fn run(&self, context: &mut Context<'_>) {
        let Some(fs) = context.platform.filesystem else {
            println!("player: no filesystem is available on this board");
            return;
        };
        run(fs, context);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopMode {
    Off,
    One,
    All,
}

impl LoopMode {
    fn label(self) -> &'static str {
        match self {
            Self::Off => "Loop off",
            Self::One => "Repeat one",
            Self::All => "Repeat all",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Aspect {
    Fit,
    Fill,
    Stretch,
    Original,
}

impl Aspect {
    fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit to window",
            Self::Fill => "Fill and crop",
            Self::Stretch => "Stretch",
            Self::Original => "Original size",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Video,
    Playlist,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialog {
    None,
    Open,
    Info,
    About,
    Help,
    Notice,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuKind {
    Media,
    Playback,
    Audio,
    Video,
    Subtitle,
    Tools,
    View,
    Help,
}

impl MenuKind {
    const ALL: [Self; 8] = [
        Self::Media,
        Self::Playback,
        Self::Audio,
        Self::Video,
        Self::Subtitle,
        Self::Tools,
        Self::View,
        Self::Help,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Media => 0,
            Self::Playback => 1,
            Self::Audio => 2,
            Self::Video => 3,
            Self::Subtitle => 4,
            Self::Tools => 5,
            Self::View => 6,
            Self::Help => 7,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Open,
    Quit,
    PlayPause,
    PlayAll,
    Stop,
    Previous,
    Next,
    SeekForward,
    SeekBack,
    JumpStart,
    JumpEnd,
    FrameNext,
    FramePrev,
    Faster,
    Slower,
    NormalSpeed,
    Fullscreen,
    TogglePlaylist,
    AspectFit,
    AspectFill,
    AspectStretch,
    AspectOriginal,
    CycleLoop,
    ToggleShuffle,
    MediaInfo,
    Remove,
    ClearPlaylist,
    RefreshPlaylist,
    About,
    Help,
    Preferences,
}

#[derive(Clone, Copy)]
struct MenuItem {
    label: &'static str,
    shortcut: &'static str,
    action: Action,
    separator: bool,
    enabled: bool,
}

const fn item(label: &'static str, shortcut: &'static str, action: Action) -> MenuItem {
    MenuItem {
        label,
        shortcut,
        action,
        separator: false,
        enabled: true,
    }
}

const fn disabled(label: &'static str, shortcut: &'static str) -> MenuItem {
    MenuItem {
        label,
        shortcut,
        action: Action::None,
        separator: false,
        enabled: false,
    }
}

const fn sep() -> MenuItem {
    MenuItem {
        label: "",
        shortcut: "",
        action: Action::None,
        separator: true,
        enabled: false,
    }
}

const MEDIA_MENU: &[MenuItem] = &[
    item("Open File...", "O", Action::Open),
    disabled("Open Folder...", ""),
    disabled("Open Disc...", ""),
    disabled("Open Network...", ""),
    sep(),
    disabled("Save Playlist...", ""),
    sep(),
    item("Quit", "Esc", Action::Quit),
];

const PLAYBACK_MENU: &[MenuItem] = &[
    item("Play / Pause", "Space", Action::PlayPause),
    item("Stop", "S", Action::Stop),
    item("Previous", "B", Action::Previous),
    item("Next", "N", Action::Next),
    sep(),
    item("Jump Forward 5s", "Right", Action::SeekForward),
    item("Jump Back 5s", "Left", Action::SeekBack),
    sep(),
    item("Next Frame", ".", Action::FrameNext),
    item("Previous Frame", ",", Action::FramePrev),
    sep(),
    item("Faster", "+", Action::Faster),
    item("Slower", "-", Action::Slower),
    item("Normal Speed", "0", Action::NormalSpeed),
    sep(),
    item("Loop", "L", Action::CycleLoop),
    item("Shuffle", "R", Action::ToggleShuffle),
];

const AUDIO_MENU: &[MenuItem] = &[
    disabled("Increase Volume", "Up"),
    disabled("Decrease Volume", "Down"),
    disabled("Mute", "M"),
    sep(),
    disabled("Audio Track", ""),
    disabled("Audio Device", ""),
];

const VIDEO_MENU: &[MenuItem] = &[
    item("Fullscreen", "F", Action::Fullscreen),
    item("Playlist", "P", Action::TogglePlaylist),
    sep(),
    item("Aspect: Fit", "A", Action::AspectFit),
    item("Aspect: Fill", "", Action::AspectFill),
    item("Aspect: Stretch", "", Action::AspectStretch),
    item("Aspect: Original", "", Action::AspectOriginal),
    sep(),
    item("Media Information", "I", Action::MediaInfo),
];

const SUBTITLE_MENU: &[MenuItem] = &[
    disabled("Add Subtitle File...", ""),
    disabled("Subtitle Track", ""),
    disabled("Subtitle Synchronization", ""),
];

const TOOLS_MENU: &[MenuItem] = &[
    item("Media Information", "I", Action::MediaInfo),
    item("Refresh Playlist", "F5", Action::RefreshPlaylist),
    item("Clear Playlist", "", Action::ClearPlaylist),
    sep(),
    item("Preferences", "", Action::Preferences),
];

const VIEW_MENU: &[MenuItem] = &[
    item("Playlist", "P", Action::TogglePlaylist),
    item("Fullscreen", "F", Action::Fullscreen),
    sep(),
    disabled("Status Bar", ""),
];

const HELP_MENU: &[MenuItem] = &[
    item("Help", "F1", Action::Help),
    item("Keyboard Shortcuts", "H", Action::Help),
    sep(),
    item("About Media Player", "", Action::About),
];

const CONTEXT_VIDEO: &[MenuItem] = &[
    item("Play / Pause", "Space", Action::PlayPause),
    item("Stop", "S", Action::Stop),
    item("Previous", "B", Action::Previous),
    item("Next", "N", Action::Next),
    item("Jump to Start", "Home", Action::JumpStart),
    item("Jump to End", "End", Action::JumpEnd),
    sep(),
    item("Fullscreen", "F", Action::Fullscreen),
    item("Playlist", "P", Action::TogglePlaylist),
    item("Media Information", "I", Action::MediaInfo),
    sep(),
    item("Loop", "L", Action::CycleLoop),
    item("Shuffle", "R", Action::ToggleShuffle),
];

const CONTEXT_PLAYLIST: &[MenuItem] = &[
    item("Play", "Enter", Action::PlayPause),
    item("Media Information", "I", Action::MediaInfo),
    item("Remove from Playlist", "Del", Action::Remove),
];

const CONTEXT_BACKGROUND: &[MenuItem] = &[
    item("Open File...", "O", Action::Open),
    item("Play All", "", Action::PlayAll),
    item("Shuffle", "R", Action::ToggleShuffle),
    sep(),
    item("Media Information", "I", Action::MediaInfo),
    item("Refresh Playlist", "F5", Action::RefreshPlaylist),
];

const MENU_LABELS: [&str; 8] = [
    "Media", "Playback", "Audio", "Video", "Subtitle", "Tools", "View", "Help",
];
const MENU_WIDTHS: [i32; 8] = [58, 76, 56, 56, 72, 56, 56, 48];
const TOOLBAR_WIDTHS: [i32; 10] = [48, 46, 54, 46, 46, 48, 50, 46, 48, 46];

fn menu_items(kind: MenuKind) -> &'static [MenuItem] {
    match kind {
        MenuKind::Media => MEDIA_MENU,
        MenuKind::Playback => PLAYBACK_MENU,
        MenuKind::Audio => AUDIO_MENU,
        MenuKind::Video => VIDEO_MENU,
        MenuKind::Subtitle => SUBTITLE_MENU,
        MenuKind::Tools => TOOLS_MENU,
        MenuKind::View => VIEW_MENU,
        MenuKind::Help => HELP_MENU,
    }
}

#[derive(Clone, Copy)]
enum DragMode {
    None,
    Seek,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContextKind {
    Video,
    Playlist(usize),
    Background,
}

#[derive(Clone, Copy)]
struct ContextMenu {
    kind: ContextKind,
    x: i32,
    y: i32,
    hover: usize,
}

struct PlayerState {
    screen: Rect,
    playlist: [NodeId; MAX_PLAYLIST],
    playlist_len: usize,
    playlist_cursor: usize,
    playlist_scroll: usize,
    current: Option<NodeId>,
    data: Vec<u8>,
    rgba: Vec<u8>,
    scratch: Vec<u8>,
    info: Option<VideoInfo>,
    error: Text<160>,
    media_name: Text<96>,
    media_path: Text<MAX_PATH>,
    media_size: usize,
    frame_width: u32,
    frame_height: u32,
    frame_index: u32,
    frame_count: u32,
    frame_delay: u32,
    fps_num: u32,
    fps_den: u32,
    duration_ms: u64,
    position_ms: u64,
    playing: bool,
    ended: bool,
    speed: f32,
    loop_mode: LoopMode,
    shuffle: bool,
    rng: u32,
    last_tick: u64,
    playlist_open: bool,
    fullscreen: bool,
    aspect: Aspect,
    focus: Focus,
    dialog: Dialog,
    open_results: [NodeId; MAX_OPEN],
    open_len: usize,
    open_cursor: usize,
    open_scroll: usize,
    notice_title: Text<48>,
    notice_body: Text<192>,
    menu_open: Option<MenuKind>,
    menu_hover: usize,
    context: Option<ContextMenu>,
    /// True when the player is hosted by the desktop window manager.
    embedded: bool,
    pointer: Point,
    has_pointer: bool,
    show_cursor: bool,
    mouse_down: bool,
    drag: DragMode,
    resume_after_seek: bool,
    last_click_ms: u64,
    last_click_index: Option<usize>,
    last_video_click_ms: u64,
    last_open_click_ms: u64,
    last_open_index: Option<usize>,
    osd: Text<96>,
    osd_until: u64,
    last_activity: u64,
    exit: bool,
}

impl PlayerState {
    fn new(fs: &dyn FileSystem, screen: Rect, args: &str, now: u64) -> Self {
        let mut state = Self {
            screen,
            playlist: [NodeId::ROOT; MAX_PLAYLIST],
            playlist_len: 0,
            playlist_cursor: 0,
            playlist_scroll: 0,
            current: None,
            data: Vec::new(),
            rgba: Vec::new(),
            scratch: Vec::new(),
            info: None,
            error: Text::new(),
            media_name: Text::new(),
            media_path: Text::new(),
            media_size: 0,
            frame_width: 0,
            frame_height: 0,
            frame_index: 0,
            frame_count: 0,
            frame_delay: 0,
            fps_num: 10,
            fps_den: 1,
            duration_ms: 0,
            position_ms: 0,
            playing: false,
            ended: false,
            speed: 1.0,
            loop_mode: LoopMode::All,
            shuffle: false,
            rng: 0x1234_5678,
            last_tick: now,
            playlist_open: true,
            fullscreen: false,
            aspect: Aspect::Fit,
            focus: Focus::Video,
            dialog: Dialog::None,
            open_results: [NodeId::ROOT; MAX_OPEN],
            open_len: 0,
            open_cursor: 0,
            open_scroll: 0,
            notice_title: Text::new(),
            notice_body: Text::new(),
            menu_open: None,
            menu_hover: 0,
            context: None,
            embedded: false,
            pointer: Point::new(-100, -100),
            has_pointer: false,
            show_cursor: true,
            mouse_down: false,
            drag: DragMode::None,
            resume_after_seek: false,
            last_click_ms: 0,
            last_click_index: None,
            last_video_click_ms: 0,
            last_open_click_ms: 0,
            last_open_index: None,
            osd: Text::new(),
            osd_until: 0,
            last_activity: now,
            exit: false,
        };
        state.scan_playlist(fs);
        if args.is_empty() {
            if state.playlist_len > 0 {
                let node = state.playlist[0];
                state.playlist_cursor = 0;
                state.open_node(fs, node, now, true);
            }
        } else if let Some(node) = fs.resolve(args.trim()) {
            if fs.is_file(node) {
                state.ensure_in_playlist(node);
                state.open_node(fs, node, now, true);
            } else {
                state.set_notice("Open File", "The path does not point to a file.", now);
            }
        } else {
            state.set_notice("Open File", "The requested file was not found.", now);
        }
        state
    }

    fn set_screen(&mut self, screen: Rect) {
        self.screen = screen;
    }

    fn scan_playlist(&mut self, fs: &dyn FileSystem) {
        self.playlist_len = 0;
        let root = fs.resolve("/media").unwrap_or_else(|| fs.root());
        collect_media(fs, root, 0, &mut self.playlist, &mut self.playlist_len);
        if self.playlist_len == 0 {
            collect_media(fs, fs.root(), 0, &mut self.playlist, &mut self.playlist_len);
        }
        if self.playlist_cursor >= self.playlist_len {
            self.playlist_cursor = self.playlist_len.saturating_sub(1);
        }
    }

    fn scan_all_media(&mut self, fs: &dyn FileSystem) {
        self.open_len = 0;
        collect_media(fs, fs.root(), 0, &mut self.open_results, &mut self.open_len);
        self.open_cursor = self.open_cursor.min(self.open_len.saturating_sub(1));
        self.open_scroll = 0;
    }

    fn ensure_in_playlist(&mut self, node: NodeId) {
        if (0..self.playlist_len).any(|index| self.playlist[index] == node) {
            return;
        }
        if self.playlist_len < MAX_PLAYLIST {
            self.playlist[self.playlist_len] = node;
            self.playlist_len += 1;
        }
    }

    fn current_index(&self) -> Option<usize> {
        let node = self.current?;
        (0..self.playlist_len).find(|index| self.playlist[*index] == node)
    }

    fn open_node(&mut self, fs: &dyn FileSystem, node: NodeId, now: u64, play: bool) {
        self.current = Some(node);
        self.ensure_in_playlist(node);
        if let Some(index) = self.current_index() {
            self.playlist_cursor = index;
        }
        self.media_name.set(fs.name(node).unwrap_or("?"));
        let mut path_buffer = [0u8; MAX_PATH];
        self.media_path
            .set(fs.path(node, &mut path_buffer).unwrap_or("/"));
        self.media_size = fs.size(node);
        self.error.clear();
        self.info = None;
        self.data.clear();
        self.rgba.clear();
        self.scratch.clear();
        self.frame_width = 0;
        self.frame_height = 0;
        self.frame_index = 0;
        self.frame_count = 0;
        self.frame_delay = 0;
        self.duration_ms = 0;
        self.position_ms = 0;
        self.ended = false;
        self.playing = false;
        self.last_tick = now;

        if self.media_size > MAX_MEDIA_BYTES {
            self.fail_open("File is too large for the player buffer.", now);
            return;
        }
        if self.media_size == 0 {
            self.fail_open("The file is empty.", now);
            return;
        }
        let mut data = vec![0u8; self.media_size];
        let mut offset = 0usize;
        while offset < data.len() {
            let read = fs.read(node, offset, &mut data[offset..]);
            if read == 0 {
                break;
            }
            offset += read;
        }
        data.truncate(offset);
        if data.is_empty() {
            self.fail_open("The file could not be read.", now);
            return;
        }
        self.data = data;

        match video_info(&self.data) {
            Ok(info) => {
                self.info = Some(info);
                self.frame_count = info.frame_count;
                self.frame_width = info.width;
                self.frame_height = info.height;
                if let Some((num, den)) = info.frame_rate {
                    if num > 0 && den > 0 {
                        self.fps_num = num;
                        self.fps_den = den;
                    }
                }
                self.duration_ms = info.duration_ms().unwrap_or_else(|| {
                    if info.frame_count > 0 {
                        info.frame_count as u64 * 1000 * self.fps_den as u64 / self.fps_num as u64
                    } else {
                        0
                    }
                });
                if !info.can_decode() {
                    self.fail_open("The codec is recognised but cannot be decoded here.", now);
                    return;
                }
                let Ok(rgba_len) = info.rgba_len() else {
                    self.fail_open("Invalid video dimensions.", now);
                    return;
                };
                if rgba_len > MAX_FRAME_BYTES {
                    self.fail_open("The decoded frame is too large for the video heap.", now);
                    return;
                }
                let Ok(scratch) = scratch_len(&self.data) else {
                    self.fail_open("Invalid video data.", now);
                    return;
                };
                self.rgba = vec![0u8; rgba_len];
                self.scratch = vec![0u8; scratch];
                self.playing = play;
                if !self.decode_current() {
                    self.playing = false;
                }
                if play && !self.error.is_empty() {
                    self.playing = false;
                }
            }
            Err(error) => {
                let mut text = Text::<160>::new();
                let _ = write!(text, "Unsupported media: {error}");
                self.error = text;
                self.playing = false;
            }
        }
        self.last_tick = now;
        if let Some(playlist) = Layout::compute(self, now).playlist {
            let rows = playlist_visible_rows(playlist);
            self.ensure_playlist_visible(rows);
        }
    }

    fn fail_open(&mut self, message: &str, now: u64) {
        self.error.set(message);
        self.playing = false;
        self.set_notice("Playback Error", message, now);
    }

    fn decode_current(&mut self) -> bool {
        if self.data.is_empty() || self.rgba.is_empty() {
            return false;
        }
        let result = decode_video_frame(
            &self.data,
            self.frame_index,
            &mut self.rgba,
            &mut self.scratch,
        );
        match result {
            Ok(frame) => {
                self.frame_width = frame.width;
                self.frame_height = frame.height;
                self.frame_delay = frame.delay_ms;
                self.error.clear();
                true
            }
            Err(VideoError::FrameOutOfRange { .. }) => {
                self.ended = true;
                self.playing = false;
                if self.duration_ms == 0 {
                    self.position_ms = 0;
                } else {
                    self.position_ms = self.duration_ms;
                }
                false
            }
            Err(error) => {
                let mut text = Text::<160>::new();
                let _ = write!(text, "Playback error: {error}");
                self.error = text;
                self.playing = false;
                false
            }
        }
    }

    fn frame_for_position(&self) -> u32 {
        let frames = if self.frame_count > 0 {
            self.frame_count as u64
        } else {
            u64::MAX
        };
        let value = (self.position_ms as u128)
            .saturating_mul(self.fps_num as u128)
            .saturating_div((self.fps_den as u128).saturating_mul(1000));
        let frame = value.min(u32::MAX as u128) as u32;
        if self.frame_count > 0 {
            frame.min(self.frame_count.saturating_sub(1).max(0))
        } else {
            (frame as u64).min(frames.saturating_sub(1)) as u32
        }
    }

    fn position_for_frame(&self, frame: u32) -> u64 {
        (frame as u64)
            .saturating_mul(1000)
            .saturating_mul(self.fps_den as u64)
            / self.fps_num.max(1) as u64
    }

    fn tick(&mut self, fs: &dyn FileSystem, now: u64) -> bool {
        let mut changed = false;
        if self.osd_until != 0 && now >= self.osd_until {
            self.osd.clear();
            self.osd_until = 0;
            changed = true;
        }
        if !self.playing {
            self.last_tick = now;
            return changed;
        }
        let elapsed = now.saturating_sub(self.last_tick);
        self.last_tick = now;
        if elapsed == 0 {
            return changed;
        }
        self.position_ms = self
            .position_ms
            .saturating_add((elapsed as f64 * self.speed as f64) as u64);
        let target = self.frame_for_position();
        if target != self.frame_index {
            self.frame_index = target;
            changed |= self.decode_current();
        }
        if self.duration_ms > 0 && self.position_ms >= self.duration_ms {
            changed |= self.handle_end(fs, now);
        }
        changed
    }

    fn handle_end(&mut self, fs: &dyn FileSystem, now: u64) -> bool {
        match self.loop_mode {
            LoopMode::One => {
                self.seek_to(0, now);
                self.playing = true;
                true
            }
            LoopMode::All => {
                if self.playlist_len > 0 {
                    self.next_media(fs, now, true);
                } else {
                    self.seek_to(0, now);
                }
                true
            }
            LoopMode::Off => {
                self.playing = false;
                self.ended = true;
                self.position_ms = self.duration_ms;
                self.show_osd("End", now);
                true
            }
        }
    }

    fn toggle_play(&mut self, fs: &dyn FileSystem, now: u64) -> bool {
        if self.current.is_none() {
            return self.open_selected_playlist(fs, now);
        }
        if self.playing {
            self.playing = false;
            self.show_osd("Paused", now);
        } else {
            if self.ended || (self.duration_ms > 0 && self.position_ms >= self.duration_ms) {
                self.seek_to(0, now);
            }
            self.playing = true;
            self.ended = false;
            self.last_tick = now;
            self.show_osd("Playing", now);
        }
        self.last_activity = now;
        true
    }

    fn stop(&mut self, now: u64) {
        self.playing = false;
        self.ended = false;
        self.position_ms = 0;
        self.frame_index = 0;
        if !self.rgba.is_empty() {
            self.decode_current();
        }
        self.show_osd("Stopped", now);
        self.last_tick = now;
    }

    fn seek_to(&mut self, position: u64, now: u64) {
        if self.duration_ms == 0 || self.data.is_empty() {
            return;
        }
        self.position_ms = position.min(self.duration_ms);
        self.frame_index = self.frame_for_position();
        self.ended = false;
        self.decode_current();
        self.last_tick = now;
        self.show_osd_time(self.position_ms, now);
    }

    fn seek_rel(&mut self, delta: i64, now: u64) {
        if self.duration_ms == 0 {
            return;
        }
        let position = if delta < 0 {
            self.position_ms.saturating_sub((-delta) as u64)
        } else {
            self.position_ms.saturating_add(delta as u64)
        };
        self.seek_to(position, now);
    }

    fn seek_percent(&mut self, percent: f32, now: u64) {
        if self.duration_ms == 0 {
            return;
        }
        let position = (self.duration_ms as f64 * percent.clamp(0.0, 1.0) as f64) as u64;
        self.seek_to(position, now);
    }

    fn step_frame(&mut self, delta: i64, now: u64) {
        if self.data.is_empty() {
            return;
        }
        self.playing = false;
        let frame = if delta < 0 {
            self.frame_index.saturating_sub((-delta) as u32)
        } else {
            self.frame_index.saturating_add(delta as u32)
        };
        self.frame_index = frame;
        self.position_ms = self.position_for_frame(frame).min(self.duration_ms);
        if !self.decode_current() && self.frame_index > 0 {
            self.frame_index = self.frame_index.saturating_sub(1);
            self.decode_current();
        }
        self.last_tick = now;
        self.show_osd("Frame step", now);
    }

    fn set_speed(&mut self, speed: f32, now: u64) {
        self.speed = speed.clamp(0.25, 4.0);
        self.last_tick = now;
        let mut text = Text::<48>::new();
        let _ = write!(text, "Speed {:.2}x", self.speed);
        self.show_osd(text.as_str(), now);
    }

    fn speed_up(&mut self, now: u64) {
        self.set_speed(self.speed * 1.25, now);
    }

    fn speed_down(&mut self, now: u64) {
        self.set_speed(self.speed / 1.25, now);
    }

    fn cycle_loop(&mut self, now: u64) {
        self.loop_mode = match self.loop_mode {
            LoopMode::Off => LoopMode::One,
            LoopMode::One => LoopMode::All,
            LoopMode::All => LoopMode::Off,
        };
        self.show_osd(self.loop_mode.label(), now);
    }

    fn toggle_shuffle(&mut self, now: u64) {
        self.shuffle = !self.shuffle;
        self.show_osd(
            if self.shuffle {
                "Shuffle on"
            } else {
                "Shuffle off"
            },
            now,
        );
    }

    fn next_index(&mut self) -> Option<usize> {
        if self.playlist_len == 0 {
            return None;
        }
        if self.shuffle && self.playlist_len > 1 {
            self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let mut index = self.rng as usize % self.playlist_len;
            if index == self.playlist_cursor {
                index = (index + 1) % self.playlist_len;
            }
            Some(index)
        } else {
            Some((self.playlist_cursor + 1) % self.playlist_len)
        }
    }

    fn previous_index(&self) -> Option<usize> {
        if self.playlist_len == 0 {
            return None;
        }
        Some(if self.playlist_cursor == 0 {
            self.playlist_len - 1
        } else {
            self.playlist_cursor - 1
        })
    }

    fn next_media(&mut self, fs: &dyn FileSystem, now: u64, play: bool) {
        if let Some(index) = self.next_index() {
            self.playlist_cursor = index;
            let node = self.playlist[index];
            self.open_node(fs, node, now, play);
        }
    }

    fn previous_media(&mut self, fs: &dyn FileSystem, now: u64, play: bool) {
        if let Some(index) = self.previous_index() {
            self.playlist_cursor = index;
            let node = self.playlist[index];
            self.open_node(fs, node, now, play);
        }
    }

    fn open_selected_playlist(&mut self, fs: &dyn FileSystem, now: u64) -> bool {
        if self.playlist_len == 0 {
            return false;
        }
        let index = self.playlist_cursor.min(self.playlist_len - 1);
        let node = self.playlist[index];
        self.open_node(fs, node, now, true);
        self.show_osd("Playing", now);
        true
    }

    fn remove_playlist_index(&mut self, index: usize, now: u64) {
        if index >= self.playlist_len {
            return;
        }
        for position in index..self.playlist_len - 1 {
            self.playlist[position] = self.playlist[position + 1];
        }
        self.playlist_len -= 1;
        if self.playlist_cursor >= self.playlist_len {
            self.playlist_cursor = self.playlist_len.saturating_sub(1);
        }
        self.show_osd("Removed from playlist", now);
    }

    fn clear_playlist(&mut self, now: u64) {
        self.playlist_len = 0;
        self.playlist_cursor = 0;
        self.playlist_scroll = 0;
        self.stop(now);
        self.current = None;
        self.info = None;
        self.data.clear();
        self.rgba.clear();
        self.scratch.clear();
        self.media_name.clear();
        self.media_path.clear();
        self.show_osd("Playlist cleared", now);
    }

    fn refresh_playlist(&mut self, fs: &dyn FileSystem, now: u64) {
        let current = self.current;
        self.scan_playlist(fs);
        if let Some(node) = current {
            if let Some(index) = (0..self.playlist_len).find(|index| self.playlist[*index] == node)
            {
                self.playlist_cursor = index;
            }
        }
        self.show_osd("Playlist refreshed", now);
    }

    fn toggle_fullscreen(&mut self, now: u64) {
        self.fullscreen = !self.fullscreen;
        self.last_activity = now;
        self.show_osd(
            if self.fullscreen {
                "Fullscreen"
            } else {
                "Windowed"
            },
            now,
        );
    }

    fn cycle_aspect(&mut self, now: u64) {
        self.aspect = match self.aspect {
            Aspect::Fit => Aspect::Fill,
            Aspect::Fill => Aspect::Stretch,
            Aspect::Stretch => Aspect::Original,
            Aspect::Original => Aspect::Fit,
        };
        self.show_osd(self.aspect.label(), now);
    }

    fn open_dialog(&mut self, fs: &dyn FileSystem, now: u64) {
        self.scan_all_media(fs);
        if self.open_len == 0 {
            self.set_notice(
                "Open File",
                "No video files were found on the filesystem.",
                now,
            );
            return;
        }
        if let Some(current) = self.current {
            if let Some(index) =
                (0..self.open_len).find(|index| self.open_results[*index] == current)
            {
                self.open_cursor = index;
            }
        }
        self.dialog = Dialog::Open;
        self.menu_open = None;
        self.last_activity = now;
    }

    fn show_osd(&mut self, message: &str, now: u64) {
        self.osd.set(message);
        self.osd_until = now.saturating_add(2200);
    }

    fn show_osd_time(&mut self, position: u64, now: u64) {
        let mut text = Text::<96>::new();
        let mut time = Text::<24>::new();
        format_time(position, &mut time);
        let _ = write!(text, "{} / ", time.as_str());
        format_time(self.duration_ms, &mut time);
        let _ = write!(text, "{}", time.as_str());
        self.osd.set(text.as_str());
        self.osd_until = now.saturating_add(1600);
    }

    fn set_notice(&mut self, title: &str, body: &str, now: u64) {
        self.notice_title.set(title);
        self.notice_body.set(body);
        self.dialog = Dialog::Notice;
        self.menu_open = None;
        self.context = None;
        self.last_activity = now;
    }

    fn controls_visible(&self, now: u64) -> bool {
        !self.fullscreen || !self.playing || now.saturating_sub(self.last_activity) < 2500
    }

    fn action_enabled(&self, action: Action) -> bool {
        match action {
            Action::None => false,
            Action::Open | Action::Quit | Action::TogglePlaylist | Action::Fullscreen => true,
            Action::PlayPause => self.current.is_some() || self.playlist_len > 0,
            Action::PlayAll => self.playlist_len > 0,
            Action::Stop | Action::SeekForward | Action::SeekBack | Action::MediaInfo => {
                self.current.is_some()
            }
            Action::JumpStart | Action::JumpEnd | Action::FrameNext | Action::FramePrev => {
                self.current.is_some()
            }
            Action::Faster | Action::Slower | Action::NormalSpeed => self.current.is_some(),
            Action::Previous | Action::Next => self.playlist_len > 1,
            Action::AspectFit
            | Action::AspectFill
            | Action::AspectStretch
            | Action::AspectOriginal => self.current.is_some(),
            Action::CycleLoop | Action::ToggleShuffle => self.playlist_len > 0,
            Action::Remove => self.playlist_len > 0,
            Action::ClearPlaylist => self.playlist_len > 0,
            Action::RefreshPlaylist => true,
            Action::About | Action::Help | Action::Preferences => true,
        }
    }

    fn action_checked(&self, action: Action) -> bool {
        match action {
            Action::TogglePlaylist => self.playlist_open,
            Action::Fullscreen => self.fullscreen,
            Action::CycleLoop => self.loop_mode != LoopMode::Off,
            Action::ToggleShuffle => self.shuffle,
            Action::AspectFit => self.aspect == Aspect::Fit,
            Action::AspectFill => self.aspect == Aspect::Fill,
            Action::AspectStretch => self.aspect == Aspect::Stretch,
            Action::AspectOriginal => self.aspect == Aspect::Original,
            _ => false,
        }
    }

    fn perform(
        &mut self,
        fs: &dyn FileSystem,
        action: Action,
        context: Option<ContextKind>,
        now: u64,
    ) {
        self.menu_open = None;
        self.context = None;
        match action {
            Action::None => {}
            Action::Open => self.open_dialog(fs, now),
            Action::Quit => self.exit = true,
            Action::PlayPause => {
                let _ = self.toggle_play(fs, now);
            }
            Action::PlayAll => {
                if self.playlist_len > 0 {
                    self.playlist_cursor = 0;
                    let node = self.playlist[0];
                    self.open_node(fs, node, now, true);
                    self.show_osd("Playing playlist", now);
                }
            }
            Action::Stop => self.stop(now),
            Action::Previous => {
                self.previous_media(fs, now, true);
                self.show_osd("Previous", now);
            }
            Action::Next => {
                self.next_media(fs, now, true);
                self.show_osd("Next", now);
            }
            Action::SeekForward => self.seek_rel(5000, now),
            Action::SeekBack => self.seek_rel(-5000, now),
            Action::JumpStart => self.seek_to(0, now),
            Action::JumpEnd => {
                if self.duration_ms > 0 {
                    self.seek_to(self.duration_ms.saturating_sub(1), now)
                }
            }
            Action::FrameNext => self.step_frame(1, now),
            Action::FramePrev => self.step_frame(-1, now),
            Action::Faster => self.speed_up(now),
            Action::Slower => self.speed_down(now),
            Action::NormalSpeed => self.set_speed(1.0, now),
            Action::Fullscreen => self.toggle_fullscreen(now),
            Action::TogglePlaylist => {
                self.playlist_open = !self.playlist_open;
                self.show_osd(
                    if self.playlist_open {
                        "Playlist"
                    } else {
                        "No playlist"
                    },
                    now,
                );
            }
            Action::AspectFit => {
                self.aspect = Aspect::Fit;
                self.show_osd(self.aspect.label(), now);
            }
            Action::AspectFill => {
                self.aspect = Aspect::Fill;
                self.show_osd(self.aspect.label(), now);
            }
            Action::AspectStretch => {
                self.aspect = Aspect::Stretch;
                self.show_osd(self.aspect.label(), now);
            }
            Action::AspectOriginal => {
                self.aspect = Aspect::Original;
                self.show_osd(self.aspect.label(), now);
            }
            Action::CycleLoop => self.cycle_loop(now),
            Action::ToggleShuffle => self.toggle_shuffle(now),
            Action::MediaInfo => {
                if self.current.is_some() {
                    self.dialog = Dialog::Info;
                }
            }
            Action::Remove => {
                if let Some(ContextKind::Playlist(index)) = context {
                    self.remove_playlist_index(index, now);
                }
            }
            Action::ClearPlaylist => self.clear_playlist(now),
            Action::RefreshPlaylist => self.refresh_playlist(fs, now),
            Action::About => self.dialog = Dialog::About,
            Action::Help => self.dialog = Dialog::Help,
            Action::Preferences => self.set_notice(
                "Preferences",
                "This build has no persistent preferences. The player starts \
                 with the first supported video and remembers playlist/view \
                 settings for the current session only.",
                now,
            ),
        }
    }
}

fn collect_media(
    fs: &dyn FileSystem,
    directory: NodeId,
    depth: usize,
    list: &mut [NodeId],
    count: &mut usize,
) {
    if depth > 12 || *count >= list.len() {
        return;
    }
    for index in 0..fs.child_count(directory) {
        if *count >= list.len() {
            return;
        }
        let Some(child) = fs.child_at(directory, index) else {
            continue;
        };
        if fs.is_dir(child) {
            collect_media(fs, child, depth + 1, list, count);
        } else if is_media_file(fs.name(child).unwrap_or("")) {
            list[*count] = child;
            *count += 1;
        }
    }
}

fn is_media_file(name: &str) -> bool {
    let extension = file_extension(name);
    [
        "gif", "avi", "mp4", "m4v", "mov", "mkv", "webm", "flv", "ivf", "h264", "264", "mjpeg",
        "mjpg",
    ]
    .iter()
    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

fn upper_extension(name: &str, out: &mut Text<16>) {
    out.clear();
    for byte in file_extension(name).bytes() {
        let _ = out.push((byte as char).to_ascii_uppercase());
    }
}

fn file_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension,
        _ => "",
    }
}

fn format_time(ms: u64, out: &mut Text<24>) {
    let total = ms / 1000;
    let hours = total / 3600;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    out.clear();
    if hours > 0 {
        let _ = write!(out, "{hours}:{minutes:02}:{seconds:02}");
    } else if ms > 0 && ms < 10_000 {
        let tenths = (ms % 1000) / 100;
        let _ = write!(out, "{minutes:02}:{seconds:02}.{tenths}");
    } else {
        let _ = write!(out, "{minutes:02}:{seconds:02}");
    }
}

fn format_fps(info: &VideoInfo) -> Text<24> {
    let mut text = Text::<24>::new();
    if let Some((num, den)) = info.frame_rate {
        if den > 0 {
            let fps = num as f64 / den as f64;
            let _ = write!(text, "{fps:.1} fps");
        }
    }
    text
}

impl PlayerState {
    fn handle_pointer(&mut self, fs: &dyn FileSystem, report: PointerEvent, now: u64) -> bool {
        self.pointer = Point::new(report.x as i32, report.y as i32);
        self.has_pointer = true;
        self.last_activity = now;

        if let Some(true) = report.left {
            self.mouse_down = true;
            if self.on_left_down(fs, self.pointer, now) {
                self.exit = true;
            }
        }
        if self.mouse_down && matches!(self.drag, DragMode::Seek) {
            self.update_seek_drag(now);
        }
        if report.wheel != 0 && !self.mouse_down {
            self.handle_wheel(fs, report.wheel, now);
        }
        if let Some(false) = report.left {
            if matches!(self.drag, DragMode::Seek) && self.resume_after_seek {
                self.playing = true;
                self.last_tick = now;
            }
            self.resume_after_seek = false;
            self.mouse_down = false;
            self.drag = DragMode::None;
        }
        if let Some(true) = report.right {
            self.on_right_click(fs, self.pointer, now);
        }
        self.exit
    }

    fn update_seek_drag(&mut self, now: u64) {
        let layout = Layout::compute(self, now);
        let seek = seek_rect(layout);
        if seek.width > 0 {
            let percent = (self.pointer.x - seek.x) as f32 / seek.width as f32;
            self.seek_percent(percent, now);
        }
    }

    fn handle_wheel(&mut self, fs: &dyn FileSystem, wheel: i32, now: u64) {
        let layout = Layout::compute(self, now);
        if self.dialog == Dialog::Open {
            let dialog = DialogLayout::compute(self.screen, self.dialog);
            let rows = open_visible_rows(dialog);
            let max = self.open_len.saturating_sub(rows);
            if wheel > 0 {
                self.open_scroll = self.open_scroll.saturating_sub(3);
            } else {
                self.open_scroll = (self.open_scroll + 3).min(max);
            }
            return;
        }
        if self.dialog != Dialog::None {
            return;
        }
        if let Some(playlist) = layout.playlist {
            if playlist.contains(self.pointer) {
                let rows = playlist_visible_rows(playlist);
                let max = self.playlist_len.saturating_sub(rows);
                if wheel > 0 {
                    self.playlist_scroll = self.playlist_scroll.saturating_sub(2);
                } else {
                    self.playlist_scroll = (self.playlist_scroll + 2).min(max);
                }
                return;
            }
        }
        if layout.video.contains(self.pointer) {
            if wheel > 0 {
                self.seek_rel(5000, now);
            } else {
                self.seek_rel(-5000, now);
            }
        }
        let _ = fs;
    }

    fn on_left_down(&mut self, fs: &dyn FileSystem, point: Point, now: u64) -> bool {
        let layout = Layout::compute(self, now);
        if self.dialog != Dialog::None {
            return self.dialog_click(fs, point, now);
        }
        if let Some(menu) = self.menu_open {
            if let Some(index) = menu_item_at(self, menu, point) {
                let entries = menu_items(menu);
                if let Some(entry) = entries.get(index).copied() {
                    if entry.enabled && !entry.separator && self.action_enabled(entry.action) {
                        self.perform(fs, entry.action, None, now);
                    }
                }
                return false;
            }
            for kind in MenuKind::ALL {
                if menu_label_rect(layout, kind).contains(point) {
                    if kind == menu {
                        self.menu_open = None;
                    } else {
                        self.menu_open = Some(kind);
                        self.menu_hover = first_enabled(kind, self);
                    }
                    return false;
                }
            }
            self.menu_open = None;
            return false;
        }
        if let Some(context) = self.context {
            if context_rect(self, context).contains(point) {
                if let Some(index) = context_item_at(self, context, point) {
                    let entries = context_entries(context.kind);
                    if let Some(entry) = entries.get(index).copied() {
                        if entry.enabled && !entry.separator {
                            self.perform(fs, entry.action, Some(context.kind), now);
                        }
                    }
                }
                return false;
            }
            self.context = None;
            return false;
        }

        for kind in MenuKind::ALL {
            if menu_label_rect(layout, kind).contains(point) {
                self.menu_open = Some(kind);
                self.menu_hover = first_enabled(kind, self);
                return false;
            }
        }

        if !self.embedded {
            if let Some(button) = title_button_at(layout, point) {
                match button {
                    0 => self.show_osd("Minimize is handled by the desktop", now),
                    1 => self.toggle_fullscreen(now),
                    _ => {
                        self.exit = true;
                        return true;
                    }
                }
                return false;
            }
        }

        if !layout.toolbar.is_empty() {
            if let Some(index) = toolbar_button_at(layout, point) {
                return self.toolbar_action(fs, index, now);
            }
        }

        if self.controls_visible(now) && seek_rect(layout).contains(point) {
            self.drag = DragMode::Seek;
            self.resume_after_seek = self.playing;
            self.update_seek_drag(now);
            self.pause_for_seek(now);
            return false;
        }
        if self.fullscreen && self.controls_visible(now) {
            if fullscreen_play_rect(layout).contains(point) {
                let _ = self.toggle_play(fs, now);
                return false;
            }
            if fullscreen_prev_rect(layout).contains(point) {
                self.previous_media(fs, now, true);
                self.show_osd("Previous", now);
                return false;
            }
            if fullscreen_next_rect(layout).contains(point) {
                self.next_media(fs, now, true);
                self.show_osd("Next", now);
                return false;
            }
            if layout.controls.contains(point) {
                return false;
            }
        }

        if let Some(playlist) = layout.playlist {
            if playlist.contains(point) {
                self.focus = Focus::Playlist;
                if let Some(index) = playlist_at(self, layout, point) {
                    let double = self.last_click_index == Some(index)
                        && now.saturating_sub(self.last_click_ms) < 480;
                    self.playlist_cursor = index;
                    self.last_click_index = Some(index);
                    self.last_click_ms = now;
                    if double {
                        self.last_click_index = None;
                        let node = self.playlist[index];
                        self.open_node(fs, node, now, true);
                    }
                }
                return false;
            }
        }

        if layout.video.contains(point) {
            self.focus = Focus::Video;
            let double = now.saturating_sub(self.last_video_click_ms) < 420;
            self.last_video_click_ms = now;
            if double {
                self.toggle_fullscreen(now);
            } else {
                let _ = self.toggle_play(fs, now);
            }
        }
        false
    }

    fn pause_for_seek(&mut self, now: u64) {
        if self.playing {
            self.playing = false;
            self.last_tick = now;
        }
    }

    fn toolbar_action(&mut self, fs: &dyn FileSystem, index: usize, now: u64) -> bool {
        match index {
            0 => self.perform(fs, Action::Open, None, now),
            1 => self.perform(fs, Action::Previous, None, now),
            2 => self.perform(fs, Action::PlayPause, None, now),
            3 => self.perform(fs, Action::Stop, None, now),
            4 => self.perform(fs, Action::Next, None, now),
            5 => self.perform(fs, Action::Fullscreen, None, now),
            6 => self.perform(fs, Action::TogglePlaylist, None, now),
            7 => self.perform(fs, Action::CycleLoop, None, now),
            8 => self.perform(fs, Action::ToggleShuffle, None, now),
            _ => self.perform(fs, Action::MediaInfo, None, now),
        }
        false
    }

    fn on_right_click(&mut self, _fs: &dyn FileSystem, point: Point, now: u64) {
        let layout = Layout::compute(self, now);
        if self.dialog != Dialog::None {
            return;
        }
        if let Some(menu) = self.menu_open {
            if menu_rect(self, menu).contains(point) {
                return;
            }
            self.menu_open = None;
        }
        self.context = None;
        if let Some(playlist) = layout.playlist {
            if playlist.contains(point) {
                if let Some(index) = playlist_at(self, layout, point) {
                    self.playlist_cursor = index;
                    self.context = Some(ContextMenu {
                        kind: ContextKind::Playlist(index),
                        x: point.x,
                        y: point.y,
                        hover: usize::MAX,
                    });
                    return;
                }
                self.context = Some(ContextMenu {
                    kind: ContextKind::Background,
                    x: point.x,
                    y: point.y,
                    hover: usize::MAX,
                });
                return;
            }
        }
        if layout.video.contains(point) {
            self.context = Some(ContextMenu {
                kind: ContextKind::Video,
                x: point.x,
                y: point.y,
                hover: usize::MAX,
            });
        }
    }

    fn dialog_click(&mut self, fs: &dyn FileSystem, point: Point, now: u64) -> bool {
        let dialog = self.dialog;
        if dialog == Dialog::None {
            return false;
        }
        let layout = DialogLayout::compute(self.screen, dialog);
        if layout.close.contains(point) {
            self.dialog = Dialog::None;
            return false;
        }
        if layout.cancel.contains(point) && dialog == Dialog::Open {
            self.dialog = Dialog::None;
            return false;
        }
        if layout.ok.contains(point) {
            if dialog == Dialog::Open {
                self.open_selected_file(fs, now);
            } else {
                self.dialog = Dialog::None;
            }
            return false;
        }
        if dialog == Dialog::Open {
            if let Some(index) = open_row_at(self, layout, point) {
                let double = self.last_open_index == Some(index)
                    && now.saturating_sub(self.last_open_click_ms) < 480;
                self.open_cursor = index;
                self.last_open_index = Some(index);
                self.last_open_click_ms = now;
                if double {
                    self.last_open_index = None;
                    self.open_dialog_index(fs, index, now);
                }
            }
        }
        false
    }

    fn open_selected_file(&mut self, fs: &dyn FileSystem, now: u64) {
        if self.open_len > 0 {
            let index = self.open_cursor.min(self.open_len - 1);
            self.open_dialog_index(fs, index, now);
        }
    }

    fn open_dialog_index(&mut self, fs: &dyn FileSystem, index: usize, now: u64) {
        if index >= self.open_len {
            return;
        }
        let node = self.open_results[index];
        self.dialog = Dialog::None;
        self.ensure_in_playlist(node);
        self.open_node(fs, node, now, true);
        self.show_osd("Playing", now);
    }

    fn handle_key(
        &mut self,
        fs: &dyn FileSystem,
        key: Key,
        modifiers: Modifiers,
        now: u64,
    ) -> bool {
        self.last_activity = now;
        if self.dialog != Dialog::None {
            self.dialog_key(fs, key, now);
            return false;
        }
        if self.menu_open.is_some() {
            self.menu_key(fs, key, now);
            return false;
        }
        if let Some(context) = self.context {
            match key {
                Key::Escape => self.context = None,
                Key::Up => self.context = Some(context_move(self, context, -1)),
                Key::Down => self.context = Some(context_move(self, context, 1)),
                Key::Enter => {
                    let entries = context_entries(context.kind);
                    if let Some(entry) = entries.get(context.hover).copied() {
                        if entry.enabled && !entry.separator {
                            self.perform(fs, entry.action, Some(context.kind), now);
                        }
                    }
                }
                _ => {}
            }
            return false;
        }

        if key == Key::F1 {
            self.dialog = Dialog::Help;
            return false;
        }
        if key == Key::F5 {
            self.perform(fs, Action::RefreshPlaylist, None, now);
            return false;
        }
        if key == Key::F10 {
            if self.fullscreen {
                return false;
            }
            self.menu_open = Some(MenuKind::Media);
            self.menu_hover = first_enabled(MenuKind::Media, self);
            return false;
        }
        if key == Key::F11 {
            self.toggle_fullscreen(now);
            return false;
        }
        if key == Key::Tab {
            if self.fullscreen {
                return false;
            }
            if self.playlist_open {
                self.focus = match self.focus {
                    Focus::Video => Focus::Playlist,
                    Focus::Playlist => Focus::Video,
                };
            } else {
                self.playlist_open = true;
                self.focus = Focus::Playlist;
            }
            return false;
        }
        if key == Key::Escape {
            if self.fullscreen {
                self.toggle_fullscreen(now);
            } else {
                self.exit = true;
            }
            return false;
        }

        if self.focus == Focus::Playlist {
            self.playlist_key(fs, key, modifiers, now);
            return false;
        }

        match key {
            Key::Space => {
                let _ = self.toggle_play(fs, now);
            }
            Key::Enter => {
                let _ = self.open_selected_playlist(fs, now);
            }
            Key::Character('s') | Key::Character('S') => self.stop(now),
            Key::Character('b') | Key::Character('B') => {
                self.previous_media(fs, now, true);
                self.show_osd("Previous", now);
            }
            Key::Character('n') | Key::Character('N') => {
                self.next_media(fs, now, true);
                self.show_osd("Next", now);
            }
            Key::Character('o') | Key::Character('O') => self.open_dialog(fs, now),
            Key::Character('p') | Key::Character('P') => {
                self.perform(fs, Action::TogglePlaylist, None, now)
            }
            Key::Character('f') | Key::Character('F') => self.toggle_fullscreen(now),
            Key::Character('l') | Key::Character('L') => self.cycle_loop(now),
            Key::Character('r') | Key::Character('R') => self.toggle_shuffle(now),
            Key::Character('i') | Key::Character('I') => {
                self.perform(fs, Action::MediaInfo, None, now)
            }
            Key::Character('a') | Key::Character('A') => self.cycle_aspect(now),
            Key::Character('h') | Key::Character('H') => self.dialog = Dialog::Help,
            Key::Character('m') | Key::Character('M') => self.set_notice(
                "Audio",
                "The feos video containers exposed by feos-video currently \
                 carry a video track only; this build has no audio stream to \
                 mute or control.",
                now,
            ),
            Key::Character('q') | Key::Character('Q') => self.exit = true,
            Key::Left => self.seek_rel(
                if modifiers.contains(Modifiers::SHIFT) {
                    -1000
                } else {
                    -5000
                },
                now,
            ),
            Key::Right => self.seek_rel(
                if modifiers.contains(Modifiers::SHIFT) {
                    1000
                } else {
                    5000
                },
                now,
            ),
            Key::PageUp => self.seek_rel(-30000, now),
            Key::PageDown => self.seek_rel(30000, now),
            Key::Home => self.seek_to(0, now),
            Key::End => {
                if self.duration_ms > 0 {
                    self.seek_to(self.duration_ms.saturating_sub(1), now);
                }
            }
            Key::Character(',') => self.step_frame(-1, now),
            Key::Character('.') => self.step_frame(1, now),
            Key::Character('+') | Key::Character('=') => self.speed_up(now),
            Key::Character('-') | Key::Character('_') => self.speed_down(now),
            Key::Character('0') => self.set_speed(1.0, now),
            Key::Up => self.set_notice(
                "Audio",
                "Volume controls are unavailable because the current video \
                 containers have no audio track.",
                now,
            ),
            Key::Down => self.set_notice(
                "Audio",
                "Volume controls are unavailable because the current video \
                 containers have no audio track.",
                now,
            ),
            _ => {}
        }
        false
    }

    fn playlist_key(&mut self, fs: &dyn FileSystem, key: Key, modifiers: Modifiers, now: u64) {
        let rows = Layout::compute(self, now)
            .playlist
            .map(playlist_visible_rows)
            .unwrap_or(5);
        match key {
            Key::Up => {
                self.playlist_cursor = self.playlist_cursor.saturating_sub(1);
                self.ensure_playlist_visible(rows);
            }
            Key::Down => {
                if self.playlist_len > 0 {
                    self.playlist_cursor = (self.playlist_cursor + 1).min(self.playlist_len - 1);
                }
                self.ensure_playlist_visible(rows);
            }
            Key::Home => {
                self.playlist_cursor = 0;
                self.ensure_playlist_visible(rows);
            }
            Key::End => {
                self.playlist_cursor = self.playlist_len.saturating_sub(1);
                self.ensure_playlist_visible(rows);
            }
            Key::PageUp => {
                self.playlist_cursor = self.playlist_cursor.saturating_sub(rows);
                self.ensure_playlist_visible(rows);
            }
            Key::PageDown => {
                self.playlist_cursor =
                    (self.playlist_cursor + rows).min(self.playlist_len.saturating_sub(1));
                self.ensure_playlist_visible(rows);
            }
            Key::Enter | Key::Space => {
                if self.open_selected_playlist(fs, now) {
                    self.focus = Focus::Video;
                }
            }
            Key::Delete => {
                if !modifiers.contains(Modifiers::CTRL) {
                    self.remove_playlist_index(self.playlist_cursor, now);
                }
            }
            Key::Escape => self.focus = Focus::Video,
            _ => {}
        }
    }

    fn ensure_playlist_visible(&mut self, rows: usize) {
        if self.playlist_cursor < self.playlist_scroll {
            self.playlist_scroll = self.playlist_cursor;
        } else if self.playlist_cursor >= self.playlist_scroll + rows {
            self.playlist_scroll = self.playlist_cursor + 1 - rows;
        }
    }

    fn dialog_key(&mut self, fs: &dyn FileSystem, key: Key, now: u64) {
        match self.dialog {
            Dialog::Open => {
                let rows = open_visible_rows(DialogLayout::compute(self.screen, self.dialog));
                let max = self.open_len.saturating_sub(rows);
                match key {
                    Key::Escape => self.dialog = Dialog::None,
                    Key::Up => {
                        self.open_cursor = self.open_cursor.saturating_sub(1);
                    }
                    Key::Down => {
                        if self.open_len > 0 {
                            self.open_cursor = (self.open_cursor + 1).min(self.open_len - 1);
                        }
                    }
                    Key::Home => self.open_cursor = 0,
                    Key::End => self.open_cursor = self.open_len.saturating_sub(1),
                    Key::PageUp => self.open_cursor = self.open_cursor.saturating_sub(rows),
                    Key::PageDown => {
                        self.open_cursor =
                            (self.open_cursor + rows).min(self.open_len.saturating_sub(1))
                    }
                    Key::Enter => self.open_selected_file(fs, now),
                    _ => {}
                }
                if self.open_cursor < self.open_scroll {
                    self.open_scroll = self.open_cursor;
                } else if self.open_cursor >= self.open_scroll + rows {
                    self.open_scroll = self.open_cursor + 1 - rows;
                }
                if self.open_scroll > max {
                    self.open_scroll = max;
                }
            }
            Dialog::None => {}
            _ => {
                if key == Key::Escape || key == Key::Enter || key == Key::Space {
                    self.dialog = Dialog::None;
                }
            }
        }
    }

    fn menu_key(&mut self, fs: &dyn FileSystem, key: Key, now: u64) {
        let Some(mut kind) = self.menu_open else {
            return;
        };
        let entries = menu_items(kind);
        match key {
            Key::Escape => self.menu_open = None,
            Key::Up => self.menu_hover = next_enabled(entries, self.menu_hover, -1, self),
            Key::Down => self.menu_hover = next_enabled(entries, self.menu_hover, 1, self),
            Key::Left => {
                let index = if kind.index() == 0 {
                    MenuKind::ALL.len() - 1
                } else {
                    kind.index() - 1
                };
                kind = MenuKind::ALL[index];
                self.menu_open = Some(kind);
                self.menu_hover = first_enabled(kind, self);
            }
            Key::Right => {
                kind = MenuKind::ALL[(kind.index() + 1) % MenuKind::ALL.len()];
                self.menu_open = Some(kind);
                self.menu_hover = first_enabled(kind, self);
            }
            Key::Enter => {
                if let Some(entry) = entries.get(self.menu_hover).copied() {
                    if entry.enabled && !entry.separator && self.action_enabled(entry.action) {
                        self.perform(fs, entry.action, None, now);
                    }
                }
            }
            _ => {}
        }
    }
}

fn first_enabled(kind: MenuKind, state: &PlayerState) -> usize {
    menu_items(kind)
        .iter()
        .position(|entry| !entry.separator && entry.enabled && state.action_enabled(entry.action))
        .unwrap_or(0)
}

fn next_enabled(entries: &[MenuItem], current: usize, delta: isize, state: &PlayerState) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let mut index = current.min(entries.len() - 1) as isize;
    for _ in 0..entries.len() {
        index = if delta < 0 {
            if index <= 0 {
                entries.len() as isize - 1
            } else {
                index - 1
            }
        } else {
            (index + 1) % entries.len() as isize
        };
        let entry = entries[index as usize];
        if !entry.separator && entry.enabled && state.action_enabled(entry.action) {
            return index as usize;
        }
    }
    current
}

fn context_entries(kind: ContextKind) -> &'static [MenuItem] {
    match kind {
        ContextKind::Video => CONTEXT_VIDEO,
        ContextKind::Playlist(_) => CONTEXT_PLAYLIST,
        ContextKind::Background => CONTEXT_BACKGROUND,
    }
}

fn context_move(state: &PlayerState, mut context: ContextMenu, delta: isize) -> ContextMenu {
    let _ = state;
    let entries = context_entries(context.kind);
    if entries.is_empty() {
        return context;
    }
    let mut index = if context.hover >= entries.len() {
        if delta < 0 {
            entries.len() as isize - 1
        } else {
            0
        }
    } else {
        context.hover as isize
    };
    for _ in 0..entries.len() {
        index = if delta < 0 {
            if index <= 0 {
                entries.len() as isize - 1
            } else {
                index - 1
            }
        } else {
            (index + 1) % entries.len() as isize
        };
        let entry = entries[index as usize];
        if !entry.separator && entry.enabled {
            context.hover = index as usize;
            break;
        }
    }
    context
}

#[derive(Clone, Copy)]
struct Layout {
    screen: Rect,
    title: Rect,
    menu_bar: Rect,
    toolbar: Rect,
    video: Rect,
    playlist: Option<Rect>,
    controls: Rect,
    status: Rect,
}

impl Layout {
    fn compute(state: &PlayerState, now: u64) -> Self {
        let screen = state.screen;
        if state.fullscreen {
            let controls = if state.controls_visible(now) {
                Rect::new(
                    screen.x,
                    screen.bottom_i32() - CONTROLS_HEIGHT,
                    screen.width,
                    CONTROLS_HEIGHT as u32,
                )
            } else {
                empty_rect()
            };
            return Self {
                screen,
                title: empty_rect(),
                menu_bar: empty_rect(),
                toolbar: empty_rect(),
                video: screen,
                playlist: None,
                controls,
                status: empty_rect(),
            };
        }
        let title_height = if state.embedded { 0 } else { TITLE_HEIGHT };
        let (title, rest) = screen.split_top(title_height);
        let (menu_bar, rest) = rest.split_top(MENU_HEIGHT);
        let (toolbar, rest) = rest.split_top(TOOLBAR_HEIGHT);
        let (status, rest) = rest.split_bottom(STATUS_HEIGHT);
        let (controls, body) = rest.split_bottom(CONTROLS_HEIGHT);
        let body = body.inset_each(2, 0, 2, 0);
        let (playlist, video) = if state.playlist_open && body.width as i32 > 680 {
            let (playlist, video) = body.split_right(PLAYLIST_WIDTH);
            (Some(playlist), video)
        } else {
            (None, body)
        };
        Self {
            screen,
            title,
            menu_bar,
            toolbar,
            video,
            playlist,
            controls,
            status,
        }
    }

    fn controls_visible(&self, state: &PlayerState, now: u64) -> bool {
        !self.controls.is_empty() && state.controls_visible(now)
    }
}

#[derive(Clone, Copy)]
struct DialogLayout {
    window: Rect,
    title: Rect,
    close: Rect,
    body: Rect,
    ok: Rect,
    cancel: Rect,
}

impl DialogLayout {
    fn compute(screen: Rect, dialog: Dialog) -> Self {
        let (mut width, mut height) = match dialog {
            Dialog::None => (1, 1),
            Dialog::Open => (760, 540),
            Dialog::Info => (600, 460),
            Dialog::About => (560, 330),
            Dialog::Help => (680, 470),
            Dialog::Notice => (500, 250),
        };
        width = width.min(screen.width as i32 - 24).max(320);
        height = height.min(screen.height as i32 - 24).max(200);
        let window = Rect::new(
            screen.x + (screen.width as i32 - width) / 2,
            screen.y + (screen.height as i32 - height) / 2,
            width as u32,
            height as u32,
        );
        let inner = window.inset(2);
        let (title, rest) = inner.split_top(28);
        let body = rest.inset_each(10, 8, 10, 42);
        let close = Rect::new(title.right_i32() - 23, title.y + 4, 19, 20);
        let ok = Rect::new(window.right_i32() - 100, window.bottom_i32() - 32, 82, 24);
        let cancel = Rect::new(window.right_i32() - 192, window.bottom_i32() - 32, 84, 24);
        Self {
            window,
            title,
            close,
            body,
            ok,
            cancel,
        }
    }
}

fn empty_rect() -> Rect {
    Rect::new(0, 0, 0, 0)
}

fn menu_label_rect(layout: Layout, kind: MenuKind) -> Rect {
    let mut x = layout.menu_bar.x + 4;
    for index in 0..kind.index() {
        x += MENU_WIDTHS[index];
    }
    Rect::new(
        x,
        layout.menu_bar.y,
        MENU_WIDTHS[kind.index()] as u32,
        layout.menu_bar.height,
    )
}

fn menu_height(kind: MenuKind) -> i32 {
    let mut height = 6;
    for entry in menu_items(kind) {
        height += if entry.separator { 7 } else { 22 };
    }
    height
}

fn menu_rect(state: &PlayerState, kind: MenuKind) -> Rect {
    let layout = Layout::compute(state, state.last_activity);
    let label = menu_label_rect(layout, kind);
    let width = 286i32.min(state.screen.width as i32 - 8);
    let x = label.x.min(state.screen.right_i32() - width - 3).max(2);
    Rect::new(
        x,
        label.bottom_i32(),
        width as u32,
        menu_height(kind) as u32,
    )
}

fn menu_item_at(state: &PlayerState, kind: MenuKind, point: Point) -> Option<usize> {
    let rect = menu_rect(state, kind);
    if !rect.contains(point) {
        return None;
    }
    let mut y = rect.y + 3;
    for (index, entry) in menu_items(kind).iter().enumerate() {
        let height = if entry.separator { 7 } else { 22 };
        if point.y >= y && point.y < y + height {
            return Some(index);
        }
        y += height;
    }
    None
}

fn context_height(kind: ContextKind) -> i32 {
    let mut height = 6;
    for entry in context_entries(kind) {
        height += if entry.separator { 7 } else { 22 };
    }
    height
}

fn context_rect(state: &PlayerState, context: ContextMenu) -> Rect {
    let width = 250i32.min(state.screen.width as i32 - 8);
    let height = context_height(context.kind);
    let x = context.x.min(state.screen.right_i32() - width - 2).max(1);
    let y = context.y.min(state.screen.bottom_i32() - height - 2).max(1);
    Rect::new(x, y, width as u32, height as u32)
}

fn context_item_at(state: &PlayerState, context: ContextMenu, point: Point) -> Option<usize> {
    let rect = context_rect(state, context);
    if !rect.contains(point) {
        return None;
    }
    let mut y = rect.y + 3;
    for (index, entry) in context_entries(context.kind).iter().enumerate() {
        let height = if entry.separator { 7 } else { 22 };
        if point.y >= y && point.y < y + height {
            return Some(index);
        }
        y += height;
    }
    None
}

fn title_button_rect(layout: Layout, index: usize) -> Rect {
    let offset = match index {
        0 => 45,
        1 => 24,
        _ => 3,
    };
    Rect::new(
        layout.title.right_i32() - offset - 18,
        layout.title.y + 5,
        18,
        20,
    )
}

fn title_button_at(layout: Layout, point: Point) -> Option<usize> {
    for index in 0..3 {
        if title_button_rect(layout, index).contains(point) {
            return Some(index);
        }
    }
    None
}

fn toolbar_button_rect(layout: Layout, index: usize) -> Rect {
    let mut x = layout.toolbar.x + 5;
    for current in 0..index {
        x += TOOLBAR_WIDTHS[current];
    }
    Rect::new(
        x,
        layout.toolbar.y + 5,
        TOOLBAR_WIDTHS[index] as u32,
        (layout.toolbar.height as i32 - 10).max(1) as u32,
    )
}

fn toolbar_button_at(layout: Layout, point: Point) -> Option<usize> {
    if layout.toolbar.is_empty() || !layout.toolbar.contains(point) {
        return None;
    }
    for index in 0..TOOLBAR_WIDTHS.len() {
        if toolbar_button_rect(layout, index).contains(point) {
            return Some(index);
        }
    }
    None
}

fn fullscreen_play_rect(layout: Layout) -> Rect {
    Rect::new(
        layout.controls.x + 18,
        seek_rect(layout).bottom_i32() + 2,
        30,
        22,
    )
}

fn fullscreen_prev_rect(layout: Layout) -> Rect {
    Rect::new(
        fullscreen_play_rect(layout).right_i32() + 6,
        seek_rect(layout).bottom_i32() + 2,
        24,
        22,
    )
}

fn fullscreen_next_rect(layout: Layout) -> Rect {
    Rect::new(
        fullscreen_prev_rect(layout).right_i32() + 2,
        seek_rect(layout).bottom_i32() + 2,
        24,
        22,
    )
}

fn seek_rect(layout: Layout) -> Rect {
    if layout.controls.is_empty() {
        return empty_rect();
    }
    Rect::new(
        layout.controls.x + 24,
        layout.controls.y + 8,
        layout.controls.width.saturating_sub(48),
        12,
    )
}

fn playlist_list_rect(playlist: Rect) -> Rect {
    Rect::new(
        playlist.x + 1,
        playlist.y + 30,
        playlist.width.saturating_sub(2),
        playlist.height.saturating_sub(52),
    )
}

fn playlist_visible_rows(playlist: Rect) -> usize {
    (playlist_list_rect(playlist).height as i32 / 32).max(1) as usize
}

fn playlist_at(state: &PlayerState, layout: Layout, point: Point) -> Option<usize> {
    let playlist = layout.playlist?;
    let list = playlist_list_rect(playlist);
    if !list.contains(point) {
        return None;
    }
    let row = ((point.y - list.y) / 32).max(0) as usize;
    let index = state.playlist_scroll + row;
    if index < state.playlist_len {
        Some(index)
    } else {
        None
    }
}

fn open_list_rect(layout: DialogLayout) -> Rect {
    Rect::new(
        layout.body.x,
        layout.body.y + 24,
        layout.body.width,
        layout.body.height.saturating_sub(32),
    )
}

fn open_visible_rows(layout: DialogLayout) -> usize {
    (open_list_rect(layout).height as i32 / 24).max(1) as usize
}

fn open_row_at(state: &PlayerState, layout: DialogLayout, point: Point) -> Option<usize> {
    let list = open_list_rect(layout);
    if !list.contains(point) {
        return None;
    }
    let row = ((point.y - list.y) / 24).max(0) as usize;
    let index = state.open_scroll + row;
    if index < state.open_len {
        Some(index)
    } else {
        None
    }
}

/// Runs the VLC-style player.
/// State of a media player hosted inside a desktop window.
static WINDOW: SpinLock<Option<PlayerState>> = SpinLock::new(None);

pub(super) fn open_player(fs: &dyn FileSystem, context: &WindowContext<'_, '_>) -> bool {
    let screen = Rect::new(
        0,
        0,
        context.canvas.width() as u32,
        context.canvas.height() as u32,
    );
    let mut state = PlayerState::new(fs, screen, context.args, context.now_ms);
    state.has_pointer = true;
    state.pointer = context.pointer();
    state.show_cursor = false;
    state.embedded = true;
    *WINDOW.lock() = Some(state);
    true
}

pub(super) fn frame_player(
    fs: &dyn FileSystem,
    context: &mut WindowContext<'_, '_>,
) -> WindowFrame {
    let mut guard = WINDOW.lock();
    let Some(state) = guard.as_mut() else {
        return WindowFrame::Close;
    };
    let now = context.now_ms;

    context.dispatch_pointer(|report| {
        if state.handle_pointer(fs, report, now) {
            state.exit = true;
        }
    });
    context.dispatch_keys(|event| {
        if let Event::KeyDown(key, modifiers) = event {
            if state.handle_key(fs, key, modifiers, now) {
                state.exit = true;
            }
        }
    });
    state.tick(fs, now);

    if state.exit {
        return WindowFrame::Close;
    }
    render(context.canvas, fs, state, now);
    WindowFrame::Continue
}

pub(super) fn close_player() {
    *WINDOW.lock() = None;
}

pub fn run(fs: &dyn FileSystem, context: &mut Context<'_>) {
    let platform = context.platform;
    let suspend = context.suspend;
    let suspend_area = context.suspend_area;
    let Some(display) = context.display.as_mut() else {
        println!("player: no framebuffer; video playback needs ramfb");
        return;
    };
    let full = Rect::new(0, 0, display.width() as u32, display.height() as u32);
    let view = context.viewport.unwrap_or(full).clipped_to(full);
    if view.is_empty() {
        return;
    }
    let screen = Rect::new(0, 0, view.width, view.height);
    let now = platform.clock.uptime_ms();
    let mut state = PlayerState::new(fs, screen, context.args, now);
    state.has_pointer = platform.pointer.is_some();
    if let Some(pointer) = platform.pointer {
        let (x, y) = pointer.position();
        state.pointer = Point::new(x as i32 - view.x, y as i32 - view.y);
    }
    let width = view.width as usize;
    let height = view.height as usize;
    let mut back_pixels = vec![0u32; width * height];
    let mut back = Canvas::new(&mut back_pixels, width, height, width);

    let mut input = Input::new();
    let mut keys = SerialKeys::new();
    let mut dirty = true;
    let mut last_render = now;

    draw_frame(display, back.as_mut(), fs, &mut state, now, view);

    loop {
        let now = platform.clock.uptime_ms();
        if keys.poll(now, &mut input) {
            break;
        }

        let mut pointer_changed = false;
        let mut suspend_requested = false;
        if let Some(pointer) = platform.pointer {
            let mut budget = 32;
            while budget > 0 {
                let Some(mut report) = pointer.poll() else {
                    break;
                };
                if let (Some(cell), Some(area), Some(true)) = (suspend, suspend_area, report.left) {
                    if area.contains(Point::new(report.x as i32, report.y as i32)) {
                        cell.set(true);
                        suspend_requested = true;
                        break;
                    }
                }
                report.x = report.x.saturating_sub(view.x as u16);
                report.y = report.y.saturating_sub(view.y as u16);
                pointer_changed = true;
                state.handle_pointer(fs, report, now);
                budget -= 1;
            }
        }
        if suspend_requested {
            break;
        }

        let input_changed = !input.is_empty();
        if input_changed {
            for event in input.events() {
                match event {
                    Event::KeyDown(key, modifiers) => {
                        if state.handle_key(fs, key, modifiers, now) {
                            state.exit = true;
                        }
                    }
                    Event::Resize(size) => {
                        state.set_screen(Rect::new(0, 0, size.width, size.height));
                    }
                    _ => {}
                }
            }
        }

        let playback_changed = state.tick(fs, now);
        if state.exit {
            break;
        }
        let osd_active = state.osd_until > now;
        let fullscreen_controls =
            state.fullscreen && state.playing && now.saturating_sub(state.last_activity) < 2600;
        if dirty
            || input_changed
            || pointer_changed
            || playback_changed
            || (state.playing && now.saturating_sub(last_render) >= 100)
            || fullscreen_controls
            || osd_active
        {
            draw_frame(display, back.as_mut(), fs, &mut state, now, view);
            dirty = false;
            last_render = now;
        }
        if !state.playing && !osd_active && !fullscreen_controls {
            dirty = false;
        }

        input.clear_events();
        platform.clock.idle_ms(10);
    }
    println!("[player] closed");
}

fn draw_frame(
    front: &mut Canvas<'static>,
    back: Option<&mut Canvas<'_>>,
    fs: &dyn FileSystem,
    state: &mut PlayerState,
    now: u64,
    view: Rect,
) {
    if let Some(back) = back {
        render(back, fs, state, now);
        front.blit(
            view.x,
            view.y,
            back.width() as u32,
            back.height() as u32,
            back.pixels(),
        );
    } else {
        render(front, fs, state, now);
    }
}

fn render(canvas: &mut Canvas<'_>, fs: &dyn FileSystem, state: &mut PlayerState, now: u64) {
    state.set_screen(Rect::new(
        0,
        0,
        canvas.width() as u32,
        canvas.height() as u32,
    ));
    let mut painter = Painter::new(canvas);
    let layout = Layout::compute(state, now);
    painter.fill_rect(layout.screen, BG);
    draw_video(&mut painter, state, layout, now);
    if !state.fullscreen {
        if !state.embedded {
            draw_title_bar(&mut painter, state, layout);
        }
        draw_menu_bar(&mut painter, state, layout);
        draw_toolbar(&mut painter, state, layout);
        draw_controls(&mut painter, state, layout, now);
        draw_status_bar(&mut painter, state, layout);
    } else if layout.controls_visible(state, now) {
        let overlay = Rect::new(
            layout.screen.x,
            layout.screen.bottom_i32() - 82,
            layout.screen.width,
            82,
        );
        painter.fill_rect_alpha(overlay, Color::BLACK, 168);
        draw_controls(&mut painter, state, layout, now);
    }
    if let Some(playlist) = layout.playlist {
        draw_playlist(&mut painter, fs, state, playlist);
    }
    if let Some(menu) = state.menu_open {
        draw_menu(&mut painter, state, menu);
    }
    if let Some(context) = state.context {
        draw_context(&mut painter, state, context);
    }
    if state.dialog != Dialog::None {
        draw_dialog(&mut painter, fs, state, now);
    }
    if state.has_pointer && state.show_cursor {
        draw_cursor(&mut painter, state.pointer.x, state.pointer.y);
    }
}

fn draw_title_bar(painter: &mut Painter<'_, '_>, state: &PlayerState, layout: Layout) {
    painter
        .canvas_mut()
        .fill_vertical_gradient(layout.title, BAR_TOP, BAR_BOTTOM);
    painter.fill_rect(
        Rect::new(
            layout.title.x,
            layout.title.bottom_i32() - 2,
            layout.title.width,
            2,
        ),
        ORANGE,
    );
    draw_cone(
        painter,
        Rect::new(layout.title.x + 8, layout.title.y + 6, 18, 18),
    );
    let mut title = Text::<120>::new();
    if state.current.is_some() {
        let _ = write!(title, "{} - VLC media player", state.media_name.as_str());
    } else {
        title.set("VLC media player - feos");
    }
    painter.draw_text_ellipsis(
        Rect::new(
            layout.title.x + 34,
            layout.title.y,
            layout.title.width.saturating_sub(110),
            layout.title.height,
        ),
        title.as_str(),
        TEXT,
        2,
        Align::Start,
        VerticalAlign::Middle,
    );
    for index in 0..3 {
        draw_title_button(
            painter,
            title_button_rect(layout, index),
            index,
            state.pointer,
        );
    }
}

fn draw_title_button(painter: &mut Painter<'_, '_>, rect: Rect, index: usize, pointer: Point) {
    let hovered = rect.contains(pointer);
    let fill = if hovered {
        if index == 2 {
            Color::rgb(196, 58, 42)
        } else {
            HOVER
        }
    } else {
        PANEL_LIGHT
    };
    painter.fill_rounded_rect(rect, 3, fill);
    painter.stroke_rounded_rect(rect, 3, 1, BORDER);
    let cx = rect.x + rect.width as i32 / 2;
    let cy = rect.y + rect.height as i32 / 2;
    match index {
        0 => painter.fill_rect(Rect::new(cx - 4, cy + 2, 8, 1), TEXT),
        1 => {
            painter.stroke_rect(Rect::new(cx - 4, cy - 4, 8, 7), 1, TEXT);
            painter.fill_rect(Rect::new(cx - 4, cy - 4, 8, 1), TEXT);
        }
        _ => {
            painter.line(cx - 4, cy - 4, cx + 4, cy + 4, TEXT);
            painter.line(cx + 4, cy - 4, cx - 4, cy + 4, TEXT);
        }
    }
}

fn draw_menu_bar(painter: &mut Painter<'_, '_>, state: &mut PlayerState, layout: Layout) {
    painter.fill_rect(layout.menu_bar, BG);
    painter.fill_rect(
        Rect::new(
            layout.menu_bar.x,
            layout.menu_bar.bottom_i32() - 1,
            layout.menu_bar.width,
            1,
        ),
        BORDER,
    );
    for kind in MenuKind::ALL {
        let rect = menu_label_rect(layout, kind);
        let active = state.menu_open == Some(kind);
        let hovered = rect.contains(state.pointer) && state.has_pointer;
        if active {
            painter.fill_rect(rect.inset_xy(2, 2), ORANGE);
        } else if hovered {
            painter.fill_rect(rect.inset_xy(2, 2), HOVER);
        }
        painter.draw_text_centered(
            rect,
            MENU_LABELS[kind.index()],
            if active { Color::BLACK } else { TEXT },
            1,
        );
    }
}

fn draw_toolbar(painter: &mut Painter<'_, '_>, state: &PlayerState, layout: Layout) {
    painter
        .canvas_mut()
        .fill_vertical_gradient(layout.toolbar, BAR_TOP, BAR_BOTTOM);
    painter.fill_rect(
        Rect::new(
            layout.toolbar.x,
            layout.toolbar.bottom_i32() - 1,
            layout.toolbar.width,
            1,
        ),
        BORDER,
    );
    let icons = [
        Icon::Folder,
        Icon::ArrowLeft,
        if state.playing {
            Icon::Pause
        } else {
            Icon::Play
        },
        Icon::Close,
        Icon::ArrowRight,
        Icon::ExternalLink,
        Icon::List,
        Icon::Refresh,
        Icon::Sort,
        Icon::Info,
    ];
    let labels = [
        "Open", "Prev", "Play", "Stop", "Next", "Full", "List", "Loop", "Shuf", "Info",
    ];
    let enabled = [
        true,
        state.playlist_len > 1,
        state.current.is_some(),
        state.current.is_some(),
        state.playlist_len > 1,
        true,
        true,
        state.playlist_len > 0,
        state.playlist_len > 0,
        state.current.is_some(),
    ];
    for index in 0..TOOLBAR_WIDTHS.len() {
        let rect = toolbar_button_rect(layout, index);
        let hovered = rect.contains(state.pointer) && state.has_pointer && enabled[index];
        let pressed = (index == 2 && state.playing)
            || (index == 5 && state.fullscreen)
            || (index == 6 && state.playlist_open)
            || (index == 7 && state.loop_mode != LoopMode::Off)
            || (index == 8 && state.shuffle);
        if index == 2 {
            painter.fill_rounded_rect(
                rect,
                4,
                if pressed {
                    ORANGE_DARK
                } else if hovered {
                    ORANGE
                } else {
                    ORANGE
                },
            );
            painter.stroke_rounded_rect(rect, 4, 1, ORANGE_LIGHT);
        } else if pressed {
            painter.fill_rounded_rect(rect, 3, HOVER);
            painter.stroke_rounded_rect(rect, 3, 1, ORANGE);
        } else if hovered {
            painter.fill_rounded_rect(rect, 3, HOVER);
        }
        let color = if !enabled[index] {
            DISABLED
        } else if index == 2 {
            Color::BLACK
        } else if pressed {
            ORANGE
        } else {
            TEXT
        };
        paint_icon(
            painter,
            Rect::new(rect.x + (rect.width as i32 - 16) / 2, rect.y + 4, 16, 16),
            icons[index],
            color,
        );
        painter.draw_text_ellipsis(
            Rect::new(rect.x, rect.y + 23, rect.width, 12),
            labels[index],
            color,
            1,
            Align::Center,
            VerticalAlign::Middle,
        );
    }
}

fn draw_video(painter: &mut Painter<'_, '_>, state: &mut PlayerState, layout: Layout, now: u64) {
    if layout.video.is_empty() {
        return;
    }
    painter.fill_rect(layout.video, VIDEO_BG);
    if state.current.is_none() {
        draw_no_media(painter, layout.video);
        return;
    }
    if !state.rgba.is_empty() && state.frame_width > 0 && state.frame_height > 0 {
        let (dest, sx, sy, sw, sh) = video_destination(
            layout.video,
            state.frame_width,
            state.frame_height,
            state.aspect,
        );
        blit_rgba(
            painter.canvas_mut(),
            dest,
            state.frame_width,
            state.frame_height,
            &state.rgba,
            sx,
            sy,
            sw,
            sh,
        );
    }
    if !state.error.is_empty() {
        let rect = Rect::new(
            layout.video.x + 24,
            layout.video.y + 24,
            layout.video.width.saturating_sub(48),
            44,
        );
        painter.fill_rect_alpha(rect, Color::BLACK, 190);
        painter.draw_text_ellipsis(
            rect.inset_xy(10, 0),
            state.error.as_str(),
            ERROR,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    if state.osd_until > now && !state.osd.is_empty() {
        let width = (painter.text_width(state.osd.as_str(), 2) + 28) as u32;
        let rect = Rect::new(
            layout.video.x + 24,
            layout.video.y + 24,
            width.min(layout.video.width.saturating_sub(48)),
            36,
        );
        painter.fill_rect_alpha(rect, Color::BLACK, 175);
        painter.draw_text_ellipsis(
            rect.inset_xy(10, 0),
            state.osd.as_str(),
            TEXT,
            2,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    if !state.playing && !state.ended && state.error.is_empty() {
        let size = 54;
        let rect = Rect::new(
            layout.video.x + (layout.video.width as i32 - size) / 2,
            layout.video.y + (layout.video.height as i32 - size) / 2,
            size as u32,
            size as u32,
        );
        painter.fill_rect_alpha(rect, Color::BLACK, 110);
        paint_icon(
            painter,
            Rect::new(rect.x + 14, rect.y + 14, 26, 26),
            Icon::Play,
            ORANGE,
        );
    }
    if state.fullscreen {
        let name = if state.media_name.is_empty() {
            "No media"
        } else {
            state.media_name.as_str()
        };
        painter.draw_text_ellipsis(
            Rect::new(
                layout.video.x + 14,
                layout.video.y + 8,
                layout.video.width.saturating_sub(28),
                18,
            ),
            name,
            Color::rgb(200, 200, 200),
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
}

fn draw_no_media(painter: &mut Painter<'_, '_>, rect: Rect) {
    let cone = Rect::new(
        rect.x + (rect.width as i32 - 96) / 2,
        rect.y + rect.height as i32 / 2 - 90,
        96,
        96,
    );
    draw_cone(painter, cone);
    painter.draw_text_centered(
        Rect::new(rect.x, cone.bottom_i32() + 20, rect.width, 24),
        "No media loaded",
        TEXT,
        2,
    );
    painter.draw_text_centered(
        Rect::new(rect.x, cone.bottom_i32() + 50, rect.width, 18),
        "Press O to open a video, or double-click a playlist item",
        MUTED,
        1,
    );
}

fn draw_controls(painter: &mut Painter<'_, '_>, state: &PlayerState, layout: Layout, now: u64) {
    if layout.controls.is_empty() || !state.controls_visible(now) {
        return;
    }
    if !state.fullscreen {
        painter
            .canvas_mut()
            .fill_vertical_gradient(layout.controls, BAR_TOP, BAR_BOTTOM);
        painter.fill_rect(
            Rect::new(
                layout.controls.x,
                layout.controls.y,
                layout.controls.width,
                1,
            ),
            BORDER,
        );
    }
    let seek = seek_rect(layout);
    painter.fill_rect(
        Rect::new(seek.x, seek.y + 4, seek.width, 4),
        Color::rgb(74, 74, 74),
    );
    let progress = if state.duration_ms > 0 {
        (state.position_ms.min(state.duration_ms) as f64 / state.duration_ms as f64) as f32
    } else {
        0.0
    };
    let filled = (seek.width as f32 * progress.clamp(0.0, 1.0)) as u32;
    painter.fill_rect(Rect::new(seek.x, seek.y + 4, filled, 4), ORANGE);
    if state.duration_ms > 0 {
        let knob_x = seek.x + filled as i32 - 4;
        painter.fill_circle(knob_x, seek.y + 5, 5, ORANGE_LIGHT);
    }

    let mut elapsed = Text::<24>::new();
    format_time(state.position_ms, &mut elapsed);
    let mut duration = Text::<24>::new();
    if state.duration_ms > 0 {
        format_time(state.duration_ms, &mut duration);
    } else {
        duration.set("--:--");
    }
    let mut times = Text::<64>::new();
    let _ = write!(times, "{} / {}", elapsed.as_str(), duration.as_str());
    if !state.fullscreen {
        painter.draw_text_ellipsis(
            Rect::new(seek.x, seek.bottom_i32() + 3, 160, 16),
            times.as_str(),
            MUTED,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }

    let mut speed = Text::<32>::new();
    let _ = write!(speed, "{:.2}x", state.speed);
    painter.draw_text_ellipsis(
        Rect::new(seek.right_i32() - 70, seek.bottom_i32() + 3, 70, 16),
        speed.as_str(),
        MUTED,
        1,
        Align::End,
        VerticalAlign::Middle,
    );

    if state.fullscreen {
        let y = seek.bottom_i32() + 2;
        let play = Rect::new(layout.controls.x + 18, y, 30, 22);
        paint_icon(
            painter,
            Rect::new(play.x + 6, play.y + 3, 18, 18),
            if state.playing {
                Icon::Pause
            } else {
                Icon::Play
            },
            ORANGE,
        );
        let prev = Rect::new(play.right_i32() + 6, y, 24, 22);
        paint_icon(
            painter,
            Rect::new(prev.x + 4, prev.y + 4, 16, 16),
            Icon::ArrowLeft,
            TEXT,
        );
        let next = Rect::new(prev.right_i32() + 2, y, 24, 22);
        paint_icon(
            painter,
            Rect::new(next.x + 4, next.y + 4, 16, 16),
            Icon::ArrowRight,
            TEXT,
        );
        painter.draw_text_ellipsis(
            Rect::new(next.right_i32() + 12, y, 170, 22),
            times.as_str(),
            MUTED,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        let state_label = if state.playing {
            "PLAYING"
        } else if state.ended {
            "END"
        } else {
            "PAUSED"
        };
        painter.draw_text_ellipsis(
            Rect::new(next.right_i32() + 186, y, 100, 22),
            state_label,
            if state.playing { ORANGE } else { MUTED },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
}

fn draw_status_bar(painter: &mut Painter<'_, '_>, state: &PlayerState, layout: Layout) {
    painter.fill_rect(layout.status, PANEL);
    painter.fill_rect(
        Rect::new(layout.status.x, layout.status.y, layout.status.width, 1),
        BORDER,
    );
    let mut status = Text::<160>::new();
    if let Some(info) = state.info {
        let mut fps = format_fps(&info);
        if fps.is_empty() {
            let _ = write!(
                fps,
                "{:.1} fps (default)",
                state.fps_num as f64 / state.fps_den.max(1) as f64
            );
        }
        let _ = write!(
            status,
            "{}  |  {}  |  {}x{}  |  {} frame(s)  |  {}",
            container_name(info.format),
            info.codec.name(),
            info.width,
            info.height,
            info.frame_count,
            fps.as_str()
        );
    } else {
        status.set("No media");
    }
    painter.draw_text_ellipsis(
        Rect::new(
            layout.status.x + 10,
            layout.status.y,
            layout.status.width.saturating_sub(180),
            layout.status.height,
        ),
        status.as_str(),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let mut right = Text::<64>::new();
    if state.shuffle {
        right.push_str("shuffle  ");
    }
    right.push_str(state.loop_mode.label());
    painter.draw_text_ellipsis(
        Rect::new(
            layout.status.right_i32() - 170,
            layout.status.y,
            160,
            layout.status.height,
        ),
        right.as_str(),
        MUTED,
        1,
        Align::End,
        VerticalAlign::Middle,
    );
}

fn draw_playlist(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut PlayerState,
    rect: Rect,
) {
    painter.fill_rect(rect, PANEL);
    painter.fill_rect(Rect::new(rect.x, rect.y, 1, rect.height), BORDER);
    let header = Rect::new(rect.x + 1, rect.y, rect.width.saturating_sub(1), 30);
    painter.canvas_mut().fill_vertical_gradient(
        header,
        Color::rgb(70, 70, 70),
        Color::rgb(42, 42, 42),
    );
    painter.draw_text_ellipsis(
        Rect::new(
            header.x + 10,
            header.y,
            header.width.saturating_sub(80),
            header.height,
        ),
        "Playlist",
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let mut count = Text::<24>::new();
    let _ = write!(count, "{}", state.playlist_len);
    painter.draw_text_ellipsis(
        Rect::new(header.right_i32() - 50, header.y, 40, header.height),
        count.as_str(),
        MUTED,
        1,
        Align::End,
        VerticalAlign::Middle,
    );
    let list = playlist_list_rect(rect);
    let rows = playlist_visible_rows(rect);
    let max = state.playlist_len.saturating_sub(rows);
    if state.playlist_scroll > max {
        state.playlist_scroll = max;
    }
    let clip = painter.push_clip(list);
    for row in 0..rows {
        let index = state.playlist_scroll + row;
        if index >= state.playlist_len {
            break;
        }
        let node = state.playlist[index];
        let y = list.y + row as i32 * 32;
        let row_rect = Rect::new(list.x, y, list.width, 32);
        let current = state.current == Some(node);
        let selected = index == state.playlist_cursor && state.focus == Focus::Playlist;
        let hovered = row_rect.contains(state.pointer) && state.has_pointer;
        if selected {
            painter.fill_rect(row_rect, ORANGE_DARK);
        } else if current {
            painter.fill_rect(row_rect, Color::rgb(60, 44, 24));
        } else if hovered {
            painter.fill_rect(row_rect, HOVER);
        }
        if current {
            painter.fill_rect(
                Rect::new(row_rect.x, row_rect.y, 3, row_rect.height),
                ORANGE,
            );
        }
        let mut number = Text::<16>::new();
        let _ = write!(number, "{:02}", index + 1);
        painter.draw_text_ellipsis(
            Rect::new(row_rect.x + 8, row_rect.y, 24, row_rect.height),
            number.as_str(),
            if selected { TEXT } else { MUTED },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        painter.draw_text_ellipsis(
            Rect::new(
                row_rect.x + 34,
                row_rect.y + 3,
                row_rect.width.saturating_sub(42),
                14,
            ),
            fs.name(node).unwrap_or("?"),
            TEXT,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        let mut extension = Text::<16>::new();
        upper_extension(fs.name(node).unwrap_or(""), &mut extension);
        let mut detail = Text::<64>::new();
        let _ = write!(
            detail,
            "{}  {}",
            extension.as_str(),
            format_size(fs.size(node)).as_str()
        );
        painter.draw_text_ellipsis(
            Rect::new(
                row_rect.x + 34,
                row_rect.y + 16,
                row_rect.width.saturating_sub(42),
                13,
            ),
            detail.as_str(),
            MUTED,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    painter.restore_clip(clip);
    draw_scrollbar(
        painter,
        list,
        state.playlist_scroll,
        rows,
        state.playlist_len,
    );
    let footer = Rect::new(
        rect.x + 1,
        rect.bottom_i32() - 21,
        rect.width.saturating_sub(1),
        20,
    );
    painter.fill_rect(footer, Color::rgb(26, 26, 26));
    let mut hint = Text::<48>::new();
    let _ = write!(hint, "{} item(s)", state.playlist_len);
    painter.draw_text_ellipsis(
        Rect::new(
            footer.x + 8,
            footer.y,
            footer.width.saturating_sub(12),
            footer.height,
        ),
        hint.as_str(),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
}

fn draw_menu(painter: &mut Painter<'_, '_>, state: &mut PlayerState, kind: MenuKind) {
    if state.has_pointer {
        if let Some(index) = menu_item_at(state, kind, state.pointer) {
            let entry = menu_items(kind)[index];
            if !entry.separator && entry.enabled && state.action_enabled(entry.action) {
                state.menu_hover = index;
            }
        }
    }
    let rect = menu_rect(state, kind);
    painter.shadow(rect, 3, 7, 90);
    painter.fill_rect(rect, Color::rgb(38, 38, 38));
    painter.stroke_rect(rect, 1, ORANGE_DARK);
    let mut y = rect.y + 3;
    for (index, entry) in menu_items(kind).iter().enumerate() {
        if entry.separator {
            painter.fill_rect(
                Rect::new(rect.x + 24, y + 3, rect.width.saturating_sub(30), 1),
                BORDER,
            );
            y += 7;
            continue;
        }
        let enabled = entry.enabled && state.action_enabled(entry.action);
        let hovered = index == state.menu_hover && enabled;
        let item_rect = Rect::new(rect.x + 2, y, rect.width.saturating_sub(4), 22);
        if hovered {
            painter.fill_rect(item_rect, ORANGE);
        }
        if state.action_checked(entry.action) {
            paint_icon(
                painter,
                Rect::new(item_rect.x + 5, item_rect.y + 6, 10, 10),
                Icon::Check,
                if hovered { Color::BLACK } else { ORANGE },
            );
        }
        painter.draw_text_ellipsis(
            Rect::new(
                item_rect.x + 22,
                item_rect.y,
                item_rect.width.saturating_sub(92),
                item_rect.height,
            ),
            entry.label,
            if !enabled {
                DISABLED
            } else if hovered {
                Color::BLACK
            } else {
                TEXT
            },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if !entry.shortcut.is_empty() {
            painter.draw_text_ellipsis(
                Rect::new(
                    item_rect.right_i32() - 82,
                    item_rect.y,
                    76,
                    item_rect.height,
                ),
                entry.shortcut,
                if !enabled {
                    DISABLED
                } else if hovered {
                    Color::BLACK
                } else {
                    MUTED
                },
                1,
                Align::End,
                VerticalAlign::Middle,
            );
        }
        y += 22;
    }
}

fn draw_context(painter: &mut Painter<'_, '_>, state: &mut PlayerState, context: ContextMenu) {
    if state.has_pointer {
        if let Some(index) = context_item_at(state, context, state.pointer) {
            let entries = context_entries(context.kind);
            let entry = entries[index];
            if !entry.separator && entry.enabled {
                state.context = Some(ContextMenu {
                    hover: index,
                    ..context
                });
            }
        }
    }
    let context = state.context.unwrap_or(context);
    let rect = context_rect(state, context);
    painter.shadow(rect, 3, 7, 90);
    painter.fill_rect(rect, Color::rgb(38, 38, 38));
    painter.stroke_rect(rect, 1, ORANGE_DARK);
    let mut y = rect.y + 3;
    for (index, entry) in context_entries(context.kind).iter().enumerate() {
        if entry.separator {
            painter.fill_rect(
                Rect::new(rect.x + 22, y + 3, rect.width.saturating_sub(28), 1),
                BORDER,
            );
            y += 7;
            continue;
        }
        let hovered = index == context.hover;
        let item_rect = Rect::new(rect.x + 2, y, rect.width.saturating_sub(4), 22);
        if hovered {
            painter.fill_rect(item_rect, ORANGE);
        }
        painter.draw_text_ellipsis(
            Rect::new(
                item_rect.x + 22,
                item_rect.y,
                item_rect.width.saturating_sub(92),
                item_rect.height,
            ),
            entry.label,
            if !entry.enabled {
                DISABLED
            } else if hovered {
                Color::BLACK
            } else {
                TEXT
            },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if !entry.shortcut.is_empty() {
            painter.draw_text_ellipsis(
                Rect::new(
                    item_rect.right_i32() - 82,
                    item_rect.y,
                    76,
                    item_rect.height,
                ),
                entry.shortcut,
                if !entry.enabled {
                    DISABLED
                } else if hovered {
                    Color::BLACK
                } else {
                    MUTED
                },
                1,
                Align::End,
                VerticalAlign::Middle,
            );
        }
        y += 22;
    }
}

fn draw_dialog(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut PlayerState,
    now: u64,
) {
    let dialog = state.dialog;
    if dialog == Dialog::None {
        return;
    }
    painter.fill_rect_alpha(state.screen, Color::BLACK, 120);
    let layout = DialogLayout::compute(state.screen, dialog);
    painter.shadow(layout.window, 7, 12, 130);
    painter.fill_rect(layout.window, BG);
    painter.stroke_rect(layout.window, 1, ORANGE_DARK);
    painter.canvas_mut().fill_vertical_gradient(
        layout.title,
        Color::rgb(68, 68, 68),
        Color::rgb(36, 36, 36),
    );
    painter.fill_rect(
        Rect::new(
            layout.title.x,
            layout.title.bottom_i32() - 2,
            layout.title.width,
            2,
        ),
        ORANGE,
    );
    let mut title = Text::<80>::new();
    match dialog {
        Dialog::Open => {
            title.set("Open Media");
        }
        Dialog::Info => {
            title.set("Media Information");
        }
        Dialog::About => {
            title.set("About VLC media player");
        }
        Dialog::Help => {
            title.set("Keyboard Shortcuts");
        }
        Dialog::Notice => {
            title.set(state.notice_title.as_str());
        }
        Dialog::None => {}
    }
    painter.draw_text_ellipsis(
        Rect::new(
            layout.title.x + 10,
            layout.title.y,
            layout.title.width.saturating_sub(40),
            layout.title.height,
        ),
        title.as_str(),
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    draw_close_x(painter, layout.close, layout.close.contains(state.pointer));
    match dialog {
        Dialog::Open => draw_open_dialog(painter, fs, state, layout),
        Dialog::Info => draw_info_dialog(painter, state, layout),
        Dialog::About => draw_about_dialog(painter, layout),
        Dialog::Help => draw_help_dialog(painter, layout),
        Dialog::Notice => draw_notice_dialog(painter, state, layout),
        Dialog::None => {}
    }
    if dialog == Dialog::Open {
        let cancel_hovered = layout.cancel.contains(state.pointer);
        draw_button(painter, layout.cancel, "Cancel", cancel_hovered, false);
    }
    let label = if dialog == Dialog::Open { "Open" } else { "OK" };
    draw_button(
        painter,
        layout.ok,
        label,
        layout.ok.contains(state.pointer),
        true,
    );
    let _ = now;
}

fn draw_open_dialog(
    painter: &mut Painter<'_, '_>,
    fs: &dyn FileSystem,
    state: &mut PlayerState,
    layout: DialogLayout,
) {
    painter.draw_text_ellipsis(
        Rect::new(layout.body.x, layout.body.y, layout.body.width, 18),
        "Choose a video file from the filesystem:",
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let list = open_list_rect(layout);
    painter.fill_rect(list, Color::rgb(26, 26, 26));
    painter.stroke_rect(list, 1, BORDER);
    let rows = open_visible_rows(layout);
    let max = state.open_len.saturating_sub(rows);
    if state.open_scroll > max {
        state.open_scroll = max;
    }
    let clip = painter.push_clip(list.inset(1));
    for row in 0..rows {
        let index = state.open_scroll + row;
        if index >= state.open_len {
            break;
        }
        let node = state.open_results[index];
        let y = list.y + 1 + row as i32 * 24;
        let row_rect = Rect::new(list.x + 1, y, list.width.saturating_sub(2), 24);
        let selected = index == state.open_cursor;
        if selected {
            painter.fill_rect(row_rect, ORANGE_DARK);
        } else if row_rect.contains(state.pointer) && state.has_pointer {
            painter.fill_rect(row_rect, HOVER);
        }
        let mut path = Text::<MAX_PATH>::new();
        let mut buffer = [0u8; MAX_PATH];
        path.set(fs.path(node, &mut buffer).unwrap_or("/"));
        painter.draw_text_ellipsis(
            Rect::new(
                row_rect.x + 8,
                row_rect.y,
                row_rect.width.saturating_sub(16),
                14,
            ),
            path.as_str(),
            TEXT,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        let mut extension = Text::<16>::new();
        upper_extension(fs.name(node).unwrap_or(""), &mut extension);
        let mut detail = Text::<48>::new();
        let _ = write!(
            detail,
            "{}   {}",
            extension.as_str(),
            format_size(fs.size(node)).as_str()
        );
        painter.draw_text_ellipsis(
            Rect::new(
                row_rect.x + 8,
                row_rect.y + 12,
                row_rect.width.saturating_sub(16),
                11,
            ),
            detail.as_str(),
            MUTED,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
    painter.restore_clip(clip);
    draw_scrollbar(painter, list, state.open_scroll, rows, state.open_len);
}

fn draw_info_dialog(painter: &mut Painter<'_, '_>, state: &PlayerState, layout: DialogLayout) {
    let body = layout.body;
    draw_cone(painter, Rect::new(body.x + 8, body.y + 4, 42, 42));
    painter.draw_text_ellipsis(
        Rect::new(body.x + 62, body.y + 2, body.width.saturating_sub(68), 24),
        state.media_name.as_str(),
        TEXT,
        2,
        Align::Start,
        VerticalAlign::Middle,
    );
    painter.draw_text_ellipsis(
        Rect::new(body.x + 62, body.y + 27, body.width.saturating_sub(68), 16),
        state.media_path.as_str(),
        MUTED,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let mut y = body.y + 58;
    if let Some(info) = state.info {
        property(
            painter,
            body.x + 4,
            y,
            "Format",
            container_name(info.format),
        );
        y += 20;
        property(painter, body.x + 4, y, "Codec", info.codec.name());
        y += 20;
        let mut dimensions = Text::<48>::new();
        let _ = write!(dimensions, "{} x {}", info.width, info.height);
        property(painter, body.x + 4, y, "Dimensions", dimensions.as_str());
        y += 20;
        let mut frames = Text::<48>::new();
        let _ = write!(frames, "{}", info.frame_count);
        property(painter, body.x + 4, y, "Frames", frames.as_str());
        y += 20;
        let mut fps = format_fps(&info);
        if fps.is_empty() {
            let _ = write!(
                fps,
                "{:.1} fps (default)",
                state.fps_num as f64 / state.fps_den.max(1) as f64
            );
        }
        property(painter, body.x + 4, y, "Frame rate", fps.as_str());
        y += 20;
        let mut duration = Text::<24>::new();
        if state.duration_ms > 0 {
            format_time(state.duration_ms, &mut duration);
        } else {
            duration.set("unknown");
        }
        property(painter, body.x + 4, y, "Duration", duration.as_str());
        y += 20;
        property(
            painter,
            body.x + 4,
            y,
            "Decode",
            if info.can_decode() {
                "supported"
            } else {
                "unsupported"
            },
        );
        y += 20;
    } else {
        property(painter, body.x + 4, y, "Format", "unknown");
        y += 20;
        property(painter, body.x + 4, y, "Codec", "unknown");
        y += 20;
    }
    let mut size = Text::<48>::new();
    let _ = write!(size, "{}", format_size(state.media_size).as_str());
    property(painter, body.x + 4, y, "File size", size.as_str());
    y += 20;
    property(painter, body.x + 4, y, "Aspect", state.aspect.label());
    y += 26;
    painter.fill_rect(
        Rect::new(body.x + 4, y, body.width.saturating_sub(8), 1),
        BORDER,
    );
    y += 8;
    if !state.error.is_empty() {
        painter.draw_text(body.x + 4, y, state.error.as_str(), ERROR, 1);
    } else {
        painter.draw_text(
            body.x + 4,
            y,
            "Playback is decoded directly into the framebuffer.",
            MUTED,
            1,
        );
    }
}

fn property(painter: &mut Painter<'_, '_>, x: i32, y: i32, label: &str, value: &str) {
    painter.draw_text(x, y, label, MUTED, 1);
    painter.draw_text_ellipsis(
        Rect::new(x + 100, y, 360, 14),
        value,
        TEXT,
        1,
        Align::Start,
        VerticalAlign::Top,
    );
}

fn draw_about_dialog(painter: &mut Painter<'_, '_>, layout: DialogLayout) {
    let body = layout.body;
    draw_cone(painter, Rect::new(body.x + 10, body.y + 8, 62, 62));
    painter.draw_text(body.x + 90, body.y + 8, "VLC media player", ORANGE, 2);
    painter.draw_text(body.x + 90, body.y + 32, "for feos", TEXT, 1);
    painter.draw_text(
        body.x + 8,
        body.y + 82,
        "A video player with a playlist, transport controls, seek bar,",
        TEXT,
        1,
    );
    painter.draw_text(
        body.x + 8,
        body.y + 98,
        "fullscreen mode, aspect control and media information.",
        TEXT,
        1,
    );
    painter.draw_text(
        body.x + 8,
        body.y + 126,
        "Formats: GIF, AVI, MP4/MOV, Matroska/WebM, FLV, IVF, H.264",
        MUTED,
        1,
    );
    painter.draw_text(
        body.x + 8,
        body.y + 142,
        "Decoders: GIF, MJPEG, raw/RLE video, H.264",
        MUTED,
        1,
    );
    painter.draw_text(
        body.x + 8,
        body.y + 174,
        "feos 0.1.0  |  no network, no ads, no telemetry",
        MUTED,
        1,
    );
}

fn draw_help_dialog(painter: &mut Painter<'_, '_>, layout: DialogLayout) {
    let lines = [
        ("Space / Enter", "Play or pause"),
        ("S", "Stop"),
        ("B / N", "Previous / next item"),
        ("Left / Right", "Seek 5 seconds"),
        ("Shift+Left/Right", "Seek 1 second"),
        ("Page Up / Down", "Seek 30 seconds"),
        ("Home / End", "Start / end"),
        (", / .", "Previous / next frame"),
        ("+ / - / 0", "Faster / slower / normal speed"),
        ("F or F11", "Toggle fullscreen"),
        ("P", "Toggle playlist"),
        ("O", "Open a media file"),
        ("I", "Media information"),
        ("L / R", "Cycle loop / toggle shuffle"),
        ("A", "Cycle aspect mode"),
        ("Tab", "Switch video / playlist focus"),
        ("Mouse wheel", "Seek or scroll the playlist"),
        ("Esc", "Close dialog / leave fullscreen / quit"),
    ];
    let mut y = layout.body.y;
    for (keys, action) in lines {
        painter.draw_text(layout.body.x, y, keys, ORANGE, 1);
        painter.draw_text(layout.body.x + 178, y, action, TEXT, 1);
        y += 17;
    }
}

fn draw_notice_dialog(painter: &mut Painter<'_, '_>, state: &PlayerState, layout: DialogLayout) {
    let body = layout.body;
    paint_icon(
        painter,
        Rect::new(body.x + 8, body.y + 8, 28, 28),
        Icon::Info,
        ORANGE,
    );
    painter.draw_text(
        body.x + 48,
        body.y + 8,
        state.notice_title.as_str(),
        TEXT,
        2,
    );
    painter.draw_text_wrapped(
        Rect::new(
            body.x + 48,
            body.y + 38,
            body.width.saturating_sub(54),
            body.height.saturating_sub(40),
        ),
        state.notice_body.as_str(),
        MUTED,
        1,
    );
}

fn draw_button(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    label: &str,
    hovered: bool,
    primary: bool,
) {
    let fill = if primary {
        if hovered {
            ORANGE_LIGHT
        } else {
            ORANGE
        }
    } else if hovered {
        HOVER
    } else {
        PANEL_LIGHT
    };
    painter.fill_rounded_rect(rect, 4, fill);
    painter.stroke_rounded_rect(rect, 4, 1, if primary { ORANGE_LIGHT } else { BORDER });
    painter.draw_text_centered(rect, label, if primary { Color::BLACK } else { TEXT }, 1);
}

fn draw_close_x(painter: &mut Painter<'_, '_>, rect: Rect, hovered: bool) {
    painter.fill_rounded_rect(
        rect,
        3,
        if hovered {
            Color::rgb(210, 70, 50)
        } else {
            Color::rgb(170, 58, 42)
        },
    );
    let cx = rect.x + rect.width as i32 / 2;
    let cy = rect.y + rect.height as i32 / 2;
    painter.line(cx - 3, cy - 3, cx + 4, cy + 4, TEXT);
    painter.line(cx + 4, cy - 3, cx - 3, cy + 4, TEXT);
}

fn draw_scrollbar(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    offset: usize,
    visible: usize,
    total: usize,
) {
    if total <= visible || rect.width < 10 {
        return;
    }
    let track = Rect::new(
        rect.right_i32() - 9,
        rect.y + 2,
        7,
        rect.height.saturating_sub(4),
    );
    painter.fill_rect(track, Color::rgb(26, 26, 26));
    painter.stroke_rect(track, 1, BORDER);
    let height = (track.height as f32 * visible as f32 / total as f32).max(18.0) as u32;
    let max_offset = track.height.saturating_sub(height) as f32;
    let max_scroll = total.saturating_sub(visible).max(1) as f32;
    let progress = (offset as f32 / max_scroll).clamp(0.0, 1.0);
    painter.fill_rounded_rect(
        Rect::new(
            track.x + 1,
            track.y + (progress * max_offset) as i32,
            track.width.saturating_sub(2),
            height,
        ),
        2,
        ORANGE_DARK,
    );
}

fn container_name(format: ContainerFormat) -> &'static str {
    match format {
        ContainerFormat::Gif => "GIF",
        ContainerFormat::Avi => "AVI",
        ContainerFormat::Mp4 => "MP4",
        ContainerFormat::Matroska => "Matroska/WebM",
        ContainerFormat::Flv => "FLV",
        ContainerFormat::Ivf => "IVF",
        ContainerFormat::H264 => "H.264 Annex-B",
        ContainerFormat::MotionJpeg => "Motion JPEG",
    }
}

fn video_destination(
    area: Rect,
    frame_width: u32,
    frame_height: u32,
    aspect: Aspect,
) -> (Rect, u32, u32, u32, u32) {
    let fw = frame_width.max(1);
    let fh = frame_height.max(1);
    match aspect {
        Aspect::Stretch => (area, 0, 0, fw, fh),
        Aspect::Fit => {
            let scale = (area.width as f64 / fw as f64).min(area.height as f64 / fh as f64);
            let width = (fw as f64 * scale + 0.5).max(1.0) as u32;
            let height = (fh as f64 * scale + 0.5).max(1.0) as u32;
            let dest = Rect::new(
                area.x + (area.width as i32 - width as i32) / 2,
                area.y + (area.height as i32 - height as i32) / 2,
                width,
                height,
            );
            (dest, 0, 0, fw, fh)
        }
        Aspect::Fill => {
            let area_aspect = area.width as f64 / area.height.max(1) as f64;
            let source_aspect = fw as f64 / fh as f64;
            if source_aspect > area_aspect {
                let sw = (fh as f64 * area_aspect + 0.5).max(1.0) as u32;
                let sx = (fw - sw) / 2;
                (area, sx, 0, sw, fh)
            } else {
                let sh = (fw as f64 / area_aspect + 0.5).max(1.0) as u32;
                let sy = (fh - sh) / 2;
                (area, 0, sy, fw, sh)
            }
        }
        Aspect::Original => {
            let dest = Rect::new(
                area.x + (area.width as i32 - fw as i32) / 2,
                area.y + (area.height as i32 - fh as i32) / 2,
                fw,
                fh,
            );
            (dest, 0, 0, fw, fh)
        }
    }
}

fn blit_rgba<'a>(
    canvas: &mut Canvas<'a>,
    dest: Rect,
    frame_width: u32,
    frame_height: u32,
    rgba: &[u8],
    sx: u32,
    sy: u32,
    sw: u32,
    sh: u32,
) {
    if dest.is_empty() || sw == 0 || sh == 0 {
        return;
    }
    let canvas_width = canvas.width() as i32;
    let canvas_height = canvas.height() as i32;
    let stride = canvas.stride();
    let x0 = dest.x.max(0);
    let y0 = dest.y.max(0);
    let x1 = dest.right_i32().min(canvas_width);
    let y1 = dest.bottom_i32().min(canvas_height);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let pixels = canvas.pixels_mut();
    let dest_width = dest.width.max(1);
    let dest_height = dest.height.max(1);
    for py in y0..y1 {
        let dy = (py - dest.y) as u32;
        let source_y = sy
            + (dy as u64 * sh as u64 / dest_height as u64).min(sh.saturating_sub(1) as u64) as u32;
        let row = source_y * frame_width;
        for px in x0..x1 {
            let dx = (px - dest.x) as u32;
            let source_x = sx
                + (dx as u64 * sw as u64 / dest_width as u64).min(sw.saturating_sub(1) as u64)
                    as u32;
            let index = ((row + source_x) * 4) as usize;
            if index + 3 < rgba.len() {
                let r = rgba[index] as u32;
                let g = rgba[index + 1] as u32;
                let b = rgba[index + 2] as u32;
                pixels[py as usize * stride + px as usize] = (r << 16) | (g << 8) | b;
            }
        }
    }
    let _ = frame_height;
}

fn draw_cone(painter: &mut Painter<'_, '_>, rect: Rect) {
    if rect.width < 6 || rect.height < 6 {
        return;
    }
    let w = rect.width as i32;
    let h = rect.height as i32;
    let base = Rect::new(
        rect.x + w / 8,
        rect.bottom_i32() - h / 5,
        (w * 3 / 4) as u32,
        (h / 5) as u32,
    );
    painter.fill_rounded_rect(base, 2, ORANGE_DARK);
    let body = [
        Point::new(rect.x + w / 3, rect.y + h / 8),
        Point::new(rect.x + w * 2 / 3, rect.y + h / 8),
        Point::new(rect.x + w * 7 / 8, base.y),
        Point::new(rect.x + w / 8, base.y),
    ];
    painter.fill_polygon(&body, ORANGE);
    for band in 0..2 {
        let y = rect.y + h / 3 + band * (h / 6);
        let t = (y - (rect.y + h / 8)) as f32 / (base.y - (rect.y + h / 8)).max(1) as f32;
        let half = (w as f32 / 6.0) + (w as f32 / 3.0) * t;
        let center = rect.x + w / 2;
        painter.fill_rect(
            Rect::new(
                center - half as i32,
                y,
                (half * 2.0).max(1.0) as u32,
                (h / 16).max(1) as u32,
            ),
            Color::rgb(255, 244, 220),
        );
    }
    painter.fill_circle(
        rect.x + w / 2,
        rect.y + h / 8,
        (w / 12).max(1) as u32,
        ORANGE_LIGHT,
    );
}

fn draw_cursor(painter: &mut Painter<'_, '_>, x: i32, y: i32) {
    const CURSOR: [&str; 19] = [
        "X           ",
        "XX          ",
        "X.X         ",
        "X..X        ",
        "X...X       ",
        "X....X      ",
        "X.....X     ",
        "X......X    ",
        "X.......X   ",
        "X........X  ",
        "X.....XXXXX ",
        "X..X..X     ",
        "X.X X..X    ",
        "XX  X..X    ",
        "X    X..X   ",
        "     X..X   ",
        "      X..X  ",
        "      X..X  ",
        "       XX   ",
    ];
    for (row, line) in CURSOR.iter().enumerate() {
        for (column, pixel) in line.bytes().enumerate() {
            let color = match pixel {
                b'X' => Color::BLACK,
                b'.' => Color::WHITE,
                _ => continue,
            };
            painter.put_pixel(x + column as i32, y + row as i32, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_extensions_are_recognised() {
        assert!(is_media_file("clip.mp4"));
        assert!(is_media_file("CLIP.MKV"));
        assert!(is_media_file("animation.gif"));
        assert!(is_media_file("stream.h264"));
        assert!(!is_media_file("README.txt"));
        assert!(!is_media_file("song.wav"));
    }

    #[test]
    fn time_formatting_handles_minutes_and_hours() {
        let mut text = Text::<24>::new();
        format_time(0, &mut text);
        assert_eq!(text.as_str(), "00:00");
        format_time(400, &mut text);
        assert_eq!(text.as_str(), "00:00.4");
        format_time(65_000, &mut text);
        assert_eq!(text.as_str(), "01:05");
        format_time(3_723_000, &mut text);
        assert_eq!(text.as_str(), "1:02:03");
    }

    #[test]
    fn fit_preserves_aspect_ratio() {
        let (dest, _, _, sw, sh) =
            video_destination(Rect::new(0, 0, 400, 200), 32, 32, Aspect::Fit);
        assert_eq!((sw, sh), (32, 32));
        assert_eq!(dest.width, dest.height);
        assert_eq!(dest.height, 200);
    }
}
