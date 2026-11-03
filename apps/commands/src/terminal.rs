//! A windowed terminal for the desktop.
//!
//! The serial console and this window share the same `feos-sh` engine. The
//! window keeps its own shell instance and overlay filesystem, renders a text
//! grid with the 8x8 bitmap font, and feeds keystrokes into the same parser,
//! builtins and coreutils.
//!
//! External apps (`ps`, `files`, …) are intentionally not run from this window:
//! they print through the global console and some take over the framebuffer.
//! Use the serial terminal for those, or launch them from the desktop.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use feos_app::{App, Context, WindowContext, WindowFrame};
use feos_coreutils::{BytesInput, BytesOutput, MemoryInfo, System};
use feos_gfx::{Canvas, Color, Rect};
use feos_gui::{Event, Key, Modifiers};
use feos_sh::{Host, Io, Shell};
use feos_sync::SpinLock;

static TERMINAL: SpinLock<Option<TerminalState>> = SpinLock::new(None);

static UPTIME_MS: AtomicU64 = AtomicU64::new(0);
static MEMORY_TOTAL: AtomicUsize = AtomicUsize::new(0);
static MEMORY_USED: AtomicUsize = AtomicUsize::new(0);
static MEMORY_FREE: AtomicUsize = AtomicUsize::new(0);
static PID: AtomicU32 = AtomicU32::new(1);

struct WindowSystem;

impl System for WindowSystem {
    fn sysname(&self) -> &'static str {
        "FEOS"
    }

    fn nodename(&self) -> &'static str {
        "feos"
    }

    fn release(&self) -> &'static str {
        feos_app::VERSION
    }

    fn version(&self) -> &'static str {
        "feos"
    }

    fn machine(&self) -> &'static str {
        "aarch64"
    }

    fn username(&self) -> &'static str {
        "feos"
    }

    fn uid(&self) -> u32 {
        1000
    }

    fn gid(&self) -> u32 {
        1000
    }

    fn uptime_ms(&self) -> u64 {
        UPTIME_MS.load(Ordering::Relaxed)
    }

    fn cpu_count(&self) -> usize {
        1
    }

    fn pid(&self) -> u32 {
        PID.load(Ordering::Relaxed)
    }

    fn memory(&self) -> Option<MemoryInfo> {
        let total = MEMORY_TOTAL.load(Ordering::Relaxed);
        if total == 0 {
            return None;
        }
        Some(MemoryInfo {
            total,
            used: MEMORY_USED.load(Ordering::Relaxed),
            free: MEMORY_FREE.load(Ordering::Relaxed),
        })
    }
}

static WINDOW_SYSTEM: WindowSystem = WindowSystem;

struct WindowHost;

impl Host for WindowHost {
    fn run_external(
        &mut self,
        _name: &str,
        _args: &[String],
        _stdin: &mut dyn feos_coreutils::Input,
        _stdout: &mut dyn feos_coreutils::Output,
        _stderr: &mut dyn feos_coreutils::Output,
        _background: bool,
    ) -> Option<i32> {
        None
    }
}

struct TerminalState {
    shell: Shell<'static>,
    lines: Vec<String>,
    input: Vec<char>,
    cursor: usize,
    scroll: usize,
    history_index: usize,
    saved_line: String,
    close_requested: bool,
}

impl TerminalState {
    fn new(filesystem: Option<&'static dyn feos_fs::FileSystem>) -> Self {
        let mut shell = Shell::new(filesystem, &WINDOW_SYSTEM);
        shell.env.set("TERM", "feos-window");
        let mut state = Self {
            shell,
            lines: Vec::new(),
            input: Vec::new(),
            cursor: 0,
            scroll: 0,
            history_index: 0,
            saved_line: String::new(),
            close_requested: false,
        };
        state
            .lines
            .push(String::from("FEOS terminal - Bash-compatible shell"));
        state.lines.push(String::from(
            "Type 'help' for commands. Ctrl-D closes this window.",
        ));
        state.lines.push(String::new());
        state.history_index = state.shell.history.len();
        state
    }

