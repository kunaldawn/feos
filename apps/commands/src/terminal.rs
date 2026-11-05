//! A windowed terminal for the desktop.
//!
//! The serial console and this window share the same `feos-sh` engine: the same
//! parser, builtins, coreutils and line editor. Each window keeps its own shell
//! instance, feeds everything printed through a terminal emulator, and renders
//! that emulator's grid with the 8x8 bitmap font.
//!
//! # Who runs what
//!
//! The desktop thread draws the window and edits the line being typed. Commands
//! run on a task of the window's own, so a command that never finishes costs that
//! window nothing but its prompt, and the desktop stays responsive.
//!
//! - What the task prints reaches the window through an output pipe, and the
//!   window drains it each frame.
//! - Keys typed while a command runs reach it through an input pipe, in the bytes
//!   a terminal would send. The command's end of that pipe has a small line
//!   discipline, unless the command asked for raw keys with `Input::set_raw`.
//! - Ctrl-C trips the shell's cancel token, and the shell kills the command.
//!
//! The shell sits behind a mutex that the task holds while a command runs. While
//! none does, the desktop thread may take it for history and completion.
//!
//! With no kernel to give it a task (host tests) or no free process slot, the
//! window runs commands on the desktop thread, with a watchdog.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use feos_app::pipe::{self, PipeReader, PipeWriter, Transfer};
use feos_app::{shell as session, App, Context, Platform, SpawnIo, WindowContext, WindowFrame};
use feos_coreutils::{BytesInput, Input, NoSystem, Output, System};
use feos_gfx::{Canvas, Color, Rect};
use feos_gui::{Event, Key, Modifiers};
use feos_hal::Clock;
use feos_keys::{key_press_from_gui, KeyCode, KeyPress};
use feos_sh::line::{render_line, rows_below, EditEvent, LineEditor};
use feos_sh::vt::{self, Vt, ATTR_BOLD, ATTR_INVERSE, ATTR_UNDERLINE, DEFAULT_COLOR};
use feos_sh::{CancelToken, ConsoleStreams, Host, Io, Shell};
use feos_sync::{futex_wait, futex_wake, Mutex, SpinLock};

use crate::host::{self, ConsoleSource, ProcessHost};

/// Longest a command may hold the desktop thread before it is cancelled, when
/// it has to run there.
const WATCHDOG_MS: u64 = 5_000;

/// Most output kept from one command run on the desktop thread. Anything past
/// this stops the command, and only the scrollback's worth would have stayed on
/// screen anyway.
const MAX_COMMAND_OUTPUT: usize = 4 * 1024 * 1024;

/// Most output a window takes in per frame, so a chatty command can't keep the
/// desktop thread from drawing.
const MAX_DRAIN: usize = 32 * 1024;

/// Screen size before the first frame reports the real one.
const INITIAL_COLUMNS: usize = 80;
const INITIAL_ROWS: usize = 24;

/// Windows the desktop can have open at once.
const INSTANCES: usize = 8;

static TERMINALS: [SpinLock<Option<TerminalState>>; INSTANCES] =
    [const { SpinLock::new(None) }; INSTANCES];

/// Windows with something on the way, which therefore need frames.
static ACTIVE: AtomicU32 = AtomicU32::new(0);

static NO_SYSTEM: NoSystem = NoSystem;

/// The window's command host when commands have to run on the desktop thread.
/// There are no external commands here, because they would stall it.
#[derive(Default)]
struct WindowHost {
    clock: Option<&'static dyn Clock>,
    deadline_ms: u64,
    expired: bool,
}

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

    fn poll_cancel(&mut self) -> bool {
        let Some(clock) = self.clock else {
            return false;
        };
        if clock.uptime_ms() >= self.deadline_ms {
            self.expired = true;
            return true;
        }
        false
    }
}

/// Collects a command's stdout and stderr in the order they were written.
struct SharedOutput<'a> {
    buffer: &'a RefCell<Vec<u8>>,
}

impl Output for SharedOutput<'_> {
    fn write(&mut self, bytes: &[u8]) {
        let mut buffer = self.buffer.borrow_mut();
        if buffer.len() + bytes.len() <= MAX_COMMAND_OUTPUT && buffer.try_reserve(bytes.len()).is_ok()
        {
            buffer.extend_from_slice(bytes);
        }
    }

    fn is_broken(&self) -> bool {
        self.buffer.borrow().len() >= MAX_COMMAND_OUTPUT
    }
}

/// What a window and its task say to each other.
struct Link {
    /// The next command line, waiting for the task.
    job: SpinLock<Option<String>>,
    /// Changes when there's a job for the task, or it should look at `closing`.
    jobs: AtomicU32,
    /// A command is running, or waiting for the task to take it.
    busy: AtomicBool,
    /// The window has gone, so the task should too.
    closing: AtomicBool,
    /// The running command asked for raw keys.
    raw: AtomicBool,
    /// The shell asked to exit, and with what, offset by one: 0 means it hasn't.
    exit: AtomicI32,
}

impl Link {
    fn new() -> Self {
        Self {
            job: SpinLock::new(None),
            jobs: AtomicU32::new(0),
            busy: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            raw: AtomicBool::new(false),
            exit: AtomicI32::new(0),
        }
    }

