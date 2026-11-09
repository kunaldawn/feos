//! The MilkDrop window: FeAmp's visualiser, running MilkDrop presets.
//!
//! It draws whatever the mixer is playing, so it works alongside any player,
//! though FeAmp's V button is the usual way in. Rendering is a task of its own
//! ([`VisTask`]), so a heavy preset slows only itself: the window shows the
//! newest finished frame, scaled up, with MilkDrop's overlays on top.
//!
//! Presets are every `.milk` file on the system; FeOS ships a selection of
//! MilkDrop's original pack in `/media/milkdrop`. Keys are MilkDrop's: Space or
//! N for the next preset, Backspace or P for the last, R for random order, L to
//! lock the current one, F to show the frame rate, and F1 for help.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::string::String;
use alloc::vec::Vec;

use feos_app::{App, Context, WindowContext, WindowFrame};
use feos_console::eprintln;
use feos_fs::{FileSystem, NodeId};
use feos_gfx::{Color, Rect};
use feos_gui::{Event, Key, MouseButton, Painter};
use feos_milkdrop::preset::MAX_PRESET_BYTES;
use feos_milkdrop::Visualizer;
use feos_sync::SpinLock;
use feos_text::Text;

/// Seconds a preset shows before the next, unless it's locked.
const PRESET_SECONDS: f64 = 20.0;
/// Frames a second the task aims for.
const TARGET_FPS: u64 = 30;
/// The largest image rendered; the window scales it up.
const MAX_RENDER: (usize, usize) = (320, 240);
/// Most presets listed.
const MAX_PRESETS: usize = 1024;
/// How long a preset's name shows after it starts.
const NAME_MS: u64 = 4_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    Next,
    Previous,
    ToggleRandom,
    ToggleLock,
}

struct Shared {
    /// The task allowed to run; any other leaves.
    owner: u32,
    /// The size the window wants rendered.
    want: (usize, usize),
    /// The newest frame.
    frame: Vec<u32>,
    size: (usize, usize),
    serial: u64,
    command: Option<Command>,
    name: Text<96>,
    index: usize,
    count: usize,
    changes: u64,
    fps: f32,
    random: bool,
    locked: bool,
}

static SHARED: SpinLock<Shared> = SpinLock::new(Shared {
    owner: 0,
    want: (0, 0),
    frame: Vec::new(),
    size: (0, 0),
    serial: 0,
    command: None,
    name: Text::EMPTY,
    index: 0,
    count: 0,
    changes: 0,
    fps: 0.0,
    random: true,
    locked: false,
});

static OPEN: AtomicBool = AtomicBool::new(false);

/// Whether the visualiser's window is open.
pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

/// Every `.milk` file, by name.
fn find_presets(fs: &dyn FileSystem) -> Vec<NodeId> {
    let mut found: Vec<NodeId> = Vec::new();
    super::walk_files(fs, |node, name| {
        let is_preset = name.len() > 5 && name[name.len() - 5..].eq_ignore_ascii_case(".milk");
        if is_preset && found.len() < MAX_PRESETS && found.try_reserve(1).is_ok() {
            found.push(node);
        }
    });
    found.sort_by(|a, b| fs.name(*a).unwrap_or("").cmp(fs.name(*b).unwrap_or("")));
    found
}

/// Reads and loads preset `node`. Returns its name, or `None` if it won't load.
fn load(fs: &dyn FileSystem, visualizer: &mut Visualizer, node: NodeId) -> Option<Text<96>> {
    let size = fs.size(node);
    if size == 0 || size > MAX_PRESET_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).ok()?;
    bytes.resize(size, 0);
    let got = fs.read(node, 0, &mut bytes);
    bytes.truncate(got);
    let text = String::from_utf8_lossy(&bytes);
    visualizer.load_text(&text).ok()?;
    let name = fs.name(node).unwrap_or("?");
    Some(Text::from_str(name.strip_suffix(".milk").unwrap_or(name)))
}

/// The rendering task. Its argument is its identity in [`SHARED`].
pub struct VisTask;

