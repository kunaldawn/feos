//! Runs a shell's external commands as processes.
//!
//! The serial shell and every Terminal window share this. A command runs as a
//! process of its own whose console is a pair of pipes, and the shell copies what
//! arrives to wherever the command's streams point. That is what makes
//! `ps | grep shell`, `$(ps)` and `ps > /tmp/out` work with no help from the
//! command, and lets Ctrl-C kill a command that is stuck.
//!
//! An app that draws on the framebuffer can't run in a process of its own. The
//! serial shell runs one on its own thread; a window asks the desktop to open it.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicPtr, Ordering};

use feos_app::pipe::{self, PipeReader, PipeWriter, Transfer};
use feos_app::{
    shell, App, ArgBlock, Context, Platform, ProcessInfo, SpawnError, SpawnIo, WaitStatus, MAX_ARGC,
    MAX_PROCESSES, PROCESS_ARGS_MAX,
};
use feos_coreutils::{Available, Input, MemoryInfo, Output, System};
use feos_gfx::Canvas;
use feos_sh::{CancelToken, ConsoleStreams, Host, JobStatus, TaskHost};

/// How long the copy loop sleeps when both streams were empty.
const IDLE_MS: u64 = 1;
/// Yields before the copy loop sleeps at all. A fast producer on another CPU
/// usually has more ready by then, and a sleep lasts until the next tick.
const SPINS: u32 = 8;

/// The system facts `uname`, `uptime`, `free` and friends report.
pub struct PlatformSystem {
    platform: &'static Platform,
}

impl System for PlatformSystem {
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
        self.platform.board.arch
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

    fn unix_time(&self) -> Option<u64> {
        self.platform.clock.unix_time()
    }

    fn uptime_ms(&self) -> u64 {
        self.platform.clock.uptime_ms()
    }

    fn clock_hz(&self) -> u64 {
        self.platform.clock.frequency()
    }

    fn cpu_count(&self) -> usize {
        self.platform.processes.system().cpus.max(1)
    }

    fn pid(&self) -> u32 {
        self.platform.processes.current()
    }

    fn memory(&self) -> Option<MemoryInfo> {
        self.platform.memory.map(|memory| {
            let info = memory.info();
            MemoryInfo {
                total: info.total_bytes,
                used: info.used_bytes,
                free: info.free_bytes,
            }
        })
    }
}

/// The one [`PlatformSystem`], allocated the first time it is asked for.
pub fn system(platform: &'static Platform) -> &'static dyn System {
    static SYSTEM: AtomicPtr<PlatformSystem> = AtomicPtr::new(core::ptr::null_mut());
    let mut known = SYSTEM.load(Ordering::Acquire);
    if known.is_null() {
        // Losing a race leaks one small allocation, once.
        known = Box::into_raw(Box::new(PlatformSystem { platform }));
        SYSTEM.store(known, Ordering::Release);
    }
    // SAFETY: the pointer came from a `Box` that is never freed.
    unsafe { &*known }
}

/// The framebuffer, for a shell that may run apps which draw on it.
pub struct DisplayAccess<'d> {
    /// The display an app draws on.
    pub display: &'d mut Option<Canvas<'static>>,
}

/// Decides what a stream's blocked reader or writer should give up for.
#[derive(Clone)]
struct Stop {
    platform: &'static Platform,
    cancel: CancelToken,
    serial: bool,
}

impl Stop {
    fn requested(&self) -> bool {
        // Ctrl-C at the console stops whatever the shell is running.
        if self.serial && feos_sh::console::poll_interrupt() {
            self.cancel.cancel();
        }
        self.cancel.is_cancelled() || self.platform.processes.kill_requested()
    }

    /// Waits a moment for a pipe, briefly busy in case it is about to change.
    fn pause(&self, idle: &mut u32) {
        *idle += 1;
        if *idle <= SPINS {
            self.platform.processes.yield_now();
        } else {
            self.platform.processes.sleep_ms(IDLE_MS);
        }
    }
}