    fn poke(&self) {
        self.jobs.fetch_add(1, Ordering::Release);
        futex_wake(&self.jobs, usize::MAX);
    }
}

/// A window's view of the task running its commands.
struct Worker {
    link: Arc<Link>,
    /// Everything commands print.
    output: PipeReader,
    /// Keys while a command runs.
    input: PipeWriter,
    /// Reads from `input` end here too: lets Ctrl-C wake a blocked read.
    input_waker: PipeReader,
    cancel: CancelToken,
}

/// Standard output and error of the task: the window's output pipe.
struct PipeOutput {
    writer: PipeWriter,
}

impl Output for PipeOutput {
    fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write(bytes);
    }

    fn is_terminal(&self) -> bool {
        true
    }

    fn is_broken(&self) -> bool {
        self.writer.is_broken()
    }

    fn adopt(&self) {
        self.writer.adopt();
    }
}

/// Standard input of the task: keys from the window through a line discipline.
///
/// A plain `read` gets cooked lines: typing is echoed, Enter ends a line,
/// Backspace and Ctrl-U edit it, and Ctrl-D is end of file. Raw reads see the
/// bytes as they came.
struct KeyInput {
    reader: PipeReader,
    echo: PipeWriter,
    link: Arc<Link>,
    cancel: CancelToken,
    line: Vec<u8>,
    position: usize,
}

impl KeyInput {
    fn stopped(&self) -> bool {
        self.cancel.is_cancelled() || self.link.closing.load(Ordering::Acquire)
    }

    fn cook(&mut self) -> bool {
        self.line.clear();
        self.position = 0;
        let mut byte = [0u8; 1];
        loop {
            if self.read_raw(&mut byte) == 0 {
                return !self.line.is_empty();
            }
            match byte[0] {
                b'\r' | b'\n' => {
                    self.line.push(b'\n');
                    let _ = self.echo.write(b"\n");
                    return true;
                }
                0x04 if self.line.is_empty() => return false,
                0x04 => return true,
                0x7f | 0x08 => {
                    let mut removed = false;
                    while let Some(last) = self.line.pop() {
                        removed = true;
                        if last & 0xc0 != 0x80 {
                            break;
                        }
                    }
                    if removed {
                        let _ = self.echo.write(b"\x08 \x08");
                    }
                }
                0x15 => {
                    while let Some(last) = self.line.pop() {
                        if last & 0xc0 != 0x80 {
                            let _ = self.echo.write(b"\x08 \x08");
                        }
                    }
                }
                byte if byte >= 0x20 && self.line.len() < 4096 => {
                    self.line.push(byte);
                    let _ = self.echo.write(&[byte]);
                }
                _ => {}
            }
        }
    }
}

impl Input for KeyInput {
    fn read(&mut self, out: &mut [u8]) -> usize {
        if self.position >= self.line.len() && !self.cook() {
            return 0;
        }
        let rest = &self.line[self.position..];
        let count = rest.len().min(out.len());
        out[..count].copy_from_slice(&rest[..count]);
        self.position += count;
        count
    }

    fn read_raw(&mut self, out: &mut [u8]) -> usize {
        loop {
            let seen = self.reader.change_count();
            match self.reader.try_read(out) {
                Transfer::Moved(count) => return count,
                Transfer::Closed => return 0,
                Transfer::WouldBlock => {}
            }
            if self.stopped() {
                return 0;
            }
            self.reader.wait_for_change(seen);
        }
    }

    fn read_pending(&mut self, out: &mut [u8]) -> usize {
        match self.reader.try_read(out) {
            Transfer::Moved(count) => count,
            _ => 0,
        }
    }

    fn is_terminal(&self) -> bool {
        true
    }

    fn set_raw(&mut self, raw: bool) {
        self.link.raw.store(raw, Ordering::Release);
    }

    fn adopt(&self) {
        self.reader.adopt();
        self.echo.adopt();
    }
}

/// Streams for a window's forked shells, on the same pipes as its own.
struct WindowConsole {
    keys: PipeReader,
    output: PipeWriter,
    link: Arc<Link>,
    cancel: CancelToken,
}

impl ConsoleSource for WindowConsole {
    fn streams(&self) -> ConsoleStreams {
        // The clone only fails once the window's pipe has gone, and then there's
        // nowhere for output to go.
        let output = || -> alloc::boxed::Box<dyn Output + Send> {
            match self.output.try_clone() {
                Some(writer) => alloc::boxed::Box::new(PipeOutput { writer }),
                None => alloc::boxed::Box::new(feos_coreutils::NullOutput),
            }
        };
        // Sharing the read end means sharing what's typed, as on any terminal.
        let input: alloc::boxed::Box<dyn Input + Send> = match (self.keys.try_clone(), self.output.try_clone()) {
            (Some(reader), Some(echo)) => alloc::boxed::Box::new(KeyInput {
                reader,
                echo,
                link: self.link.clone(),
                cancel: self.cancel.clone(),
                line: Vec::new(),
                position: 0,
            }),
            _ => alloc::boxed::Box::new(feos_coreutils::NullInput),
        };
        (input, output(), output())
    }
}