impl App for VisTask {
    fn name(&self) -> &'static str {
        "milkdrop-render"
    }

    fn usage(&self) -> &'static str {
        "(started by the MilkDrop window)"
    }

    fn description(&self) -> &'static str {
        "render MilkDrop presets for the visualiser window"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let platform = context.platform;
        let (Some(fs), Some(identity)) = (platform.filesystem, context.args.trim().parse::<u32>().ok()) else {
            return 2;
        };
        let processes = platform.processes;
        let clock = platform.clock;
        let presets = find_presets(fs);
        let mut rng = clock.uptime_ms() | 1;
        let mut random = |count: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % count.max(1) as u64) as usize
        };
        let mut index = random(presets.len());
        let mut history: Vec<usize> = Vec::new();
        let mut visualizer: Option<Visualizer> = None;
        let mut size = (0, 0);
        let mut pcm = [0i16; 576 * 2];
        let mut heard = 0u64;
        let mut last = clock.uptime_ms();
        let mut reload = true;
        {
            let mut shared = SHARED.lock();
            shared.count = presets.len();
        }
        while !processes.kill_requested() {
            let started = clock.uptime_ms();
            let (want, command, random_order, locked) = {
                let mut shared = SHARED.lock();
                if shared.owner != identity {
                    return 0;
                }
                (shared.want, shared.command.take(), shared.random, shared.locked)
            };
            let want = (want.0.clamp(32, MAX_RENDER.0), want.1.clamp(24, MAX_RENDER.1));
            if want != size || visualizer.is_none() {
                visualizer = Visualizer::new(want.0, want.1, 48_000);
                size = want;
                reload = true;
            }
            let Some(vis) = visualizer.as_mut() else {
                processes.sleep_ms(100);
                continue;
            };
            vis.preset_seconds = PRESET_SECONDS;
            let mut advance = |forward: bool, index: &mut usize, history: &mut Vec<usize>| {
                if presets.is_empty() {
                    return;
                }
                if forward {
                    if history.try_reserve(1).is_ok() {
                        if history.len() >= 64 {
                            history.remove(0);
                        }
                        history.push(*index);
                    }
                    *index = if random_order { random(presets.len()) } else { (*index + 1) % presets.len() };
                } else {
                    *index = history.pop().unwrap_or((*index + presets.len() - 1) % presets.len());
                }
            };
            match command {
                Some(Command::Next) => {
                    advance(true, &mut index, &mut history);
                    reload = true;
                }
                Some(Command::Previous) => {
                    advance(false, &mut index, &mut history);
                    reload = true;
                }
                Some(Command::ToggleRandom) => {
                    let mut shared = SHARED.lock();
                    shared.random = !shared.random;
                }
                Some(Command::ToggleLock) => {
                    let mut shared = SHARED.lock();
                    shared.locked = !shared.locked;
                }
                None => {}
            }
            if !locked && !reload && !presets.is_empty() && vis.preset_age() > PRESET_SECONDS {
                advance(true, &mut index, &mut history);
                reload = true;
            }
            if reload {
                reload = false;
                // A preset that won't load is passed over, for as many as there are.
                let mut name = None;
                for _ in 0..presets.len() {
                    name = load(fs, vis, presets[index]);
                    if name.is_some() {
                        break;
                    }
                    index = (index + 1) % presets.len();
                }
                let mut shared = SHARED.lock();
                shared.name = name.unwrap_or(Text::from_str("FeOS default"));
                shared.index = index;
                shared.changes += 1;
            }

            // What's playing now; silence when nothing new has been mixed.
            let count = feos_mixer::scope(&mut pcm);
            if count == heard {
                pcm.fill(0);
            }
            heard = count;
            let now = clock.uptime_ms();
            let seconds = now.saturating_sub(last) as f32 / 1000.0;
            last = now;
            vis.render(&pcm, seconds.max(0.001));

            {
                let mut shared = SHARED.lock();
                let pixels = vis.pixels();
                if shared.frame.len() != pixels.len() {
                    shared.frame.clear();
                    if shared.frame.try_reserve_exact(pixels.len()).is_ok() {
                        shared.frame.resize(pixels.len(), 0);
                    }
                }
                if shared.frame.len() == pixels.len() {
                    shared.frame.copy_from_slice(pixels);
                    shared.size = (vis.width(), vis.height());
                    shared.serial += 1;
                    shared.fps = vis.fps();
                }
            }
            let spent = clock.uptime_ms().saturating_sub(started);
            processes.sleep_ms((1000 / TARGET_FPS).saturating_sub(spent).max(2));
        }
        0
    }
}

