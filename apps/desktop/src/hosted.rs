//! Windowed front ends for commands that otherwise only print to the console.
//!
//! The Start menu's entries for `info` and `help` open these apps, which suit a
//! window better than the same text in a Terminal. Anything else that only prints
//! opens in a Terminal window. The shell commands are untouched: [`Registry`]
//! shadows them only for desktop launches.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicI32, Ordering};

use feos_app::{App, AppRegistry, Context, Platform, WindowContext, WindowFrame, VERSION};
use feos_gfx::{Canvas, Color};
use feos_gui::{Event, Painter};
use feos_text::Text;

/// Resolves entries for desktop launches, preferring the windowed front ends.
pub struct Registry(pub &'static dyn AppRegistry);

impl AppRegistry for Registry {
    fn find(&self, name: &str) -> Option<&'static dyn App> {
        match name {
            "info" => Some(&SYSTEM_INFO),
            "help" => Some(&CONSOLE_HELP),
            _ => self.0.find(name),
        }
    }
}

const LINE_HEIGHT: i32 = 18;
const MARGIN: i32 = 12;
const BACKGROUND: Color = Color::rgb(255, 255, 255);
const TEXT: Color = Color::rgb(24, 24, 24);
const MUTED: Color = Color::rgb(92, 92, 92);

/// Applies wheel and key scrolling to `offset`, and returns the new offset.
fn scroll(context: &WindowContext<'_, '_>, offset: &AtomicI32, lines: i32) -> i32 {
    let page = context.canvas.height() as i32 / LINE_HEIGHT;
    let mut value = offset.load(Ordering::Relaxed);
    context.dispatch_keys(|event| {
        if let Event::KeyDown(key, _) = event {
            match key {
                feos_gui::Key::Up => value -= 1,
                feos_gui::Key::Down => value += 1,
                feos_gui::Key::PageUp => value -= page,
                feos_gui::Key::PageDown => value += page,
                feos_gui::Key::Home => value = 0,
                feos_gui::Key::End => value = lines,
                _ => {}
            }
        }
    });
    context.dispatch_pointer(|report| value -= report.wheel * 3);
    let value = value.clamp(0, (lines - page + 2).max(0));
    offset.store(value, Ordering::Relaxed);
    value
}

/// Writes rows of text from `scroll` down, clipped to the canvas.
struct Page<'a, 'b> {
    painter: Painter<'a, 'b>,
    scroll: i32,
    line: i32,
    height: i32,
}

impl Page<'_, '_> {
    fn row(&mut self, text: &str, color: Color) {
        let y = MARGIN + (self.line - self.scroll) * LINE_HEIGHT;
        if y > -LINE_HEIGHT && y < self.height {
            self.painter.draw_text(MARGIN, y, text, color, 1);
        }
        self.line += 1;
    }

    fn field(&mut self, label: &str, value: &str) {
        let mut line = Text::<128>::new();
        let _ = write!(line, "{label:<18}{value}");
        self.row(line.as_str(), TEXT);
    }
}

fn page<'a, 'b>(canvas: &'a mut Canvas<'b>, scroll: i32) -> Page<'a, 'b> {
    canvas.clear(BACKGROUND);
    let height = canvas.height() as i32;
    Page {
        painter: Painter::new(canvas),
        scroll,
        line: 0,
        height,
    }
}

/// `info`, in a window.
struct SystemInfo;

static SYSTEM_INFO: SystemInfo = SystemInfo;
static INFO_SCROLL: AtomicI32 = AtomicI32::new(0);

/// Lines [`draw_info`] writes, for the scroll limit.
const INFO_LINES: i32 = 14;