/// The read end of a pipe, as a stream.
struct PipeInput {
    reader: PipeReader,
    stop: Stop,
}

impl Input for PipeInput {
    fn try_read(&mut self, out: &mut [u8]) -> Available {
        match self.reader.try_read(out) {
            Transfer::Moved(count) => Available::Bytes(count),
            Transfer::WouldBlock if self.stop.requested() => Available::End,
            Transfer::WouldBlock => Available::Pending,
            Transfer::Closed => Available::End,
        }
    }

    fn read(&mut self, out: &mut [u8]) -> usize {
        let mut idle = 0;
        loop {
            match self.reader.try_read(out) {
                Transfer::Moved(count) => return count,
                Transfer::Closed => return 0,
                Transfer::WouldBlock => {
                    if self.stop.requested() {
                        return 0;
                    }
                    self.stop.pause(&mut idle);
                }
            }
        }
    }

    fn adopt(&self) {
        self.reader.adopt();
    }
}

/// The write end of a pipe, as a stream.
struct PipeOutput {
    writer: PipeWriter,
    stop: Stop,
}

impl Output for PipeOutput {
    fn write(&mut self, mut bytes: &[u8]) {
        let mut idle = 0;
        while !bytes.is_empty() {
            match self.writer.try_write(bytes) {
                Transfer::Moved(count) => {
                    bytes = &bytes[count..];
                    idle = 0;
                }
                Transfer::Closed => return,
                Transfer::WouldBlock => {
                    if self.stop.requested() {
                        return;
                    }
                    self.stop.pause(&mut idle);
                }
            }
        }
    }

    fn is_broken(&self) -> bool {
        self.writer.is_broken() || self.stop.requested()
    }

    fn adopt(&self) {
        self.writer.adopt();
    }
}

/// New streams on the terminal a shell belongs to.
pub trait ConsoleSource: Send + Sync {
    /// Standard input, output and error for another task.
    fn streams(&self) -> ConsoleStreams;
}

/// The serial console's streams, which any task can use.
struct SerialConsole;

impl ConsoleSource for SerialConsole {
    fn streams(&self) -> ConsoleStreams {
        (
            Box::new(feos_sh::ConsoleInput),
            Box::new(feos_sh::ConsoleOutput),
            Box::new(feos_sh::ConsoleOutput),
        )
    }
}

/// A shell's route to processes.
pub struct ProcessHost<'d> {
    platform: &'static Platform,
    display: Option<DisplayAccess<'d>>,
    stop: Stop,
    console: Arc<dyn ConsoleSource>,
}

impl<'d> ProcessHost<'d> {
    /// A host for the serial shell, which owns the framebuffer and the UART.
    pub fn serial(
        platform: &'static Platform,
        display: &'d mut Option<Canvas<'static>>,
        cancel: CancelToken,
    ) -> Self {
        Self {
            platform,
            display: Some(DisplayAccess { display }),
            stop: Stop {
                platform,
                cancel,
                serial: true,
            },
            console: Arc::new(SerialConsole),
        }
    }

    /// A host for a shell with no framebuffer, stopped through `cancel`.
    pub fn detached(
        platform: &'static Platform,
        cancel: CancelToken,
        console: Arc<dyn ConsoleSource>,
    ) -> Self {
        Self {
            platform,
            display: None,
            stop: Stop {
                platform,
                cancel,
                serial: false,
            },
            console,
        }
    }

    /// Calls `change` on a process and everything it started, as job control
    /// must: a job's shell waits on the commands that are doing the work.
    fn for_tree(&self, pid: u32, mut change: impl FnMut(u32)) {
        let mut infos = [ProcessInfo::EMPTY; MAX_PROCESSES];
        let count = self.platform.processes.snapshot(&mut infos);
        let mut tree = [0u32; MAX_PROCESSES];
        let found = feos_app::process_tree(&infos[..count], pid, &mut tree);
        for member in &tree[..found] {
            change(*member);
        }
    }