    fn execute(&mut self) {
        let line: String = self.input.iter().collect();
        let prompt = self.shell.prompt();
        self.lines.push(format!("{prompt}{line}"));

        if !line.trim().is_empty() && self.shell.history.last() != Some(&line) {
            self.shell.history.push(line.clone());
            if self.shell.history.len() > 200 {
                self.shell.history.remove(0);
            }
        }

        let mut stdin = BytesInput::empty();
        let mut stdout = BytesOutput::new();
        let mut stderr = BytesOutput::new();
        let status = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
            };
            self.shell.execute_line(&line, &mut io, &mut WindowHost)
        };
        let _ = status;
        self.append_output(stdout.bytes());
        self.append_output(stderr.bytes());

        self.input.clear();
        self.cursor = 0;
        self.scroll = 0;
        self.history_index = self.shell.history.len();

        if self.shell.take_exit_status().is_some() {
            self.close_requested = true;
            self.lines.push(String::from("[terminal closed]"));
        }
    }

    fn append_output(&mut self, bytes: &[u8]) {
        if contains_clear(bytes) {
            self.lines.clear();
        }

        let text = String::from_utf8_lossy(bytes);
        let mut current = String::new();
        let mut escape = false;

        for ch in text.chars() {
            if escape {
                if ch.is_ascii_alphabetic() || ch == '~' {
                    escape = false;
                }
                continue;
            }
            match ch {
                '\x1b' => escape = true,
                '\n' => {
                    self.lines.push(core::mem::take(&mut current));
                }
                '\r' => {}
                '\x08' => {
                    current.pop();
                }
                '\t' => current.push_str("    "),
                ch if ch.is_control() => {}
                ch => current.push(ch),
            }
        }
        if !current.is_empty() {
            self.lines.push(current);
        }
        if self.lines.len() > 1000 {
            let excess = self.lines.len() - 1000;
            self.lines.drain(0..excess);
        }
    }

    fn insert(&mut self, ch: char) {
        if self.cursor < self.input.len() {
            self.input.insert(self.cursor, ch);
        } else {
            self.input.push(ch);
        }
        self.cursor += 1;
        self.scroll = 0;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    fn line(&self) -> String {
        self.input.iter().collect()
    }

    fn complete(&mut self) {
        let line = self.line();
        let byte_cursor = line
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
        let completion = self.shell.complete(&line, byte_cursor, &WindowHost);
        self.input = completion.line.chars().collect();
        self.cursor = completion.line[..completion.cursor].chars().count();
        if !completion.candidates.is_empty() {
            let mut line = String::new();
            for candidate in completion.candidates {
                if !line.is_empty() {
                    line.push_str("  ");
                }
                line.push_str(&candidate);
            }
            self.lines.push(line);
        }
    }
}

fn contains_clear(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"\x1b[2J")
}

/// The `terminal` application.
pub struct Terminal;

impl App for Terminal {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["term", "xterm"]
    }

    fn usage(&self) -> &'static str {
        "terminal"
    }

    fn description(&self) -> &'static str {
        "open the Bash-compatible terminal window"
    }

    fn foreground(&self) -> bool {
        false
    }

    fn windowed(&self) -> bool {
        true
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        let state = TerminalState::new(context.platform.filesystem);
        update_system(context);
        let mut guard = TERMINAL.lock();
        *guard = Some(state);
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        update_system(context);

        let mut guard = match TERMINAL.try_lock() {
            Some(guard) => guard,
            None => return WindowFrame::Continue,
        };
        let Some(state) = guard.as_mut() else {
            return WindowFrame::Close;
        };

        if context.keyboard_active {
            let events: Vec<Event> = context.input.events().collect();
            // The serial decoder emits KeyDown + Text; some keyboard drivers
            // emit only KeyDown(Character). Insert from Text when present and
            // fall back to the key event otherwise, so neither source doubles
            // characters.
            let has_text = events.iter().any(|event| matches!(event, Event::Text(_)));
            for event in events {
                handle_event(state, event, context, has_text);
            }
        }
        if context.pointer_active {
            for event in context.input.events() {
                if let Event::PointerWheel(delta) = event {
                    if delta > 0 {
                        state.scroll = state.scroll.saturating_add(2);
                    } else {
                        state.scroll = state.scroll.saturating_sub(2);
                    }
                }
            }
        }

        if state.close_requested {
            return WindowFrame::Close;
        }

        render(state, context.canvas);
        WindowFrame::Continue
    }

    fn close_window(&self) {
        *TERMINAL.lock() = None;
    }

    fn run(&self, _context: &mut Context<'_>) {
        feos_console::println!(
            "terminal: open it from the desktop, or use the serial shell directly"
        );
    }
}