/// The body of a window's task: runs lines until the window goes.
fn worker_task(
    platform: &'static Platform,
    shell: Arc<Mutex<Shell<'static>>>,
    link: Arc<Link>,
    cancel: CancelToken,
    mut stdin: KeyInput,
    mut stdout: PipeOutput,
    mut stderr: PipeOutput,
) -> i32 {
    stdin.reader.adopt();
    stdin.echo.adopt();
    stdout.writer.adopt();
    stderr.writer.adopt();
    let (Some(keys), Some(output)) = (stdin.reader.try_clone(), stdout.writer.try_clone()) else {
        return 1;
    };
    let console = Arc::new(WindowConsole {
        keys,
        output,
        link: link.clone(),
        cancel: cancel.clone(),
    });
    let mut host = ProcessHost::detached(platform, cancel, console);
    loop {
        let seen = link.jobs.load(Ordering::Acquire);
        if link.closing.load(Ordering::Acquire) || platform.processes.kill_requested() {
            return 0;
        }
        let Some(line) = link.job.lock().take() else {
            futex_wait(&link.jobs, seen);
            continue;
        };

        let exit = {
            let mut shell = shell.lock();
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
            };
            shell.execute_line(&line, &mut io, &mut host);
            shell.take_exit_status()
        };
        link.raw.store(false, Ordering::Release);
        if let Some(code) = exit {
            link.exit.store(code.wrapping_add(1).max(1), Ordering::Release);
        }
        link.busy.store(false, Ordering::Release);
    }
}

struct TerminalState {
    shell: Arc<Mutex<Shell<'static>>>,
    vt: Vt,
    editor: LineEditor,
    /// How far below the prompt's row the editor left the cursor.
    edit_row: usize,
    scroll: usize,
    close_requested: bool,
    scale: u32,
    worker: Option<Worker>,
    /// A command was handed to the task and its prompt hasn't come back yet.
    running: bool,
    /// The window changed size while a command held the shell.
    size_changed: bool,
    /// Counted in [`ACTIVE`].
    active: bool,
}

impl TerminalState {
    fn new(filesystem: Option<&'static dyn feos_fs::FileSystem>) -> Self {
        Self::with_system(filesystem, &NO_SYSTEM)
    }

    fn with_system(
        filesystem: Option<&'static dyn feos_fs::FileSystem>,
        system: &'static dyn System,
    ) -> Self {
        let mut shell = Shell::new(filesystem, system);
        shell.env.set("TERM", "xterm");
        let editor = LineEditor::new(shell.history.len());
        let mut state = Self {
            shell: Arc::new(Mutex::new(shell)),
            vt: Vt::new(INITIAL_COLUMNS, INITIAL_ROWS),
            editor,
            edit_row: 0,
            scroll: 0,
            close_requested: false,
            scale: 1,
            worker: None,
            running: false,
            size_changed: false,
            active: false,
        };
        state.set_screen_variables();
        state.vt.feed(b"FEOS terminal - Bash-compatible shell\n");
        state
            .vt
            .feed(b"Type 'help' for commands. Ctrl+/- zooms, Ctrl+Shift+V pastes, Ctrl-D closes.\n\n");
        state.print_prompt();
        state
    }

    /// Gives the window a task of its own. Without one, commands run on the
    /// desktop thread.
    fn start_worker(&mut self, platform: &'static Platform) {
        let (Some((output, out_writer)), Some((key_reader, input))) = (pipe::create(), pipe::create())
        else {
            return;
        };
        let (Some(err_writer), Some(echo), Some(console), Some(input_waker)) = (
            out_writer.try_clone(),
            out_writer.try_clone(),
            out_writer.try_clone(),
            key_reader.try_clone(),
        ) else {
            return;
        };
        let link = Arc::new(Link::new());
        let cancel = self.shell.lock().cancel_token();
        let stdin = KeyInput {
            reader: key_reader,
            echo,
            link: link.clone(),
            cancel: cancel.clone(),
            line: Vec::new(),
            position: 0,
        };
        let stdout = PipeOutput { writer: out_writer };
        let stderr = PipeOutput { writer: err_writer };
        let (task_shell, task_link, task_cancel) = (self.shell.clone(), link.clone(), cancel.clone());
        let io = SpawnIo {
            stdout: Some(&console),
            stderr: Some(&console),
            console: Some(&console),
            background: true,
            ..SpawnIo::default()
        };
        let spawned = platform.processes.spawn_task(
            "terminal",
            io,
            alloc::boxed::Box::new(move || {
                worker_task(platform, task_shell, task_link, task_cancel, stdin, stdout, stderr)
            }),
        );
        if spawned.is_ok() {
            self.worker = Some(Worker {
                link,
                output,
                input,
                input_waker,
                cancel,
            });
        }
    }