    /// Runs an app that has to be where the framebuffer is.
    fn run_on_display(&mut self, app: &'static dyn App, name: &str, args: &[String]) -> i32 {
        let Some(access) = self.display.as_mut() else {
            if shell::session_active() {
                let mut line = String::from(name);
                for arg in args {
                    let quote = if arg.contains(char::is_whitespace) { "'" } else { "" };
                    let _ = write!(line, " {quote}{arg}{quote}");
                }
                return if shell::request_run(&line) { 0 } else { 1 };
            }
            return 126;
        };

        let mut block = ArgBlock::new();
        for arg in args {
            if !block.push(arg) {
                return 126;
            }
        }
        let mut joined = [0u8; PROCESS_ARGS_MAX];
        let joined = block.joined(&mut joined);
        let mut slots = [""; MAX_ARGC];
        let mut context = Context {
            args: joined,
            argv: block.argv(&mut slots),
            display: access.display,
            viewport: None,
            suspend: None,
            suspend_area: None,
            platform: self.platform,
        };
        app.run(&mut context)
    }

    /// Starts `app` in its own process and copies its output until it ends.
    fn run_process(
        &mut self,
        app: &'static dyn App,
        args: &[String],
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
    ) -> Result<i32, SpawnError> {
        let processes = self.platform.processes;
        let (Some((out_reader, out_writer)), Some((err_reader, err_writer))) =
            (pipe::create(), pipe::create())
        else {
            return Err(SpawnError::NoFreeSlot);
        };
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let io = SpawnIo {
            stdout: Some(&out_writer),
            stderr: Some(&err_writer),
            ..SpawnIo::default()
        };
        let pid = processes.spawn_argv(app, &argv, io)?;
        drop(out_writer);
        drop(err_writer);

        let mut buffer = [0u8; 2048];
        let mut out_open = true;
        let mut err_open = true;
        let mut killed = false;
        let mut interrupted = false;
        let mut idle = 0u32;
        loop {
            let moved = copy(&out_reader, &mut out_open, &mut buffer, stdout)
                | copy(&err_reader, &mut err_open, &mut buffer, stderr);
            let exited = processes.try_wait(pid) != WaitStatus::Running;
            if !out_open && !err_open || (exited && !moved) {
                break;
            }
            if !killed {
                interrupted = self.poll_cancel();
                if interrupted || stdout.is_broken() {
                    killed = true;
                    let _ = processes.try_kill(pid);
                }
            }
            if moved {
                idle = 0;
            } else {
                idle += 1;
                if idle <= SPINS {
                    processes.yield_now();
                } else {
                    processes.sleep_ms(IDLE_MS);
                }
            }
        }
        let status = processes.wait(pid).unwrap_or(feos_app::EXIT_KILLED);
        Ok(if interrupted { 130 } else { status })
    }
}