struct WindowState {
    pid: Option<u32>,
    identity: u32,
    show_fps: bool,
    show_help: bool,
    seen_changes: u64,
    name_until: u64,
    next_check: u64,
}

static WINDOW: SpinLock<Option<WindowState>> = SpinLock::new(None);
static IDENTITY: SpinLock<u32> = SpinLock::new(0);

/// What the window asks the task for: half the window, at most [`MAX_RENDER`].
fn render_size(width: usize, height: usize) -> (usize, usize) {
    let (mut w, mut h) = ((width / 2).max(32), (height / 2).max(24));
    if w > MAX_RENDER.0 || h > MAX_RENDER.1 {
        let scale = (MAX_RENDER.0 as f32 / w as f32).min(MAX_RENDER.1 as f32 / h as f32);
        w = (w as f32 * scale) as usize;
        h = (h as f32 * scale) as usize;
    }
    (w, h)
}

/// Scales `frame` to fill `out`, nearest pixel.
fn stretch(out: &mut [u32], out_width: usize, out_height: usize, stride: usize, frame: &[u32], size: (usize, usize)) {
    let (width, height) = size;
    if width == 0 || height == 0 || frame.len() < width * height {
        return;
    }
    let step_x = (width << 16) / out_width.max(1);
    let step_y = (height << 16) / out_height.max(1);
    for y in 0..out_height {
        let source_row = ((y * step_y) >> 16).min(height - 1) * width;
        let row = &mut out[y * stride..y * stride + out_width];
        let mut fx = 0usize;
        for pixel in row.iter_mut() {
            *pixel = frame[source_row + (fx >> 16).min(width - 1)];
            fx += step_x;
        }
    }
}

fn spawn(context: &WindowContext<'_, '_>, state: &mut WindowState) {
    let identity = {
        let mut next = IDENTITY.lock();
        *next = next.wrapping_add(1).max(1);
        *next
    };
    state.identity = identity;
    SHARED.lock().owner = identity;
    let mut args = Text::<16>::new();
    let _ = write!(args, "{identity}");
    state.pid = context.platform.processes.spawn(&VisTask, args.as_str()).ok();
}

/// The MilkDrop window.
pub struct MilkDrop;

