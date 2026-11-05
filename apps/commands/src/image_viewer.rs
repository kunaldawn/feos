//! The image viewer window: shows one picture, fitted or zoomed, and steps through
//! the others in its folder.
//!
//! Decoding runs on a task of its own, because a big JPEG takes a while and a
//! damaged one shouldn't cost the desktop anything. The window takes the finished
//! picture out of a mailbox whose lock is only ever held to move it.

use core::fmt::Write;

use alloc::vec::Vec;

use feos_app::{App, Context, WindowContext, WindowFrame};
use feos_console::eprintln;
use feos_fs::{FileSystem, NodeId};
use feos_gfx::{Canvas, Color, Rect};
use feos_gui::{Align, Event, Key, Painter, VerticalAlign};
use feos_sync::{Mutex, SpinLock};
use feos_text::Text;

use crate::files::preview::{decode_fit, Decoded};

/// The largest picture kept, which fits a screen and spares the heap.
const BOX: (u32, u32) = (1920, 1200);
const STATUS_HEIGHT: i32 = 22;
const BACKGROUND: Color = Color::rgb(46, 48, 54);
const STATUS: Color = Color::rgb(30, 32, 36);
const TEXT: Color = Color::rgb(226, 228, 232);
const MIN_ZOOM: u32 = 25;
const MAX_ZOOM: u32 = 800;
const SLEEP_MS: u64 = 10;

/// Names the viewer can show.
pub(crate) fn is_image_file(name: &str) -> bool {
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    ["png", "jpg", "jpeg", "bmp", "ppm", "pgm", "pbm", "pnm", "qoi", "ico"]
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
}

/// What the window and its task share.
struct Mailbox {
    /// The task that may run. Any other leaves.
    owner: u32,
    /// Counts requests, so a late answer to an old one is dropped.
    serial: u32,
    request: Option<NodeId>,
    answer: Option<(u32, Option<Decoded>)>,
}

static MAILBOX: Mutex<Mailbox> = Mutex::new(Mailbox {
    owner: 0,
    serial: 0,
    request: None,
    answer: None,
});

/// The task that decodes. Its argument is its identity.
pub struct ImageTask;

impl App for ImageTask {
    fn name(&self) -> &'static str {
        "image-decoder"
    }

    fn usage(&self) -> &'static str {
        "(started by the image viewer)"
    }

    fn description(&self) -> &'static str {
        "decode pictures for the image viewer"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(fs) = context.platform.filesystem else {
            return 1;
        };
        let Ok(identity) = context.args.trim().parse::<u32>() else {
            return 2;
        };
        let processes = context.platform.processes;
        while !processes.kill_requested() {
            let request = {
                let mut mailbox = MAILBOX.lock();
                if mailbox.owner != identity {
                    return 0;
                }
                mailbox.request.take().map(|node| (node, mailbox.serial))
            };
            let Some((node, serial)) = request else {
                processes.sleep_ms(SLEEP_MS);
                continue;
            };
            let decoded = decode_fit(fs, node, BOX.0, BOX.1);
            let mut mailbox = MAILBOX.lock();
            if mailbox.owner != identity {
                return 0;
            }
            if mailbox.serial == serial {
                mailbox.answer = Some((serial, decoded));
            }
        }
        0
    }
}

/// The window.
struct ViewerState {
    /// The pictures in the folder of the open one, in folder order.
    siblings: Vec<NodeId>,
    current: Option<NodeId>,
    name: Text<96>,
    picture: Option<Decoded>,
    loading: bool,
    failed: bool,
    /// Percent; `None` fits the window.
    zoom: Option<u32>,
    serial: u32,
}

static WINDOW: SpinLock<Option<ViewerState>> = SpinLock::new(None);

impl ViewerState {
    fn open(&mut self, fs: &dyn FileSystem, node: NodeId) {
        self.siblings.clear();
        if let Some(parent) = fs.parent(node) {
            for index in 0..fs.child_count(parent) {
                if let Some(child) = fs.child_at(parent, index) {
                    if fs.is_file(child)
                        && fs.name(child).is_some_and(is_image_file)
                        && self.siblings.try_reserve(1).is_ok()
                    {
                        self.siblings.push(child);
                    }
                }
            }
        }
        self.show(fs, node);
    }

    fn show(&mut self, fs: &dyn FileSystem, node: NodeId) {
        self.current = Some(node);
        self.name.set(fs.name(node).unwrap_or("?"));
        self.picture = None;
        self.loading = true;
        self.failed = false;
        self.zoom = None;
        let mut mailbox = MAILBOX.lock();
        mailbox.serial = mailbox.serial.wrapping_add(1);
        mailbox.request = Some(node);
        mailbox.answer = None;
        self.serial = mailbox.serial;
    }