impl ProcessHost<'_> {
    /// Starts a user program and looks after its streams until it ends.
    ///
    /// The program's standard input is a pipe too, fed from the shell's stream as it
    /// has room. A program's reads of a terminal aren't provided for: there's no line
    /// discipline to give it, and the shell's Ctrl-C polling reads the same bytes. It
    /// sees end of file.
    fn run_user(
        &mut self,
        path: &str,
        argv: &[&str],
        cwd: &str,
        stdin: &mut dyn Input,
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
    ) -> Result<i32, SpawnError> {
        let processes = self.platform.processes;
        let (Some((out_reader, out_writer)), Some((err_reader, err_writer)), Some((in_reader, in_writer))) =
            (pipe::create(), pipe::create(), pipe::create())
        else {
            return Err(SpawnError::NoFreeSlot);
        };
        let io = SpawnIo {
            stdin: Some(&in_reader),
            stdout: Some(&out_writer),
            stderr: Some(&err_writer),
            ..SpawnIo::default()
        };
        let pid = processes.spawn_user(path, argv, cwd, io)?;
        drop(out_writer);
        drop(err_writer);
        drop(in_reader);

        let mut feed = if stdin.is_terminal() { None } else { Some(in_writer) };
        let mut buffer = [0u8; 2048];
        let mut waiting = [0u8; 2048];
        let (mut waiting_from, mut waiting_to) = (0, 0);
        let mut input_ended = false;
        let mut out_open = true;
        let mut err_open = true;
        let mut killed = false;
        let mut interrupted = false;
        let mut idle = 0u32;
        loop {
            let mut moved = copy(&out_reader, &mut out_open, &mut buffer, stdout)
                | copy(&err_reader, &mut err_open, &mut buffer, stderr);

            if let Some(writer) = feed.as_ref() {
                if waiting_from == waiting_to && !input_ended {
                    match stdin.try_read(&mut waiting) {
                        Available::Bytes(count) => {
                            waiting_from = 0;
                            waiting_to = count;
                        }
                        Available::Pending => {}
                        Available::End => input_ended = true,
                    }
                }
                let mut gone = false;
                if waiting_from < waiting_to {
                    match writer.try_write(&waiting[waiting_from..waiting_to]) {
                        Transfer::Moved(count) => {
                            waiting_from += count;
                            moved = true;
                        }
                        Transfer::WouldBlock => {}
                        Transfer::Closed => gone = true,
                    }
                }
                if gone || (input_ended && waiting_from == waiting_to) {
                    feed = None;
                }
            }

            let exited = processes.try_wait(pid) != WaitStatus::Running;
            if (!out_open && !err_open) && exited || (exited && !moved) {
                break;
            }
            if !killed {
                interrupted = self.poll_cancel();
                if interrupted || stdout.is_broken() {
                    killed = true;
                    let _ = processes.try_kill(pid);
                }
            }
            if moved {
                idle = 0;
            } else {
                idle += 1;
                if idle <= SPINS {
                    processes.yield_now();
                } else {
                    processes.sleep_ms(IDLE_MS);
                }
            }
        }
        let status = processes.wait(pid).unwrap_or(feos_app::EXIT_KILLED);
        Ok(if interrupted { 130 } else { status })
    }
}

/// Moves one chunk from `reader` to `output`. Returns `true` if any moved.
fn copy(
    reader: &PipeReader,
    open: &mut bool,
    buffer: &mut [u8],
    output: &mut dyn Output,
) -> bool {
    if !*open {
        return false;
    }
    match reader.try_read(buffer) {
        Transfer::Moved(count) => {
            output.write(&buffer[..count]);
            true
        }
        Transfer::WouldBlock => false,
        Transfer::Closed => {
            *open = false;
            false
        }
    }
}

