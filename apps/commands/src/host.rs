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
    shell, App, ArgBlock, Context, Platform, ProcessInfo, Processes, SpawnError, SpawnIo, WaitStatus,
    MAX_ARGC, MAX_PROCESSES, PROCESS_ARGS_MAX,
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

    // Kernel-linked code is all of one privilege, and enforcing nothing on itself. It says
    // so here. User programs are who permissions apply to: `/bin/runas` starts one as
    // somebody else.
    fn username(&self) -> &'static str {
        "root"
    }

    fn uid(&self) -> u32 {
        0
    }

    fn gid(&self) -> u32 {
        0
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

    fn cpu_ms(&self) -> u64 {
        self.platform.processes.cpu_ms()
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

    /// Takes a Ctrl-Z waiting to stop the foreground command.
    fn take_suspend(&self) -> bool {
        false
    }

    /// Takes a Ctrl-\\ waiting to quit the foreground command.
    fn take_quit(&self) -> bool {
        false
    }
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

    fn take_suspend(&self) -> bool {
        feos_sh::console::take_suspend()
    }

    fn take_quit(&self) -> bool {
        feos_sh::console::take_quit()
    }
}

/// A command running in the foreground, or stopped by Ctrl-Z: its process and what
/// connects it to the shell's streams.
struct ForegroundJob {
    pid: u32,
    output: PipeReader,
    errors: PipeReader,
    output_open: bool,
    errors_open: bool,
    /// Where its standard input is fed. `None` once that has ended.
    feed: Option<PipeWriter>,
    /// The same pipe's read end, to take back what it never read when it ends.
    unread: PipeReader,
    /// Read from the shell's standard input and not yet taken by the pipe.
    waiting: Vec<u8>,
    input_ended: bool,
}

/// A shell's route to processes.
pub struct ProcessHost<'d> {
    platform: &'static Platform,
    display: Option<DisplayAccess<'d>>,
    stop: Stop,
    console: Arc<dyn ConsoleSource>,
    /// The shell's own host, not a fork's: only it can have a foreground command.
    foreground: bool,
    /// Commands Ctrl-Z stopped.
    suspended: Vec<ForegroundJob>,
    /// The one that has stopped just now, for the shell to take.
    just_suspended: Option<u32>,
}