    fn step(&mut self, fs: &dyn FileSystem, delta: isize) {
        let Some(at) = self
            .current
            .and_then(|current| self.siblings.iter().position(|&node| node == current))
        else {
            return;
        };
        let count = self.siblings.len() as isize;
        let next = (at as isize + delta).rem_euclid(count) as usize;
        let node = self.siblings[next];
        self.show(fs, node);
    }

    fn receive(&mut self) {
        let answer = {
            let mut mailbox = MAILBOX.lock();
            mailbox.answer.take()
        };
        if let Some((serial, decoded)) = answer {
            if serial == self.serial {
                self.loading = false;
                self.failed = decoded.is_none();
                self.picture = decoded;
            }
        }
    }

    fn change_zoom(&mut self, up: bool, fitted: u32) {
        let now = self.zoom.unwrap_or(fitted);
        let next = if up { now.saturating_mul(5) / 4 + 1 } else { now.saturating_mul(4) / 5 };
        self.zoom = Some(next.clamp(MIN_ZOOM, MAX_ZOOM));
    }

    fn draw(&self, canvas: &mut Canvas<'_>) {
        let (width, height) = (canvas.width() as i32, canvas.height() as i32);
        let view_height = (height - STATUS_HEIGHT).max(0);
        let mut painter = Painter::new(canvas);
        painter.fill_rect(Rect::new(0, 0, width as u32, view_height as u32), BACKGROUND);
        let view = Rect::new(0, 0, width as u32, view_height as u32);
        let mut status = Text::<160>::new();
        match &self.picture {
            Some(picture) => {
                let percent = self.zoom.unwrap_or_else(|| fit_percent(picture, view));
                let shown_width = (picture.width * percent / 100).max(1) as i32;
                let shown_height = (picture.height * percent / 100).max(1) as i32;
                let left = (width - shown_width) / 2;
                let top = (view_height - shown_height) / 2;
                draw_scaled(&mut painter, picture, left, top, shown_width, shown_height, view);
                let _ = write!(
                    status,
                    "{}   {}x{}   {}%",
                    self.name.as_str(),
                    picture.width,
                    picture.height,
                    percent
                );
            }
            None if self.failed => {
                painter.draw_text_centered(view, "This file can't be shown as a picture.", TEXT, 1);
                let _ = write!(status, "{}", self.name.as_str());
            }
            None if self.loading => {
                painter.draw_text_centered(view, "Loading...", TEXT, 1);
                let _ = write!(status, "{}", self.name.as_str());
            }
            None => {
                painter.draw_text_centered(view, "No picture open", TEXT, 1);
            }
        }
        let bar = Rect::new(0, view_height, width as u32, STATUS_HEIGHT as u32);
        painter.fill_rect(bar, STATUS);
        painter.draw_text_ellipsis(
            Rect::new(8, view_height, (width - 16).max(0) as u32, STATUS_HEIGHT as u32),
            status.as_str(),
            TEXT,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
}

/// The zoom that fits a picture in a rectangle, without enlarging it past 100%.
fn fit_percent(picture: &Decoded, view: Rect) -> u32 {
    let across = view.width as u64 * 100 / picture.width.max(1) as u64;
    let down = view.height as u64 * 100 / picture.height.max(1) as u64;
    across.min(down).clamp(1, 100) as u32
}

fn draw_scaled(
    painter: &mut Painter<'_, '_>,
    picture: &Decoded,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    clip: Rect,
) {
    let y_start = (clip.y - top).max(0);
    let y_end = (clip.y + clip.height as i32 - top).min(height);
    let x_start = (clip.x - left).max(0);
    let x_end = (clip.x + clip.width as i32 - left).min(width);
    for y in y_start..y_end {
        let source_y = y as u64 * picture.height as u64 / height as u64;
        let row = source_y as usize * picture.width as usize;
        for x in x_start..x_end {
            let source_x = x as u64 * picture.width as u64 / width as u64;
            if let Some(&pixel) = picture.pixels.get(row + source_x as usize) {
                let color = Color::rgb((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8);
                painter.blend_pixel(left + x, top + y, color, 255);
            }
        }
    }
}

/// The Image Viewer application.
pub struct ImageViewer;

impl App for ImageViewer {
    fn name(&self) -> &'static str {
        "viewer"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["image-viewer", "imageview"]
    }

    fn usage(&self) -> &'static str {
        "viewer <file>"
    }

    fn description(&self) -> &'static str {
        "show a PNG, JPEG, BMP, PNM, QOI or ICO picture in a window"
    }

    fn foreground(&self) -> bool {
        true
    }

    fn windowed(&self) -> bool {
        true
    }

    fn animates(&self) -> bool {
        // The answer turns up by itself, so look for it while it's coming.
        WINDOW
            .try_lock()
            .is_some_and(|window| window.as_ref().is_some_and(|state| state.loading))
    }