fn update_system(context: &WindowContext<'_, '_>) {
    UPTIME_MS.store(context.platform.clock.uptime_ms(), Ordering::Relaxed);
    PID.store(context.platform.processes.current(), Ordering::Relaxed);
    if let Some(memory) = context.platform.memory {
        let info = memory.info();
        MEMORY_TOTAL.store(info.total_bytes, Ordering::Relaxed);
        MEMORY_USED.store(info.used_bytes, Ordering::Relaxed);
        MEMORY_FREE.store(info.free_bytes, Ordering::Relaxed);
    }
}

fn handle_event(
    state: &mut TerminalState,
    event: Event,
    context: &WindowContext<'_, '_>,
    has_text: bool,
) {
    match event {
        Event::Text(ch) if !ch.is_control() => state.insert(ch),
        Event::KeyDown(key, modifiers) => {
            if modifiers.contains(Modifiers::CTRL) {
                match key {
                    Key::Character('c') | Key::Character('C') => {
                        state.lines.push(format!("{}^C", state.shell.prompt()));
                        state.input.clear();
                        state.cursor = 0;
                    }
                    Key::Character('d') | Key::Character('D') => {
                        if state.input.is_empty() {
                            state.close_requested = true;
                        } else {
                            state.delete();
                        }
                    }
                    Key::Character('l') | Key::Character('L') => state.lines.clear(),
                    Key::Character('u') | Key::Character('U') => {
                        state.input.drain(..state.cursor);
                        state.cursor = 0;
                    }
                    Key::Character('k') | Key::Character('K') => {
                        state.input.truncate(state.cursor);
                    }
                    Key::Character('a') | Key::Character('A') => state.cursor = 0,
                    Key::Character('e') | Key::Character('E') => state.cursor = state.input.len(),
                    Key::Character('w') | Key::Character('W') => {
                        let mut start = state.cursor;
                        while start > 0 && state.input[start - 1].is_whitespace() {
                            start -= 1;
                        }
                        while start > 0 && !state.input[start - 1].is_whitespace() {
                            start -= 1;
                        }
                        state.input.drain(start..state.cursor);
                        state.cursor = start;
                    }
                    _ => {}
                }
                return;
            }

            match key {
                Key::Character(ch) if !has_text && !ch.is_control() => state.insert(ch),
                Key::Enter => state.execute(),
                Key::Backspace => state.backspace(),
                Key::Delete => state.delete(),
                Key::Left => {
                    state.cursor = state.cursor.saturating_sub(1);
                }
                Key::Right => {
                    if state.cursor < state.input.len() {
                        state.cursor += 1;
                    }
                }
                Key::Home => state.cursor = 0,
                Key::End => state.cursor = state.input.len(),
                Key::Up => {
                    if state.history_index == state.shell.history.len() {
                        state.saved_line = state.line();
                    }
                    if state.history_index > 0 {
                        state.history_index -= 1;
                        state.input = state.shell.history[state.history_index].chars().collect();
                        state.cursor = state.input.len();
                        state.scroll = 0;
                    }
                }
                Key::Down => {
                    if state.history_index < state.shell.history.len() {
                        state.history_index += 1;
                        if state.history_index == state.shell.history.len() {
                            state.input = state.saved_line.chars().collect();
                        } else {
                            state.input =
                                state.shell.history[state.history_index].chars().collect();
                        }
                        state.cursor = state.input.len();
                    }
                }
                Key::Tab => state.complete(),
                Key::PageUp => {
                    let rows = visible_rows(context.canvas);
                    state.scroll = state.scroll.saturating_add(rows / 2).min(state.lines.len());
                }
                Key::PageDown => {
                    let rows = visible_rows(context.canvas);
                    state.scroll = state.scroll.saturating_sub(rows / 2);
                }
                Key::Escape => state.close_requested = true,
                _ => {}
            }
        }
        _ => {}
    }
}