fn draw_info(page: &mut Page<'_, '_>, platform: &Platform, width: usize, height: usize) {
    let mut value = Text::<96>::new();
    let (kernel_start, kernel_end) = platform.board.kernel_range;
    let (bss_start, bss_end) = platform.board.bss_range;

    let mut heading = Text::<32>::new();
    let _ = write!(heading, "feos {VERSION}");
    page.row(heading.as_str(), TEXT);
    page.row("", TEXT);
    page.field("Board", platform.board.name);
    page.field("Architecture", platform.board.arch);

    let mut set = |page: &mut Page<'_, '_>, label: &str, args: core::fmt::Arguments<'_>| {
        value.clear();
        let _ = value.write_fmt(args);
        page.field(label, value.as_str());
    };
    set(
        page,
        "CPUs online",
        format_args!("{}", platform.processes.system().cpus.max(1)),
    );
    set(
        page,
        "Timer",
        format_args!("{} Hz", platform.clock.frequency()),
    );
    let uptime = platform.clock.uptime_ms() / 1000;
    set(
        page,
        "Uptime",
        format_args!("{}:{:02}:{:02}", uptime / 3600, (uptime / 60) % 60, uptime % 60),
    );
    set(
        page,
        "Kernel image",
        format_args!("{kernel_start:#x} .. {kernel_end:#x}"),
    );
    set(page, "BSS", format_args!("{bss_start:#x} .. {bss_end:#x}"));
    set(
        page,
        "Console UART",
        format_args!("{:#x}", platform.board.console_base),
    );
    set(page, "Window", format_args!("{width}x{height}"));
    if platform.board.framebuffer_addr != 0 {
        set(
            page,
            "Framebuffer",
            format_args!("{:#x}", platform.board.framebuffer_addr),
        );
    } else {
        page.field("Framebuffer", "unavailable");
    }
    match platform.audio {
        Some(audio) if audio.is_ready() => set(
            page,
            "Audio",
            format_args!("{} Hz, {} channels", audio.sample_rate(), audio.channels()),
        ),
        _ => page.field("Audio", "unavailable"),
    }
}

impl App for SystemInfo {
    fn name(&self) -> &'static str {
        "info"
    }
    fn usage(&self) -> &'static str {
        "info"
    }
    fn description(&self) -> &'static str {
        "kernel, CPU, board and display information"
    }
    fn windowed(&self) -> bool {
        true
    }
    fn animates(&self) -> bool {
        // The uptime line ticks over.
        true
    }
    fn animation_period_ms(&self) -> u32 {
        1000
    }
    fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
        INFO_SCROLL.store(0, Ordering::Relaxed);
        true
    }
    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        let offset = scroll(context, &INFO_SCROLL, INFO_LINES);
        let (width, height) = (context.canvas.width(), context.canvas.height());
        let mut output = page(context.canvas, offset);
        draw_info(&mut output, context.platform, width, height);
        WindowFrame::Continue
    }
    fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
}

/// `help`, in a window.
struct ConsoleHelp;

static CONSOLE_HELP: ConsoleHelp = ConsoleHelp;
static HELP_SCROLL: AtomicI32 = AtomicI32::new(0);

/// The commands the help window describes. Those a build leaves out are
/// skipped, since each row comes from looking the name up.
const COMMANDS: &[&str] = &[
    "help", "info", "uptime", "ps", "kill", "spawn", "tick", "sleep", "meminfo", "cpuinfo",
    "uname", "ls", "cat", "tree", "hexdump", "files", "taskmgr", "terminal", "player", "dosbox",
    "gui", "desktop", "splash", "tone", "reboot", "poweroff",
];

impl App for ConsoleHelp {
    fn name(&self) -> &'static str {
        "help"
    }
    fn usage(&self) -> &'static str {
        "help"
    }
    fn description(&self) -> &'static str {
        "list the built-in shell commands"
    }
    fn windowed(&self) -> bool {
        true
    }
    fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
        HELP_SCROLL.store(0, Ordering::Relaxed);
        true
    }
    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        const FIXED_LINES: i32 = 16;
        let offset = scroll(context, &HELP_SCROLL, COMMANDS.len() as i32 + FIXED_LINES);
        let platform = context.platform;
        let mut output = page(context.canvas, offset);
        output.row("Help and shell commands", TEXT);
        output.row("", TEXT);
        output.row("Keyboard", MUTED);
        output.row("  Ctrl+Esc        open the Start menu", TEXT);
        output.row("  Alt+Tab         switch to another window", TEXT);
        output.row("  Alt+Shift+Tab   send this window to the back", TEXT);
        output.row("  Alt+F4          close the window", TEXT);
        output.row("  Alt+Space       open the window menu", TEXT);
        output.row("  Ctrl+\\          leave the desktop for the serial shell", TEXT);
        output.row("", TEXT);
        output.row("Commands (Start > Run, or the Terminal)", MUTED);
        for name in COMMANDS {
            if let Some(app) = platform.apps.find(name) {
                let mut line = Text::<128>::new();
                let _ = write!(line, "  {:<22}{}", app.usage(), app.description());
                output.row(line.as_str(), TEXT);
            }
        }
        WindowFrame::Continue
    }
    fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windowed_front_ends_shadow_only_their_own_names() {
        struct Empty;
        impl AppRegistry for Empty {
            fn find(&self, _name: &str) -> Option<&'static dyn App> {
                None
            }
        }
        static EMPTY: Empty = Empty;
        let registry = Registry(&EMPTY);
        assert!(registry.find("info").is_some_and(|app| app.windowed()));
        assert!(registry.find("help").is_some_and(|app| app.windowed()));
        assert!(registry.find("ps").is_none());
    }
}