impl Host for ProcessHost<'_> {
    fn run_external(
        &mut self,
        name: &str,
        args: &[String],
        _stdin: &mut dyn Input,
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
        background: bool,
    ) -> Option<i32> {
        let app = self.platform.apps.find(name)?;

        if app.needs_display() || (app.windowed() && self.display.is_none()) {
            let status = self.run_on_display(app, name, args);
            if status == 126 {
                let mut message = String::new();
                let _ = writeln!(message, "{name}: needs the display, which this shell has no way to reach");
                stderr.write(message.as_bytes());
            }
            return Some(status);
        }

        if background {
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let io = SpawnIo {
                background: true,
                ..SpawnIo::default()
            };
            return Some(match self.platform.processes.spawn_argv(app, &argv, io) {
                Ok(pid) => {
                    let mut message = String::new();
                    let _ = writeln!(message, "[shell] {name} started as PID {pid}");
                    stderr.write(message.as_bytes());
                    0
                }
                Err(error) => report_spawn(name, error, stderr),
            });
        }

        Some(match self.run_process(app, args, stdout, stderr) {
            Ok(status) => status,
            Err(error) => report_spawn(name, error, stderr),
        })
    }

    fn run_program(
        &mut self,
        path: &str,
        args: &[String],
        cwd: &str,
        stdin: &mut dyn Input,
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
        background: bool,
    ) -> Option<i32> {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        if background {
            let io = SpawnIo {
                background: true,
                ..SpawnIo::default()
            };
            return Some(match self.platform.processes.spawn_user(path, &argv, cwd, io) {
                Ok(pid) => {
                    let mut message = String::new();
                    let _ = writeln!(message, "[shell] {path} started as PID {pid}");
                    stderr.write(message.as_bytes());
                    0
                }
                Err(error) => report_spawn(path, error, stderr),
            });
        }
        Some(match self.run_user(path, &argv, cwd, stdin, stdout, stderr) {
            Ok(status) => status,
            Err(error) => report_spawn(path, error, stderr),
        })
    }

    fn command_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        for app in crate::REGISTRY {
            names.push(app.name());
            names.extend(app.aliases().iter().copied());
        }
        names
    }

    fn has_command(&self, name: &str) -> bool {
        self.platform.apps.find(name).is_some()
    }

    fn poll_cancel(&mut self) -> bool {
        self.stop.requested()
    }

    fn tasks(&mut self) -> Option<&mut dyn TaskHost> {
        Some(self)
    }
}

impl TaskHost for ProcessHost<'_> {
    fn pipe(&mut self) -> Option<(Box<dyn Input + Send>, Box<dyn Output + Send>)> {
        let (reader, writer) = pipe::create()?;
        let stop = Stop {
            serial: false,
            ..self.stop.clone()
        };
        Some((
            Box::new(PipeInput {
                reader,
                stop: self.stop.clone(),
            }),
            Box::new(PipeOutput { writer, stop }),
        ))
    }

    fn console_streams(&mut self) -> ConsoleStreams {
        self.console.streams()
    }

    fn fork_host(&mut self, cancel: CancelToken) -> Box<dyn Host + Send> {
        Box::new(ProcessHost::<'static> {
            platform: self.platform,
            display: None,
            stop: Stop {
                platform: self.platform,
                cancel,
                serial: false,
            },
            console: self.console.clone(),
        })
    }

    fn spawn(
        &mut self,
        name: &str,
        background: bool,
        job: Box<dyn FnOnce() -> i32 + Send>,
    ) -> Option<u32> {
        let io = SpawnIo {
            background,
            ..SpawnIo::default()
        };
        self.platform.processes.spawn_task(name, io, job).ok()
    }

    fn status(&mut self, pid: u32) -> JobStatus {
        let processes = self.platform.processes;
        match processes.try_wait(pid) {
            WaitStatus::Running => match processes.detail(pid) {
                Some(detail) if detail.stopped => JobStatus::Stopped,
                _ => JobStatus::Running,
            },
            WaitStatus::Exited(code) => JobStatus::Exited(code),
            WaitStatus::Unknown => JobStatus::Unknown,
        }
    }

    fn kill(&mut self, pid: u32) {
        let _ = self.platform.processes.try_kill(pid);
    }

    fn set_stopped(&mut self, pid: u32, stopped: bool) {
        let processes = self.platform.processes;
        self.for_tree(pid, |member| {
            processes.set_stopped(member, stopped);
        });
    }

    fn pause(&mut self) {
        self.platform.processes.sleep_ms(IDLE_MS);
    }

    fn system(&mut self) -> &'static dyn System {
        system(self.platform)
    }
}

fn report_spawn(name: &str, error: SpawnError, stderr: &mut dyn Output) -> i32 {
    let detail = match error {
        SpawnError::NoFreeSlot => "no free process slot",
        SpawnError::ArgsTooLong => "arguments too long",
        SpawnError::Unsupported => "this kernel can't start processes",
    };
    let mut message = String::new();
    let _ = writeln!(message, "{name}: {detail}");
    stderr.write(message.as_bytes());
    1
}