fn visible_rows(canvas: &Canvas<'_>) -> usize {
    (canvas.height().saturating_sub(8) / LINE_HEIGHT as usize).max(1)
}

const CELL_WIDTH: i32 = 8;
const LINE_HEIGHT: i32 = 10;

fn render(state: &TerminalState, canvas: &mut Canvas<'_>) {
    let width = canvas.width() as i32;
    let height = canvas.height() as i32;
    let background = Color::rgb(8, 12, 16);
    let text_color = Color::rgb(196, 220, 206);
    let prompt_color = Color::rgb(92, 222, 148);

    canvas.fill_rect(
        Rect::new(0, 0, width.max(1) as u32, height.max(1) as u32),
        background,
    );

    let cols = ((width - 8).max(CELL_WIDTH) / CELL_WIDTH) as usize;
    let rows = visible_rows(canvas);
    if rows == 0 || cols == 0 {
        return;
    }

    let history_rows = rows.saturating_sub(1);
    let total = state.lines.len();
    let start = if state.scroll == 0 {
        total.saturating_sub(history_rows)
    } else {
        total
            .saturating_sub(history_rows)
            .saturating_sub(state.scroll)
    };

    let mut y = 4;
    for line in state.lines.iter().skip(start).take(history_rows) {
        draw_clipped(canvas, 4, y, line, cols, text_color);
        y += LINE_HEIGHT;
    }

    let prompt = state.shell.prompt();
    let prompt_row = 4 + history_rows as i32 * LINE_HEIGHT;
    draw_clipped(canvas, 4, prompt_row, &prompt, cols, prompt_color);

    let prompt_width = prompt.chars().count();
    let input = state.line();
    let input_x = 4 + prompt_width as i32 * CELL_WIDTH;
    draw_clipped(
        canvas,
        input_x,
        prompt_row,
        &input,
        cols.saturating_sub(prompt_width),
        text_color,
    );

    let cursor_x = input_x + state.cursor as i32 * CELL_WIDTH;
    if cursor_x < width - 2 {
        canvas.fill_rect(
            Rect::new(cursor_x, prompt_row + LINE_HEIGHT - 2, CELL_WIDTH as u32, 2),
            prompt_color,
        );
    }
}