impl Drop for ProcessHost<'_> {
    /// A stopped command has nothing left to show its output on, or to resume it.
    fn drop(&mut self) {
        for job in &self.suspended {
            let _ = self.platform.processes.try_kill(job.pid);
        }
    }
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
            foreground: true,
            suspended: Vec::new(),
            just_suspended: None,
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
            foreground: true,
            suspended: Vec::new(),
            just_suspended: None,
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

    /// Starts `app` in its own process and looks after its streams until it ends.
    fn run_process(
        &mut self,
        app: &'static dyn App,
        args: &[String],
        stdin: &mut dyn Input,
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
    ) -> Result<i32, SpawnError> {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let processes = self.platform.processes;
        self.start_foreground(stdin, stdout, stderr, |io| processes.spawn_argv(app, &argv, io))
    }

    /// Starts a user program and looks after its streams until it ends.
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
        self.start_foreground(stdin, stdout, stderr, |io| processes.spawn_user(path, argv, cwd, io))
    }

    fn start_foreground(
        &mut self,
        stdin: &mut dyn Input,
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
        spawn: impl FnOnce(SpawnIo<'_>) -> Result<u32, SpawnError>,
    ) -> Result<i32, SpawnError> {
        let (Some((output, out_writer)), Some((errors, err_writer)), Some((in_reader, in_writer))) =
            (pipe::create(), pipe::create(), pipe::create())
        else {
            return Err(SpawnError::NoFreeSlot);
        };
        let pid = spawn(SpawnIo {
            stdin: Some(&in_reader),
            stdout: Some(&out_writer),
            stderr: Some(&err_writer),
            ..SpawnIo::default()
        })?;
        drop(out_writer);
        drop(err_writer);
        let job = ForegroundJob {
            pid,
            unread: in_reader,
            output,
            errors,
            output_open: true,
            errors_open: true,
            feed: Some(in_writer),
            waiting: Vec::new(),
            input_ended: false,
        };
        Ok(self.supervise(job, stdin, stdout, stderr))
    }

    /// Feeds a process's standard input and copies its output until it ends, or until
    /// Ctrl-Z stops it. Returns its status.
    ///
    /// A terminal is only read while the process is asking for input, so what's typed
    /// ahead of a command that never reads stays for the next reader, and isn't echoed.
    /// Only a command with nothing between it and the terminal can be stopped: anything
    /// else has a shell waiting for it that a stopped command would hang.
    fn supervise(
        &mut self,
        mut job: ForegroundJob,
        stdin: &mut dyn Input,
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
    ) -> i32 {
        let processes = self.platform.processes;
        let terminal = stdin.is_terminal();
        let stoppable =
            self.foreground && terminal && stdout.is_terminal() && stderr.is_terminal();
        // Only keys pressed for this command count.
        let _ = self.console.take_suspend();
        let _ = self.console.take_quit();
        let mut buffer = [0u8; 2048];
        let mut killed = false;
        let mut interrupted = false;
        let mut quit = false;
        let mut idle = 0u32;
        loop {
            let (exited, mut moved) = copy_streams(
                processes,
                job.pid,
                [(&job.output, &mut job.output_open), (&job.errors, &mut job.errors_open)],
                &mut buffer,
                [&mut *stdout, &mut *stderr],
            );
            moved |= feed_stdin(&mut job, stdin, terminal, &mut buffer);

            if (!job.output_open && !job.errors_open) && exited || (exited && !moved) {
                break;
            }
            if !killed {
                interrupted = self.poll_cancel();
                if self.console.take_quit() {
                    quit = true;
                }
                if interrupted || quit || stdout.is_broken() {
                    killed = true;
                    let _ = processes.try_kill(job.pid);
                } else if self.console.take_suspend() && stoppable {
                    self.for_tree(job.pid, |member| {
                        processes.set_stopped(member, true);
                    });
                    self.just_suspended = Some(job.pid);
                    self.suspended.push(job);
                    return 148;
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
        // Keys it was given and didn't read belong to whoever reads next. Only a
        // terminal keeps them: a pipe's reader could never see them anyway.
        if terminal {
            let mut rest = core::mem::take(&mut job.waiting);
            let mut taken = Vec::new();
            while let Transfer::Moved(count) = job.unread.try_read(&mut buffer) {
                taken.extend_from_slice(&buffer[..count]);
            }
            taken.append(&mut rest);
            stdin.unread(&taken);
        }
        let status = processes.wait(job.pid).unwrap_or(feos_app::EXIT_KILLED);
        if quit {
            // The shell's QUIT trap, if there is one, sees this next.
            processes.signal(processes.current(), feos_app::signal::QUIT);
            return 131;
        }
        if interrupted {
            130
        } else {
            status
        }
    }

    /// Forgets a stopped job that has ended.
    fn forget_job(&mut self, pid: u32) {
        self.suspended.retain(|job| job.pid != pid);
    }
}

/// Moves a chunk of standard input towards a process. Returns `true` if any moved.
fn feed_stdin(
    job: &mut ForegroundJob,
    stdin: &mut dyn Input,
    terminal: bool,
    buffer: &mut [u8],
) -> bool {
    let Some(writer) = job.feed.as_ref() else {
        return false;
    };
    if job.waiting.is_empty() && !job.input_ended && (!terminal || writer.reader_wants()) {
        match stdin.try_read(buffer) {
            Available::Bytes(count) => job.waiting.extend_from_slice(&buffer[..count]),
            Available::Pending => {}
            Available::End => job.input_ended = true,
        }
    }
    let mut moved = false;
    let mut gone = false;
    if !job.waiting.is_empty() {
        match writer.try_write(&job.waiting) {
            Transfer::Moved(count) => {
                job.waiting.drain(..count);
                moved = true;
            }
            Transfer::WouldBlock => {}
            Transfer::Closed => gone = true,
        }
    }
    if gone || (job.input_ended && job.waiting.is_empty()) {
        job.feed = None;
    }
    moved
}

/// The body of a task that shows a job's output after `bg`, when no shell's there to.
fn copy_job_output(
    processes: &'static dyn Processes,
    output: PipeReader,
    errors: PipeReader,
    mut stdout: Box<dyn Output + Send>,
    mut stderr: Box<dyn Output + Send>,
) -> i32 {
    output.adopt();
    errors.adopt();
    stdout.adopt();
    stderr.adopt();
    let mut buffer = [0u8; 2048];
    let (mut output_open, mut errors_open) = (true, true);
    while output_open || errors_open {
        let mut moved = copy(&output, &mut output_open, &mut buffer, &mut *stdout);
        moved |= copy(&errors, &mut errors_open, &mut buffer, &mut *stderr);
        if !moved {
            processes.sleep_ms(5);
        }
    }
    0
}

/// Copies a chunk from each of a process's streams, and says whether it had already
/// exited, and whether anything moved.
///
/// The order matters: it has to look at the process first. All it wrote is in the pipes
/// by the time it has exited, so an exited process with nothing moved has nothing more
/// to come. The other way round, its last write and its exit can both land between
/// finding the pipe empty and finding it gone, and that output would never be read.
fn copy_streams(
    processes: &dyn Processes,
    pid: u32,
    streams: [(&PipeReader, &mut bool); 2],
    buffer: &mut [u8],
    outputs: [&mut dyn Output; 2],
) -> (bool, bool) {
    let exited = processes.try_wait(pid) != WaitStatus::Running;
    let mut moved = false;
    for ((reader, open), output) in streams.into_iter().zip(outputs) {
        moved |= copy(reader, open, buffer, output);
    }
    (exited, moved)
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
        stdin: &mut dyn Input,
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

        Some(match self.run_process(app, args, stdin, stdout, stderr) {
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

    fn poll_suspend(&mut self) -> bool {
        // The serial console's keys are only seen by looking.
        if self.foreground && self.stop.requested() {
            return false;
        }
        self.foreground && self.console.take_suspend()
    }

    fn take_suspended(&mut self) -> Option<u32> {
        self.just_suspended.take()
    }

    fn resume_job(
        &mut self,
        pid: u32,
        stdin: &mut dyn Input,
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
    ) -> Option<i32> {
        let index = self.suspended.iter().position(|job| job.pid == pid)?;
        let job = self.suspended.remove(index);
        Some(self.supervise(job, stdin, stdout, stderr))
    }

    fn continue_job(&mut self, pid: u32) {
        let Some(index) = self.suspended.iter().position(|job| job.pid == pid) else {
            return;
        };
        let job = self.suspended.remove(index);
        let (_, stdout, stderr) = self.console.streams();
        let processes = self.platform.processes;
        let (output, errors) = (job.output, job.errors);
        let io = SpawnIo {
            background: true,
            ..SpawnIo::default()
        };
        // With no task to show it, its output stays in the pipes until they fill.
        let _ = processes.spawn_task(
            "job-io",
            io,
            Box::new(move || copy_job_output(processes, output, errors, stdout, stderr)),
        );
    }

    fn catch_signals(&mut self, mask: u32) {
        self.platform.processes.catch_signals(mask);
    }

    fn take_signals(&mut self) -> u32 {
        self.platform.processes.take_signals()
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
            foreground: false,
            suspended: Vec::new(),
            just_suspended: None,
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
            WaitStatus::Exited(code) => {
                self.forget_job(pid);
                JobStatus::Exited(code)
            }
            WaitStatus::Unknown => {
                self.forget_job(pid);
                JobStatus::Unknown
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::vec;

    /// A process that does its last write and exits while the copy loop looks at it:
    /// the moment a CPU can be taken away from the loop for.
    struct ExitsWhileWatched {
        last_words: std::sync::Mutex<Option<PipeWriter>>,
    }

    impl Processes for ExitsWhileWatched {
        fn spawn(&self, _app: &'static dyn App, _args: &str) -> Result<u32, SpawnError> {
            Err(SpawnError::Unsupported)
        }

        fn kill(&self, _pid: u32) -> bool {
            false
        }

        fn current(&self) -> u32 {
            0
        }

        fn snapshot(&self, _out: &mut [ProcessInfo]) -> usize {
            0
        }

        fn try_wait(&self, _pid: u32) -> WaitStatus {
            if let Some(writer) = self.last_words.lock().unwrap().take() {
                assert_eq!(writer.try_write(b"last line\n"), Transfer::Moved(10));
                drop(writer);
            }
            WaitStatus::Exited(0)
        }
    }

    #[derive(Default)]
    struct Collected(RefCell<Vec<u8>>);

    impl Output for &Collected {
        fn write(&mut self, bytes: &[u8]) {
            self.0.borrow_mut().extend_from_slice(bytes);
        }
    }

    #[test]
    fn output_written_just_before_exit_is_not_lost() {
        let (out_reader, out_writer) = pipe::create().unwrap();
        let (err_reader, err_writer) = pipe::create().unwrap();
        // Holding this end open means the stream can't report end of file instead,
        // which is how a child with the same stream would look.
        let processes = ExitsWhileWatched {
            last_words: std::sync::Mutex::new(Some(out_writer.try_clone().unwrap())),
        };
        let collected = Collected::default();
        let (mut stdout, mut stderr) = (&collected, &collected);
        let (mut out_open, mut err_open) = (true, true);
        let mut buffer = vec![0u8; 256];
        loop {
            let (exited, moved) = copy_streams(
                &processes,
                1,
                [(&out_reader, &mut out_open), (&err_reader, &mut err_open)],
                &mut buffer,
                [&mut stdout, &mut stderr],
            );
            // As in the loops above: an exited process with nothing moved is finished.
            if exited && !moved {
                break;
            }
        }
        drop((out_writer, err_writer));
        assert_eq!(collected.0.borrow().as_slice(), b"last line\n");
    }
}