impl App for MilkDrop {
    fn name(&self) -> &'static str {
        "milkdrop"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["feamp-vis", "visualizer"]
    }

    fn usage(&self) -> &'static str {
        "milkdrop"
    }

    fn description(&self) -> &'static str {
        "MilkDrop visualisation of whatever is playing"
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
        33
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        let mut state = WindowState {
            pid: None,
            identity: 0,
            show_fps: false,
            show_help: false,
            seen_changes: 0,
            name_until: 0,
            next_check: 0,
        };
        SHARED.lock().want = render_size(context.canvas.width(), context.canvas.height());
        spawn(context, &mut state);
        if state.pid.is_none() {
            SHARED.lock().owner = 0;
            return false;
        }
        *WINDOW.lock() = Some(state);
        OPEN.store(true, Ordering::Relaxed);
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        let Some(mut state) = WINDOW.lock().take() else {
            return WindowFrame::Close;
        };
        let now = context.now_ms;
        let mut close = false;
        // A task that died (a fault in a preset, say) is replaced.
        if now >= state.next_check {
            state.next_check = now + 1000;
            if state.pid.is_some_and(|pid| context.platform.processes.detail(pid).is_none()) {
                spawn(context, &mut state);
            }
        }
        let mut command = None;
        context.dispatch_keys(|event| {
            if let Event::KeyDown(key, _) = event {
                match key {
                    Key::Space | Key::Right | Key::Character('n' | 'N' | 'h' | 'H') => command = Some(Command::Next),
                    Key::Backspace | Key::Left | Key::Character('p' | 'P') => command = Some(Command::Previous),
                    Key::Character('r' | 'R') => command = Some(Command::ToggleRandom),
                    Key::Character('l' | 'L') => command = Some(Command::ToggleLock),
                    Key::Character('f' | 'F') => state.show_fps = !state.show_fps,
                    Key::Enter => state.name_until = now + NAME_MS,
                    Key::F1 => state.show_help = !state.show_help,
                    Key::Escape => close = true,
                    _ => {}
                }
            }
        });
        context.dispatch_pointer(|report| {
            if report.left == Some(true) {
                command = Some(Command::Next);
            }
        });
        let _ = MouseButton::Left;

        let (width, height) = (context.canvas.width(), context.canvas.height());
        let stride = context.canvas.stride();
        let (name, index, count, fps, random, locked) = {
            let mut shared = SHARED.lock();
            shared.want = render_size(width, height);
            if command.is_some() {
                shared.command = command;
            }
            if shared.changes != state.seen_changes {
                state.seen_changes = shared.changes;
                state.name_until = now + NAME_MS;
            }
            let size = shared.size;
            if shared.serial > 0 {
                stretch(context.canvas.pixels_mut(), width, height, stride, &shared.frame, size);
            } else {
                context.canvas.clear(Color::BLACK);
            }
            (shared.name, shared.index, shared.count, shared.fps, shared.random, shared.locked)
        };

        let mut painter = Painter::new(context.canvas);
        let shadowed = |painter: &mut Painter<'_, '_>, x: i32, y: i32, text: &str, color: Color| {
            painter.draw_text(x + 1, y + 1, text, Color::BLACK, 1);
            painter.draw_text(x, y, text, color, 1);
        };
        if now < state.name_until {
            let mut line = Text::<128>::new();
            let _ = write!(line, "{}", name.as_str());
            let x = width as i32 - 8 - painter.text_width(line.as_str(), 1);
            shadowed(&mut painter, x.max(4), 6, line.as_str(), Color::rgb(255, 255, 255));
            let mut detail = Text::<64>::new();
            let _ = write!(detail, "{}/{}{}{}", index + 1, count, if random { "  random" } else { "" }, if locked { "  locked" } else { "" });
            let x = width as i32 - 8 - painter.text_width(detail.as_str(), 1);
            shadowed(&mut painter, x.max(4), 18, detail.as_str(), Color::rgb(190, 200, 255));
        }
        if state.show_fps {
            let mut line = Text::<32>::new();
            let _ = write!(line, "{:.1} fps", fps);
            shadowed(&mut painter, 6, 6, line.as_str(), Color::rgb(255, 255, 140));
        }
        if state.show_help {
            let lines = [
                "MilkDrop for FeAmp",
                "",
                "Space, N   next preset",
                "Backspace  previous preset",
                "R          random order on/off",
                "L          lock this preset",
                "Enter      show the preset's name",
                "F          frame rate",
                "F1         this help",
                "click      next preset",
            ];
            let panel = Rect::new(10, 30, 290, lines.len() as u32 * 12 + 12);
            painter.fill_rect_alpha(panel, Color::BLACK, 170);
            for (row, line) in lines.iter().enumerate() {
                painter.draw_text(18, 36 + row as i32 * 12, line, Color::rgb(220, 230, 255), 1);
            }
        }
        if count == 0 && now % 4000 < 3000 {
            shadowed(&mut painter, 8, height as i32 - 16, "No .milk presets found: showing the default", Color::rgb(255, 200, 120));
        }
        *WINDOW.lock() = Some(state);
        if close {
            WindowFrame::Close
        } else {
            WindowFrame::Continue
        }
    }

    fn close_window(&self) {
        SHARED.lock().owner = 0;
        *WINDOW.lock() = None;
        OPEN.store(false, Ordering::Relaxed);
    }

    fn run(&self, _context: &mut Context<'_>) -> i32 {
        eprintln!("milkdrop: this is a desktop window; open it from the Start menu or FeAmp");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_size_is_half_the_window_up_to_a_limit() {
        assert_eq!(render_size(512, 384), (256, 192));
        let (w, h) = render_size(1600, 1200);
        assert!(w <= MAX_RENDER.0 && h <= MAX_RENDER.1);
        assert_eq!(render_size(10, 10), (32, 24));
    }

    #[test]
    fn stretch_fills_the_window() {
        let frame = [1u32, 2, 3, 4];
        let mut out = [0u32; 16];
        stretch(&mut out, 4, 4, 4, &frame, (2, 2));
        assert_eq!(out, [1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
    }
}