fn draw_clipped(canvas: &mut Canvas<'_>, x: i32, y: i32, text: &str, columns: usize, color: Color) {
    if columns == 0 {
        return;
    }
    let clipped: String = text.chars().take(columns).collect();
    canvas.draw_text(x, y, &clipped, color, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_app::{AppRegistry, BoardInfo, Platform};
    use feos_app::{ProcessInfo, Processes, SpawnError};
    use feos_gfx::Point;
    use feos_gui::Input;
    use feos_hal::{Clock, Cpu, Power};

    struct FakeClock;
    impl Clock for FakeClock {
        fn frequency(&self) -> u64 {
            1_000_000
        }
        fn ticks(&self) -> u64 {
            0
        }
        fn spin_ms(&self, _ms: u64) {}
    }
    static CLOCK: FakeClock = FakeClock;

    struct FakePower;
    impl Power for FakePower {
        fn system_off(&self) -> ! {
            loop {}
        }
        fn system_reset(&self) -> ! {
            loop {}
        }
    }
    static POWER: FakePower = FakePower;

    struct FakeCpu;
    impl Cpu for FakeCpu {
        fn current_el(&self) -> u64 {
            1
        }
        fn midr(&self) -> u64 {
            0
        }
        fn halt(&self) -> ! {
            loop {}
        }
    }
    static CPU: FakeCpu = FakeCpu;

    struct FakeProcesses;
    impl Processes for FakeProcesses {
        fn spawn(&self, _app: &'static dyn App, _args: &str) -> Result<u32, SpawnError> {
            Ok(2)
        }
        fn kill(&self, _pid: u32) -> bool {
            false
        }
        fn current(&self) -> u32 {
            1
        }
        fn snapshot(&self, out: &mut [ProcessInfo]) -> usize {
            if out.is_empty() {
                0
            } else {
                let _ = &mut out[0];
                0
            }
        }
    }
    static PROCESSES: FakeProcesses = FakeProcesses;

    struct Registry;
    impl AppRegistry for Registry {
        fn find(&self, _name: &str) -> Option<&'static dyn App> {
            None
        }
    }
    static REGISTRY: Registry = Registry;

    #[test]
    fn desktop_terminal_receives_keyboard_events() {
        let bundle = include_bytes!("../../desktop/apps/terminal.fapp");
        let entries = [feos_bundle::DirEntry::new("terminal.fapp", bundle)];
        let mut desktop = std::boxed::Box::new(feos_desktop::Desktop::<4, 4, 2>::default());
        desktop.scan(&entries);

        assert!(matches!(
            desktop.launch(0, &crate::APP_REGISTRY, &PROCESSES),
            feos_desktop::LaunchOutcome::Windowed { window: 0 }
        ));

        let platform = test_platform();
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        desktop.render_with_platform(&mut canvas, &Input::new(), 100, Some(&platform));

        let mut input = Input::new();
        let mut keys = feos_keys::SerialKeys::new();
        for byte in b"echo hi\r" {
            keys.feed(*byte, 10, &mut input);
        }
        desktop.render_with_platform(&mut canvas, &input, 200, Some(&platform));

        let guard = TERMINAL.lock();
        let state = guard.as_ref().expect("terminal state");
        assert!(
            state.lines.iter().any(|line| line == "hi"),
            "terminal output was {:?}",
            state.lines
        );
        drop(guard);
        Terminal.close_window();
    }

    fn test_platform() -> Platform {
        Platform {
            board: BoardInfo {
                name: "test",
                console_base: 0,
                framebuffer_addr: 0,
                kernel_range: (0, 0),
                bss_range: (0, 0),
            },
            clock: &CLOCK,
            power: &POWER,
            cpu: &CPU,
            audio: None,
            pointer: None,
            keyboard: None,
            processes: &PROCESSES,
            memory: None,
            apps: &REGISTRY,
            filesystem: None,
        }
    }

    #[test]
    fn terminal_accepts_text_and_executes_it() {
        let mut pixels = [0u32; 400 * 200];
        let platform = Platform {
            board: BoardInfo {
                name: "test",
                console_base: 0,
                framebuffer_addr: 0,
                kernel_range: (0, 0),
                bss_range: (0, 0),
            },
            clock: &CLOCK,
            power: &POWER,
            cpu: &CPU,
            audio: None,
            pointer: None,
            keyboard: None,
            processes: &PROCESSES,
            memory: None,
            apps: &REGISTRY,
            filesystem: None,
        };
        let mut canvas = Canvas::new(&mut pixels, 400, 200, 400).unwrap();
        let input = Input::new();
        let context = WindowContext {
            args: "",
            canvas: &mut canvas,
            input: &input,
            platform: &platform,
            now_ms: 0,
            origin: Point::new(0, 0),
            pointer_active: false,
            keyboard_active: true,
        };
        assert!(Terminal.open_window(&context));

        let mut input = Input::new();
        input.push(Event::Text('e'));
        input.push(Event::Text('c'));
        input.push(Event::Text('h'));
        input.push(Event::Text('o'));
        input.push(Event::Text(' '));
        input.push(Event::Text('h'));
        input.push(Event::Text('i'));
        input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
        let mut context = WindowContext {
            args: "",
            canvas: &mut canvas,
            input: &input,
            platform: &platform,
            now_ms: 10,
            origin: Point::new(0, 0),
            pointer_active: false,
            keyboard_active: true,
        };
        let _ = Terminal.frame_window(&mut context);

        let guard = TERMINAL.lock();
        let state = guard.as_ref().expect("terminal state");
        assert!(
            state.lines.iter().any(|line| line == "hi"),
            "terminal output was {:?}",
            state.lines
        );
    }
}