    fn animation_period_ms(&self) -> u32 {
        50
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        let Some(fs) = context.platform.filesystem else {
            return false;
        };
        let identity = {
            let mut mailbox = MAILBOX.lock();
            mailbox.owner = mailbox.owner.wrapping_add(1).max(1);
            mailbox.request = None;
            mailbox.answer = None;
            mailbox.owner
        };
        let mut args = Text::<16>::new();
        let _ = write!(args, "{identity}");
        let processes = context.platform.processes;
        if processes.spawn(&ImageTask, args.as_str()).is_err() {
            return false;
        }
        let mut state = ViewerState {
            siblings: Vec::new(),
            current: None,
            name: Text::new(),
            picture: None,
            loading: false,
            failed: false,
            zoom: None,
            serial: 0,
        };
        if let Some(node) = fs
            .resolve(context.args.trim())
            .filter(|&node| !context.args.trim().is_empty() && fs.is_file(node))
        {
            state.open(fs, node);
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
        state.receive();
        let view = Rect::new(
            0,
            0,
            context.canvas.width() as u32,
            (context.canvas.height() as i32 - STATUS_HEIGHT).max(0) as u32,
        );
        context.dispatch_keys(|event| {
            let Event::KeyDown(key, _) = event else {
                return;
            };
            let fitted = state
                .picture
                .as_ref()
                .map_or(100, |picture| fit_percent(picture, view));
            match key {
                Key::Left | Key::PageUp => state.step(fs, -1),
                Key::Right | Key::PageDown | Key::Space => state.step(fs, 1),
                Key::Character('+') | Key::Character('=') => state.change_zoom(true, fitted),
                Key::Character('-') => state.change_zoom(false, fitted),
                Key::Character('0') => state.zoom = None,
                Key::Character('1') => state.zoom = Some(100),
                _ => {}
            }
        });
        context.dispatch_pointer(|report| {
            let fitted = state
                .picture
                .as_ref()
                .map_or(100, |picture| fit_percent(picture, view));
            if report.wheel != 0 {
                state.change_zoom(report.wheel > 0, fitted);
            }
        });
        state.draw(context.canvas);
        WindowFrame::Continue
    }

    fn close_window(&self) {
        {
            let mut mailbox = MAILBOX.lock();
            mailbox.owner = mailbox.owner.wrapping_add(1).max(1);
            mailbox.request = None;
            mailbox.answer = None;
        }
        *WINDOW.lock() = None;
    }

    fn run(&self, _context: &mut Context<'_>) -> i32 {
        eprintln!("viewer: this is a desktop window");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32) -> Decoded {
        Decoded {
            width,
            height,
            pixels: alloc::vec![0x00ff_0000; (width * height) as usize],
        }
    }

    #[test]
    fn image_names_are_recognised() {
        assert!(is_image_file("a.PNG"));
        assert!(is_image_file("b.ico"));
        assert!(!is_image_file("c.txt"));
        assert!(!is_image_file("png"));
    }

    #[test]
    fn fitting_shrinks_but_never_enlarges() {
        let view = Rect::new(0, 0, 100, 100);
        assert_eq!(fit_percent(&picture(50, 50), view), 100);
        assert_eq!(fit_percent(&picture(200, 100), view), 50);
        assert_eq!(fit_percent(&picture(100, 400), view), 25);
    }

    #[test]
    fn scaling_is_clipped_to_the_view() {
        let mut pixels = [0u32; 16];
        let mut canvas = Canvas::new(&mut pixels, 4, 4, 4).unwrap();
        let mut painter = Painter::new(&mut canvas);
        let clip = Rect::new(0, 0, 2, 2);
        draw_scaled(&mut painter, &picture(2, 2), -1, -1, 8, 8, clip);
        drop(painter);
        assert_eq!(pixels[0] & 0xff_ffff, 0xff_0000);
        assert_eq!(pixels[5] & 0xff_ffff, 0xff_0000);
        assert_eq!(pixels[2] & 0xff_ffff, 0, "right of the view");
        assert_eq!(pixels[8] & 0xff_ffff, 0, "below the view");
    }

    #[test]
    fn zoom_stays_in_range() {
        let mut state = ViewerState {
            siblings: Vec::new(),
            current: None,
            name: Text::new(),
            picture: None,
            loading: false,
            failed: false,
            zoom: None,
            serial: 0,
        };
        for _ in 0..64 {
            state.change_zoom(true, 100);
        }
        assert_eq!(state.zoom, Some(MAX_ZOOM));
        for _ in 0..64 {
            state.change_zoom(false, 100);
        }
        assert_eq!(state.zoom, Some(MIN_ZOOM));
    }
}