    /// Takes in what commands printed. Returns `true` if anything arrived.
    fn drain(&mut self) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        let mut buffer = [0u8; 2048];
        let mut taken = 0;
        while taken < MAX_DRAIN {
            match worker.output.try_read(&mut buffer) {
                Transfer::Moved(count) => {
                    self.vt.feed(&buffer[..count]);
                    taken += count;
                }
                _ => break,
            }
        }
        if taken > 0 {
            self.scroll = 0;
        }
        taken > 0
    }

    /// Notices that the task finished the command, and shows a new prompt.
    fn poll_worker(&mut self) {
        self.drain();
        if !self.running {
            return;
        }
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        if worker.link.busy.load(Ordering::Acquire) {
            return;
        }
        // The command is done, but what it printed may still be in the pipe.
        if matches!(worker.output.peek(), Transfer::Moved(_)) {
            return;
        }
        let exit = worker.link.exit.load(Ordering::Acquire);
        self.running = false;
        if exit != 0 {
            self.close_requested = true;
            return;
        }
        if self.size_changed {
            self.size_changed = false;
            self.set_screen_variables();
        }
        self.finish_command();
    }

    fn set_screen_variables(&mut self) {
        let (cols, rows) = (self.vt.cols(), self.vt.rows());
        let mut shell = self.shell.lock();
        shell.env.set("COLUMNS", &feos_text_number(cols));
        shell.env.set("LINES", &feos_text_number(rows));
    }

    fn print_prompt(&mut self) {
        let prompt = self.prompt();
        self.vt.feed(prompt.as_bytes());
        self.edit_row = 0;
        self.redraw_line();
    }

    fn prompt(&self) -> String {
        self.editor.prompt(&self.shell.lock())
    }

    /// Redraws the input with the same sequences the serial editor sends.
    fn redraw_line(&mut self) {
        let prompt = self.prompt();
        let (bytes, row) = render_line(
            &prompt,
            self.editor.text(),
            self.editor.cursor(),
            self.vt.cols(),
            self.edit_row,
        );
        self.vt.feed(&bytes);
        self.edit_row = row;
    }

    /// Moves the cursor below the input, wherever it is in the line.
    fn leave_line(&mut self) {
        let prompt = self.prompt();
        let below = rows_below(&prompt, self.editor.text(), self.vt.cols());
        for _ in self.edit_row..below {
            self.vt.feed(b"\n");
        }
        self.vt.feed(b"\n");
        self.edit_row = 0;
    }

    fn execute(&mut self, line: &str, clock: Option<&'static dyn Clock>) {
        feos_sh::remember(&mut self.shell.lock(), line);

        if let Some(worker) = self.worker.as_ref() {
            worker.link.exit.store(0, Ordering::Release);
            *worker.link.job.lock() = Some(String::from(line));
            worker.link.busy.store(true, Ordering::Release);
            worker.link.poke();
            self.running = true;
            return;
        }

        let mut host = WindowHost {
            clock,
            deadline_ms: clock.map_or(0, |clock| clock.uptime_ms()) + WATCHDOG_MS,
            expired: false,
        };
        let shared = RefCell::new(Vec::new());
        let mut stdin = BytesInput::empty();
        let mut stdout = SharedOutput { buffer: &shared };
        let mut stderr = SharedOutput { buffer: &shared };
        let exit = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
            };
            let mut shell = self.shell.lock();
            shell.execute_line(line, &mut io, &mut host);
            shell.take_exit_status()
        };
        self.vt.feed(&shared.into_inner());
        if host.expired {
            self.vt.feed(b"\n[stopped: commands here may run for 5 seconds]\n");
        }

        if exit.is_some() {
            self.editor.reset(self.shell.lock().history.len());
            self.scroll = 0;
            self.close_requested = true;
            return;
        }
        self.finish_command();
    }

    /// Readies the line editor for the next command.
    fn finish_command(&mut self) {
        let history = self.shell.lock().history.len();
        self.editor.reset(history);
        self.scroll = 0;
        // A prompt never starts in the middle of a line.
        if self.vt.cursor().0 != 0 {
            self.vt.feed(b"\n");
        }
        self.print_prompt();
    }

    /// Sends bytes to the running command, as keys.
    fn send_keys(&mut self, bytes: &[u8]) {
        if let Some(worker) = self.worker.as_ref() {
            // The queue is short only if the command isn't reading; dropping keys
            // beats stalling the desktop.
            let _ = worker.input.try_write(bytes);
        }
    }

    /// Ctrl-C while a command runs.
    fn interrupt(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        worker.cancel.cancel();
        worker.input_waker.wake();
        self.vt.feed(b"^C\n");
    }

    /// Sends one key to the line editor, or the running command, and shows the
    /// result.
    fn press(&mut self, key: KeyPress, clock: Option<&'static dyn Clock>) {
        self.scroll = 0;
        if self.running {
            let raw = self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.link.raw.load(Ordering::Acquire));
            if !raw && key == KeyPress::ctrl('c') {
                self.interrupt();
            } else if let Some(bytes) = key_bytes(key) {
                self.send_keys(bytes.as_str().as_bytes());
            }
            return;
        }

        let host = WindowHost::default();
        let event = self.editor.handle(key, &mut self.shell.lock(), &host);
        match event {
            EditEvent::None => {}
            EditEvent::Changed => self.redraw_line(),
            EditEvent::Submit(line) => {
                self.leave_line();
                self.execute(&line, clock);
            }
            EditEvent::Continuation => {
                // The editor has already moved on, so its text is empty.
                self.vt.feed(b"\n");
                self.print_prompt();
            }
            EditEvent::Interrupted => {
                self.leave_line();
                self.vt.feed(b"^C\n");
                let history = {
                    let mut shell = self.shell.lock();
                    shell.last_status = 130;
                    shell.history.len()
                };
                self.editor.reset(history);
                self.print_prompt();
            }
            EditEvent::Eof => self.close_requested = true,
            EditEvent::ClearScreen => {
                self.vt.clear_all();
                self.print_prompt();
            }
            EditEvent::Candidates(candidates) => {
                self.leave_line();
                let mut line = String::new();
                for candidate in candidates {
                    if !line.is_empty() {
                        line.push_str("  ");
                    }
                    line.push_str(&candidate);
                }
                line.push('\n');
                self.vt.feed(line.as_bytes());
                self.print_prompt();
            }
        }
    }

    fn zoom_in(&mut self) {
        if self.scale < MAX_SCALE {
            self.scale += 1;
            self.scroll = 0;
        }
    }

    fn zoom_out(&mut self) {
        if self.scale > MIN_SCALE {
            self.scale -= 1;
            self.scroll = 0;
        }
    }

    /// Matches the screen to the window, keeping the input line intact.
    fn fit(&mut self, cols: usize, rows: usize) {
        if cols == self.vt.cols() && rows == self.vt.rows() {
            return;
        }
        self.vt.resize(cols, rows);
        if self.running {
            self.size_changed = true;
        } else {
            self.set_screen_variables();
        }
    }

    /// Types the clipboard, one key per character. Lines are entered as they
    /// would be by hand, so a pasted script runs as it arrives.
    fn paste(&mut self, clock: Option<&'static dyn Clock>) {
        let mut buffer = [0u8; session::CLIPBOARD_MAX];
        let text = session::clipboard_get(&mut buffer);
        for ch in text.chars() {
            match ch {
                '\n' | '\r' => self.press(KeyPress::plain(KeyCode::Enter), clock),
                '\t' => self.press(KeyPress::plain(KeyCode::Tab), clock),
                ch if ch.is_control() => {}
                ch => self.press(KeyPress::plain(KeyCode::Char(ch)), clock),
            }
        }
    }

    /// Keeps [`ACTIVE`] right, so the desktop knows this window wants frames.
    fn update_active(&mut self) {
        let active = self.running
            || self
                .worker
                .as_ref()
                .is_some_and(|worker| matches!(worker.output.peek(), Transfer::Moved(_)));
        if active != self.active {
            self.active = active;
            if active {
                ACTIVE.fetch_add(1, Ordering::Relaxed);
            } else {
                ACTIVE.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        if self.active {
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(worker) = self.worker.as_ref() {
            worker.link.closing.store(true, Ordering::Release);
            worker.cancel.cancel();
            worker.input_waker.wake();
            worker.link.poke();
        }
    }
}

/// What a key looks like to a program reading a terminal.
fn key_bytes(key: KeyPress) -> Option<feos_text::Text<8>> {
    use core::fmt::Write as _;
    let mut text = feos_text::Text::<8>::new();
    let sequence = match key.code {
        KeyCode::Char(ch) if key.ctrl => {
            let code = (ch.to_ascii_lowercase() as u32).wrapping_sub('a' as u32 - 1);
            if !(1..=26).contains(&code) {
                return None;
            }
            let _ = text.write_char(code as u8 as char);
            return Some(text);
        }
        KeyCode::Char(ch) => {
            let _ = text.write_char(ch);
            return Some(text);
        }
        KeyCode::Enter => "\r",
        KeyCode::Tab => "\t",
        KeyCode::Backspace => "\x7f",
        KeyCode::Delete => "\x1b[3~",
        KeyCode::Escape => "\x1b",
        KeyCode::Up => "\x1b[A",
        KeyCode::Down => "\x1b[B",
        KeyCode::Right => "\x1b[C",
        KeyCode::Left => "\x1b[D",
        KeyCode::Home => "\x1b[H",
        KeyCode::End => "\x1b[F",
        KeyCode::PageUp => "\x1b[5~",
        KeyCode::PageDown => "\x1b[6~",
        _ => return None,
    };
    let _ = text.write_str(sequence);
    Some(text)
}

fn feos_text_number(value: usize) -> String {
    alloc::format!("{value}")
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

    fn multi_instance(&self) -> bool {
        true
    }

    fn animates(&self) -> bool {
        ACTIVE.load(Ordering::Relaxed) > 0
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        let Some(slot) = TERMINALS.get(context.instance) else {
            return false;
        };
        let global = feos_app::platform();
        let mut state = match global {
            Some(platform) => TerminalState::with_system(context.platform.filesystem, host::system(platform)),
            None => TerminalState::new(context.platform.filesystem),
        };
        if let Some(platform) = global {
            state.start_worker(platform);
        }
        // `-c command` opens a window to run that command: the Run dialog's way
        // of showing a command's output.
        if let Some(command) = context.args.trim().strip_prefix("-c ") {
            state.vt.feed(command.trim().as_bytes());
            state.leave_line();
            state.execute(command.trim(), Some(context.platform.clock));
        }
        state.update_active();
        *slot.lock() = Some(state);
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        let Some(slot) = TERMINALS.get(context.instance) else {
            return WindowFrame::Close;
        };
        let mut guard = match slot.try_lock() {
            Some(guard) => guard,
            None => return WindowFrame::Continue,
        };
        let Some(state) = guard.as_mut() else {
            return WindowFrame::Close;
        };

        state.poll_worker();
        if context.keyboard_active {
            let events: Vec<Event> = context.input.events().collect();
            let has_text = events.iter().any(|event| matches!(event, Event::Text(_)));
            for event in events {
                handle_event(state, event, context, has_text);
            }
        }
        if context.pointer_active {
            for event in context.input.events() {
                if let Event::PointerWheel(delta) = event {
                    if delta > 0 {
                        let limit = state.vt.scrollback_len();
                        state.scroll = state.scroll.saturating_add(2).min(limit);
                    } else {
                        state.scroll = state.scroll.saturating_sub(2);
                    }
                }
            }
        }

        if state.close_requested {
            return WindowFrame::Close;
        }

        let cols = visible_columns(context.canvas, state.scale);
        let rows = visible_rows(context.canvas, state.scale);
        state.fit(cols, rows);
        state.update_active();
        render(state, context.canvas);
        WindowFrame::Continue
    }

    fn close_instance(&self, instance: usize) {
        if let Some(slot) = TERMINALS.get(instance) {
            *slot.lock() = None;
        }
    }

    fn close_window(&self) {
        for instance in 0..INSTANCES {
            self.close_instance(instance);
        }
    }

    fn background_tick(&self, _platform: &Platform, _now_ms: u64) {
        // A minimized window gets no frames, but its command mustn't stall.
        for slot in &TERMINALS {
            if let Some(mut guard) = slot.try_lock() {
                if let Some(state) = guard.as_mut() {
                    state.drain();
                }
            }
        }
    }

    fn run(&self, _context: &mut Context<'_>) -> i32 {
        feos_console::eprintln!(
            "terminal: open it from the desktop, or use the serial shell directly"
        );
        1
    }
}

fn handle_event(
    state: &mut TerminalState,
    event: Event,
    context: &WindowContext<'_, '_>,
    has_text: bool,
) {
    let clock = Some(context.platform.clock);
    match event {
        Event::Text(ch) if !ch.is_control() => {
            state.press(KeyPress::plain(KeyCode::Char(ch)), clock);
        }
        Event::KeyDown(key, modifiers) => {
            if modifiers.contains(Modifiers::CTRL) {
                match key {
                    Key::Character('=') | Key::Character('+') => return state.zoom_in(),
                    Key::Character('-') | Key::Character('_') => return state.zoom_out(),
                    Key::Character('0') => {
                        state.scale = MIN_SCALE;
                        return;
                    }
                    Key::Character('V') => return state.paste(clock),
                    Key::Character('v') if modifiers.contains(Modifiers::SHIFT) => {
                        return state.paste(clock)
                    }
                    _ => {}
                }
            }

            match key {
                // The serial decoder sends KeyDown and Text together; some
                // keyboard drivers send only KeyDown. Take the text from one or
                // the other, so neither source doubles characters.
                Key::Character(_) | Key::Space
                    if has_text
                        && !modifiers.contains(Modifiers::CTRL)
                        && !modifiers.contains(Modifiers::ALT) => {}
                Key::PageUp if !state.running => {
                    let rows = visible_rows(context.canvas, state.scale);
                    let limit = state.vt.scrollback_len();
                    state.scroll = state.scroll.saturating_add(rows / 2).min(limit);
                }
                Key::PageDown if !state.running => {
                    let rows = visible_rows(context.canvas, state.scale);
                    state.scroll = state.scroll.saturating_sub(rows / 2);
                }
                other => {
                    if let Some(press) = key_press_from_gui(other, modifiers) {
                        state.press(press, clock);
                    }
                }
            }
        }
        _ => {}
    }
}

fn visible_rows(canvas: &Canvas<'_>, scale: u32) -> usize {
    let line_height = BASE_LINE_HEIGHT as usize * scale as usize;
    (canvas.height().saturating_sub(8) / line_height).max(1)
}

fn visible_columns(canvas: &Canvas<'_>, scale: u32) -> usize {
    let cell_width = BASE_CELL_WIDTH as usize * scale as usize;
    (canvas.width().saturating_sub(8) / cell_width).max(1)
}

const BASE_CELL_WIDTH: i32 = 8;
const BASE_LINE_HEIGHT: i32 = 10;
const MIN_SCALE: u32 = 1;
const MAX_SCALE: u32 = 4;

const BACKGROUND: Color = Color::rgb(8, 12, 16);
const FOREGROUND: Color = Color::rgb(196, 220, 206);
const CURSOR: Color = Color::rgb(92, 222, 148);

fn cell_color(index: u8, default: Color, bold: bool) -> Color {
    if index == DEFAULT_COLOR {
        return default;
    }
    // Bold brightens the eight base colours, as terminals have always done.
    let index = if bold && index < 8 { index + 8 } else { index };
    let (r, g, b) = vt::palette(index);
    Color::rgb(r, g, b)
}

fn render(state: &TerminalState, canvas: &mut Canvas<'_>) {
    let width = canvas.width() as i32;
    let height = canvas.height() as i32;
    let scale = state.scale;
    let cell_width = BASE_CELL_WIDTH * scale as i32;
    let line_height = BASE_LINE_HEIGHT * scale as i32;

    canvas.fill_rect(
        Rect::new(0, 0, width.max(1) as u32, height.max(1) as u32),
        BACKGROUND,
    );

    let rows = state.vt.rows();
    let mut run = String::new();
    for row in 0..rows {
        let y = 4 + row as i32 * line_height;
        let cells = state.vt.visible_row(row, state.scroll);
        let mut index = 0usize;
        while index < cells.len() {
            // Neighbouring cells with one look draw as one run.
            let first = cells[index];
            let start = index;
            run.clear();
            while index < cells.len()
                && cells[index].fg == first.fg
                && cells[index].bg == first.bg
                && cells[index].attrs == first.attrs
            {
                run.push(cells[index].ch);
                index += 1;
            }

            let bold = first.attrs & ATTR_BOLD != 0;
            let mut fg = cell_color(first.fg, FOREGROUND, bold);
            let mut bg = cell_color(first.bg, BACKGROUND, false);
            let inverse = first.attrs & ATTR_INVERSE != 0;
            if inverse {
                core::mem::swap(&mut fg, &mut bg);
            }
            let x = 4 + start as i32 * cell_width;
            let run_width = (index - start) as i32 * cell_width;
            if first.bg != DEFAULT_COLOR || inverse {
                canvas.fill_rect(
                    Rect::new(x, y, run_width as u32, line_height as u32),
                    bg,
                );
            }
            let text = run.trim_end_matches(' ');
            if !text.is_empty() {
                canvas.draw_text(x, y, text, fg, scale);
            }
            if first.attrs & ATTR_UNDERLINE != 0 {
                canvas.fill_rect(
                    Rect::new(x, y + line_height - scale as i32, run_width as u32, scale),
                    fg,
                );
            }
        }
    }

    if state.scroll == 0 && state.vt.cursor_visible() {
        let (column, row) = state.vt.cursor();
        let underline = (scale * 2).max(2);
        canvas.fill_rect(
            Rect::new(
                4 + column as i32 * cell_width,
                4 + (row as i32 + 1) * line_height - underline as i32,
                cell_width as u32,
                underline,
            ),
            CURSOR,
        );
    }
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

        let guard = TERMINALS[0].lock();
        let state = guard.as_ref().expect("terminal state");
        assert!(
            state.vt.text_lines().iter().any(|line| line == "hi"),
            "terminal output was {:?}",
            state.vt.text_lines()
        );
        assert_eq!(state.scale, 1);
        drop(guard);

        let mut input = Input::new();
        let mut keys = feos_keys::SerialKeys::new();
        keys.feed(0x1d, 20, &mut input);
        desktop.render_with_platform(&mut canvas, &input, 300, Some(&platform));
        assert_eq!(TERMINALS[0].lock().as_ref().unwrap().scale, 2);

        let mut input = Input::new();
        let mut keys = feos_keys::SerialKeys::new();
        keys.feed(0x1f, 40, &mut input);
        desktop.render_with_platform(&mut canvas, &input, 400, Some(&platform));
        assert_eq!(TERMINALS[0].lock().as_ref().unwrap().scale, 1);

        Terminal.close_window();
    }

    fn test_platform() -> Platform {
        Platform {
            board: BoardInfo {
                name: "test",
                arch: "test",
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
            net: None,
        }
    }

    #[test]
    fn terminal_zoom_is_bounded() {
        let mut state = TerminalState::new(None);
        assert_eq!(state.scale, MIN_SCALE);
        for _ in 0..10 {
            state.zoom_in();
        }
        assert_eq!(state.scale, MAX_SCALE);
        for _ in 0..10 {
            state.zoom_out();
        }
        assert_eq!(state.scale, MIN_SCALE);
    }

    #[test]
    fn terminal_accepts_text_and_executes_it() {
        let mut pixels = [0u32; 400 * 200];
        let platform = Platform {
            board: BoardInfo {
                name: "test",
                arch: "test",
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
            net: None,
        };
        let mut canvas = Canvas::new(&mut pixels, 400, 200, 400).unwrap();
        let input = Input::new();
        let context = WindowContext {
            args: "",
            instance: 0,
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
            instance: 0,
            canvas: &mut canvas,
            input: &input,
            platform: &platform,
            now_ms: 10,
            origin: Point::new(0, 0),
            pointer_active: false,
            keyboard_active: true,
        };
        let _ = Terminal.frame_window(&mut context);

        let guard = TERMINALS[0].lock();
        let state = guard.as_ref().expect("terminal state");
        assert!(
            state.vt.text_lines().iter().any(|line| line == "hi"),
            "terminal output was {:?}",
            state.vt.text_lines()
        );
    }

    fn typed(state: &mut TerminalState, text: &str) {
        for ch in text.chars() {
            state.press(KeyPress::plain(KeyCode::Char(ch)), None);
        }
    }

    fn enter(state: &mut TerminalState) {
        state.press(KeyPress::plain(KeyCode::Enter), None);
    }

    #[test]
    fn the_window_prints_stdout_and_stderr_in_order() {
        let mut state = TerminalState::new(None);
        typed(&mut state, "echo one; cat /missing; echo two");
        enter(&mut state);
        let lines = state.vt.text_lines();
        let one = lines.iter().position(|line| line == "one").unwrap();
        let error = lines.iter().position(|line| line.starts_with("cat:")).unwrap();
        let two = lines.iter().position(|line| line == "two").unwrap();
        assert!(one < error && error < two, "{lines:?}");
    }

    #[test]
    fn the_window_continues_multi_line_commands() {
        let mut state = TerminalState::new(None);
        typed(&mut state, "for i in 1 2; do");
        enter(&mut state);
        assert!(state.vt.text_lines().iter().any(|line| line == ">"));
        typed(&mut state, "echo n$i; done");
        enter(&mut state);
        let lines = state.vt.text_lines();
        assert!(lines.iter().any(|line| line == "n1"), "{lines:?}");
        assert!(lines.iter().any(|line| line == "n2"), "{lines:?}");
    }

    #[test]
    fn escape_does_not_close_the_window() {
        let mut state = TerminalState::new(None);
        state.press(KeyPress::plain(KeyCode::Escape), None);
        assert!(!state.close_requested);
        state.press(KeyPress::ctrl('d'), None);
        assert!(state.close_requested);
    }

    #[test]
    fn exit_closes_the_window_but_a_child_shell_does_not() {
        let mut state = TerminalState::new(None);
        typed(&mut state, "sh -c 'exit 3'");
        enter(&mut state);
        assert!(!state.close_requested);
        typed(&mut state, "exit");
        enter(&mut state);
        assert!(state.close_requested);
    }

    #[test]
    fn long_input_wraps_and_stays_visible() {
        let mut state = TerminalState::new(None);
        state.fit(20, 10);
        typed(&mut state, &"x".repeat(50));
        let lines = state.vt.text_lines();
        assert_eq!(lines.iter().map(|line| line.matches('x').count()).sum::<usize>(), 50);
        let (column, _) = state.vt.cursor();
        assert!(column < 20);
    }

    #[test]
    fn output_keeps_colours_and_tab_stops() {
        let mut state = TerminalState::new(None);
        typed(&mut state, "printf 'a\\tb\\033[31mc\\033[0m\\n'");
        enter(&mut state);
        let lines = state.vt.text_lines();
        assert!(lines.iter().any(|line| line == "a       bc"), "{lines:?}");
    }

    #[test]
    fn scrollback_is_capped() {
        let mut state = TerminalState::new(None);
        typed(&mut state, "seq 1 5000");
        enter(&mut state);
        assert!(state.vt.scrollback_len() <= feos_sh::vt::MAX_SCROLLBACK);
    }

    #[test]
    fn a_runaway_command_is_stopped_by_the_watchdog() {
        struct JumpingClock(core::sync::atomic::AtomicU64);
        impl Clock for JumpingClock {
            fn frequency(&self) -> u64 {
                1_000
            }
            fn ticks(&self) -> u64 {
                self.0.fetch_add(100, Ordering::Relaxed)
            }
            fn spin_ms(&self, _ms: u64) {}
        }
        static JUMPING: JumpingClock = JumpingClock(core::sync::atomic::AtomicU64::new(0));

        let mut state = TerminalState::new(None);
        for ch in "while :; do while :; do :; done; done".chars() {
            state.press(KeyPress::plain(KeyCode::Char(ch)), Some(&JUMPING));
        }
        state.press(KeyPress::plain(KeyCode::Enter), Some(&JUMPING));
        assert!(state
            .vt
            .text_lines()
            .iter()
            .any(|line| line.contains("stopped")));
        assert_eq!(state.shell.lock().last_status, 130);
    }

    #[test]
    fn keys_turn_into_what_a_terminal_sends() {
        let bytes = |key| key_bytes(key).unwrap();
        assert_eq!(bytes(KeyPress::plain(KeyCode::Char('é'))).as_str(), "é");
        assert_eq!(bytes(KeyPress::ctrl('c')).as_str(), "\x03");
        assert_eq!(bytes(KeyPress::plain(KeyCode::Enter)).as_str(), "\r");
        assert_eq!(bytes(KeyPress::plain(KeyCode::Up)).as_str(), "\x1b[A");
    }

    #[test]
    fn pasting_types_the_clipboard_into_the_line() {
        let mut state = TerminalState::new(None);
        assert!(session::clipboard_set("echo pas\ted\necho two"));
        state.paste(None);
        let lines = state.vt.text_lines();
        assert!(lines.iter().any(|line| line.contains("echo two")), "{lines:?}");
        enter(&mut state);
        let lines = state.vt.text_lines();
        assert!(lines.iter().any(|line| line == "two"), "{lines:?}");
    }

    #[test]
    fn each_window_has_its_own_shell() {
        let mut first = TerminalState::new(None);
        let mut second = TerminalState::new(None);
        typed(&mut first, "x=first");
        enter(&mut first);
        typed(&mut second, "echo [$x]");
        enter(&mut second);
        assert!(second.vt.text_lines().iter().any(|line| line == "[]"));
    }

    #[test]
    fn prompt_width_matches_what_is_drawn() {
        let state = TerminalState::new(None);
        let prompt = state.shell.lock().prompt();
        assert_eq!(state.vt.cursor().0, feos_sh::line::prompt_width(&prompt));
    }
}
