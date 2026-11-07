//! Concurrency: streaming pipelines, background jobs and `trap`.
//!
//! With a [`TaskHost`], a pipeline's stages run at once with bounded pipes between
//! them, so `yes | head -1` ends, and `&` really runs in the background. Each
//! concurrent part gets a fork of the shell: a copy of the variables, functions
//! and working directory that writes to the same files. The last stage of a
//! pipeline stays on the shell's own task, where the pipeline's output goes.
//!
//! A host without tasks gets the buffered pipeline in `exec.rs`, and `&` runs in
//! the foreground. So does a pipeline whose streams can't be handed to another
//! task, such as one with its error output going to a capture.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_coreutils::{BytesInput, Input, NullInput, NullOutput, Output};

use core::cell::RefCell;

use crate::fds::{FdInput, FdOutput, OpenFile, StreamView};
use crate::lexer::Word;
use crate::parser::{Command, List};
use crate::{CancelToken, Flow, Host, Io, JobStatus, Shell, TaskHost};

/// One background job, as `jobs` lists it.
#[derive(Clone)]
pub(crate) struct Job {
    pub(crate) id: usize,
    pub(crate) pid: u32,
    pub(crate) text: String,
    pub(crate) stopped: bool,
}

/// The streams a forked shell starts with.
struct Streams {
    stdin: Box<dyn Input + Send>,
    stdout: Box<dyn Output + Send>,
    stderr: Box<dyn Output + Send>,
}

/// A forked shell's standard output. Writing once the reader has gone ends the
/// shell, as SIGPIPE would; a command that writes nothing is left to finish.
struct BreakingOutput {
    inner: Box<dyn Output + Send>,
    cancel: CancelToken,
    broke: bool,
}

impl Output for BreakingOutput {
    fn write(&mut self, bytes: &[u8]) {
        if self.inner.is_broken() {
            self.broke = true;
            self.cancel.cancel();
            return;
        }
        self.inner.write(bytes);
    }

    fn is_terminal(&self) -> bool {
        self.inner.is_terminal()
    }

    fn is_broken(&self) -> bool {
        self.inner.is_broken()
    }
}

fn describe_words(words: &[Word]) -> String {
    let mut text = String::new();
    for word in words {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&word.raw());
    }
    text
}

/// A short form of a command for `jobs` and the process list.
fn describe_command(command: &Command) -> String {
    match command {
        Command::Simple(simple) => describe_words(&simple.words),
        Command::Redirected(inner, _) => describe_command(inner),
        Command::Subshell(_) => String::from("( ... )"),
        Command::Group(_) => String::from("{ ... }"),
        _ => String::from("sh"),
    }
}

fn describe_list(list: &List) -> String {
    let mut text = String::new();
    for command in &list.first.commands {
        if !text.is_empty() {
            text.push_str(" | ");
        }
        text.push_str(&describe_command(command));
    }
    if !list.rest.is_empty() {
        text.push_str(" ...");
    }
    text
}

/// The name a task for `command` gets in the process list.
fn task_name(command: &Command) -> String {
    let text = describe_command(command);
    let word = text.split_whitespace().next().unwrap_or("sh");
    String::from(word)
}

impl Shell<'_> {
    /// A copy of this shell for another task. Variables and functions carry
    /// over; files are shared, when the filesystem can be.
    fn fork(&self, system: &'static dyn feos_coreutils::System) -> Shell<'static> {
        let mut shell = Shell::with_fs(self.fs.fork(), system);
        shell.env = self.env.clone();
        shell.aliases = self.aliases.clone();
        shell.functions = self.functions.clone();
        // So `jobs | grep x` still has something to list.
        shell.jobs = self.jobs.clone();
        shell.next_job = self.next_job;
        shell.params = self.params.clone();
        shell.arg0 = self.arg0.clone();
        shell.opts = self.opts;
        shell.last_status = self.last_status;
        shell.deadline = self.deadline;
        shell.columns = self.columns;
        shell.interactive = false;
        shell.random = self.random.rotate_left(13) | 1;
        shell
    }

    /// Whether descriptors 1 and 2 are ones another task can be given.
    fn fork_output(
        &mut self,
        fd: usize,
        tasks: &mut dyn TaskHost,
    ) -> Option<Box<dyn Output + Send>> {
        match self.fds.resolve(fd)? {
            OpenFile::Base(1) => Some(tasks.console_streams().1),
            OpenFile::Base(2) => Some(tasks.console_streams().2),
            OpenFile::Null | OpenFile::Zero => Some(Box::new(NullOutput)),
            _ => None,
        }
    }

    /// What standard input another task can be given. A file or here-document is
    /// copied; the parent's position in it doesn't move.
    fn fork_input(&mut self, tasks: &mut dyn TaskHost) -> Option<Box<dyn Input + Send>> {
        match self.fds.resolve(0)? {
            OpenFile::Base(0) => Some(tasks.console_streams().0),
            OpenFile::Null => Some(Box::new(NullInput)),
            OpenFile::Reader { data, position } => {
                let rest = data.get(*position..).unwrap_or(&[]).to_vec();
                Some(Box::new(BytesInput::new(rest)))
            }
            _ => None,
        }
    }

    /// Starts `body` on a task of its own, in a fork of this shell.
    fn spawn_fork(
        &mut self,
        name: &str,
        background: bool,
        streams: Streams,
        tasks: &mut dyn TaskHost,
        body: impl FnOnce(&mut Shell<'static>, &mut Io<'_>, &mut dyn Host) -> i32 + Send + 'static,
    ) -> Option<u32> {
        let mut shell = self.fork(tasks.system());
        let mut host = tasks.fork_host(shell.cancel_token());
        let job = Box::new(move || {
            let Streams {
                mut stdin,
                stdout,
                mut stderr,
            } = streams;
            stdin.adopt();
            stdout.adopt();
            stderr.adopt();
            let mut stdout = BreakingOutput {
                inner: stdout,
                cancel: shell.cancel_token(),
                broke: false,
            };
            let status = {
                let mut io = Io {
                    stdin: &mut *stdin,
                    stdout: &mut stdout,
                    stderr: &mut *stderr,
                };
                shell.run_forked(&mut io, &mut *host, body)
            };
            if stdout.broke {
                141
            } else {
                status
            }
        });
        tasks.spawn(name, background, job)
    }

    /// The whole life of a forked shell: its status is the task's exit status.
    fn run_forked(
        &mut self,
        io: &mut Io<'_>,
        host: &mut dyn Host,
        body: impl FnOnce(&mut Shell<'static>, &mut Io<'_>, &mut dyn Host) -> i32,
    ) -> i32
    where
        Self: AsStatic,
    {
        self.note_stack_base();
        let status = body(self.as_static(), io, host);
        let status = match core::mem::replace(&mut self.flow, Flow::None) {
            Flow::Exit(code) | Flow::Return(code) | Flow::Abort(code) => code,
            // Nothing else can interrupt a fork: no keyboard reaches it.
            Flow::Interrupted => 137,
            Flow::None | Flow::Break(_) | Flow::Continue(_) => status,
        };
        self.last_status = status;
        self.run_exit_trap(io, host);
        self.flush_writers(io);
        status
    }

    /// Runs a pipeline with every stage live at once. `None` means the buffered
    /// path should run it instead, and nothing has started.
    pub(crate) fn run_streaming(
        &mut self,
        commands: &[Command],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Option<i32> {
        let (last, earlier) = commands.split_last()?;
        if self.fds.has_extra_descriptors() {
            return None;
        }
        let tasks = host.tasks()?;

        let mut previous = Some(self.fork_input(tasks)?);
        // Checked before anything starts, so a refusal can still fall back.
        self.fork_output(2, tasks)?;

        let mut pids: Vec<u32> = Vec::new();
        for command in earlier {
            let started = self.start_stage(command, &mut previous, tasks);
            match started {
                Some((pid, reader)) => {
                    pids.push(pid);
                    previous = Some(reader);
                }
                None if pids.is_empty() => return None,
                None => {
                    for pid in &pids {
                        tasks.kill(*pid);
                    }
                    self.error(io, "no free process slot for this pipeline");
                    return Some(1);
                }
            }
        }

        // The last stage is on this task, where the pipeline's output goes.
        let saved = self.fds.save();
        let mut leftovers = Vec::new();
        if let Some(reader) = previous.take() {
            let file = self.fds.open(OpenFile::Stream(reader));
            self.fds.bind(0, file, &mut leftovers);
        }
        let mut status = if self.stage_needs_isolation(last) {
            self.in_subshell(io, host, |shell, io, host| {
                shell.run_command(last, io, host, false)
            })
        } else {
            self.run_command(last, io, host, false)
        };
        // Closes the read end, which is what stops a stage that never would.
        self.finish_redirects(saved, io);
        self.flush_leftovers(leftovers, io);

        let mut failed = 0;
        for pid in pids {
            let code = self.wait_for_task(pid, io, host);
            if code != 0 {
                failed = code;
            }
        }
        if status != 0 {
            failed = status;
        }
        if self.opts.pipefail {
            status = failed;
        }
        Some(status)
    }

    fn start_stage(
        &mut self,
        command: &Command,
        previous: &mut Option<Box<dyn Input + Send>>,
        tasks: &mut dyn TaskHost,
    ) -> Option<(u32, Box<dyn Input + Send>)> {
        let (reader, writer) = tasks.pipe()?;
        let stderr = self.fork_output(2, tasks)?;
        let stdin = previous.take()?;
        let command = command.clone();
        let name = task_name(&command);
        let streams = Streams {
            stdin,
            stdout: writer,
            stderr,
        };
        let pid = self.spawn_fork(&name, false, streams, tasks, move |shell, io, host| {
            shell.run_command(&command, io, host, false)
        })?;
        Some((pid, reader))
    }

    /// Waits for a task, killing it if this shell is interrupted meanwhile.
    pub(crate) fn wait_for_task(
        &mut self,
        pid: u32,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let _ = io;
        let mut killed = false;
        loop {
            let interrupted = self.checkpoint(host);
            // `wait` gives way to a trap, and so does a job in the foreground.
            self.deliver_signals(io, host);
            let suspend = self.foreground_wait && !interrupted && !killed && host.poll_suspend();
            let Some(tasks) = host.tasks() else {
                return 1;
            };
            if interrupted && !killed {
                killed = true;
                tasks.kill(pid);
            }
            if suspend {
                tasks.set_stopped(pid, true);
                self.suspended_task = true;
                return SUSPENDED_STATUS;
            }
            match tasks.status(pid) {
                JobStatus::Exited(code) => return code,
                JobStatus::Unknown => return 127,
                JobStatus::Running | JobStatus::Stopped => tasks.pause(),
            }
        }
    }

    /// Starts a `&` list on a task. `None` means it should run in the foreground.
    pub(crate) fn start_job(
        &mut self,
        list: &List,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Option<i32> {
        if self.fds.has_extra_descriptors() {
            return None;
        }
        let tasks = host.tasks()?;
        let stdout = self.fork_output(1, tasks)?;
        let stderr = self.fork_output(2, tasks)?;
        let streams = Streams {
            // A background job doesn't get the keyboard.
            stdin: Box::new(NullInput),
            stdout,
            stderr,
        };

        let mut body = list.clone();
        body.background = false;
        let name = task_name(body.first.commands.first()?);
        let pid = self.spawn_fork(&name, true, streams, tasks, move |shell, io, host| {
            shell.run_list(&body, io, host)
        })?;

        self.next_job += 1;
        let id = self.next_job;
        self.jobs.push(Job {
            id,
            pid,
            text: describe_list(list),
            stopped: false,
        });
        self.last_background = Some(pid);
        if self.interactive {
            self.write_err(io, &format!("[{id}] {pid}\n"));
        }
        Some(0)
    }

    /// Reports jobs that have finished, at the prompt.
    pub(crate) fn notify_jobs(&mut self, io: &mut Io<'_>, host: &mut dyn Host) {
        if self.jobs.is_empty() {
            return;
        }
        let Some(tasks) = host.tasks() else {
            return;
        };
        let mut message = String::new();
        self.jobs.retain(|job| match tasks.status(job.pid) {
            JobStatus::Running | JobStatus::Stopped => true,
            JobStatus::Exited(0) | JobStatus::Unknown => {
                message.push_str(&format!("[{}]  Done  {}\n", job.id, job.text));
                false
            }
            JobStatus::Exited(code) => {
                message.push_str(&format!("[{}]  Exit {code}  {}\n", job.id, job.text));
                false
            }
        });
        if !message.is_empty() {
            self.write_err(io, &message);
        }
    }

    /// Finds a job from `%n`, `%%`, `%+` or a PID. No text means the latest.
    fn find_job(&self, spec: Option<&str>) -> Option<usize> {
        match spec {
            None | Some("%%" | "%+" | "%") => self.jobs.len().checked_sub(1),
            Some(text) => {
                if let Some(number) = text.strip_prefix('%') {
                    let id = number.parse::<usize>().ok()?;
                    self.jobs.iter().position(|job| job.id == id)
                } else {
                    let pid = text.parse::<u32>().ok()?;
                    self.jobs.iter().position(|job| job.pid == pid)
                }
            }
        }
    }

    pub(crate) fn builtin_jobs(
        &mut self,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let only_pids = args.first().is_some_and(|arg| arg == "-p");
        let mut text = String::new();
        if let Some(tasks) = host.tasks() {
            let count = self.jobs.len();
            let mut finished = Vec::new();
            for (index, job) in self.jobs.iter_mut().enumerate() {
                let state = match tasks.status(job.pid) {
                    JobStatus::Running => String::from("Running"),
                    JobStatus::Stopped => String::from("Stopped"),
                    JobStatus::Exited(0) | JobStatus::Unknown => {
                        finished.push(job.id);
                        String::from("Done")
                    }
                    JobStatus::Exited(code) => {
                        finished.push(job.id);
                        format!("Exit {code}")
                    }
                };
                job.stopped = state == "Stopped";
                if only_pids {
                    text.push_str(&format!("{}\n", job.pid));
                } else {
                    let mark = if index + 1 == count { '+' } else { ' ' };
                    text.push_str(&format!("[{}]{mark} {state:<8} {}\n", job.id, job.text));
                }
            }
            self.jobs.retain(|job| !finished.contains(&job.id));
        }
        self.write_out(io, &text);
        0
    }

    pub(crate) fn builtin_fg(
        &mut self,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let Some(index) = self.find_job(args.first().map(String::as_str)) else {
            self.error(io, "fg: no such job");
            return 1;
        };
        let (pid, text) = (self.jobs[index].pid, self.jobs[index].text.clone());
        let Some(tasks) = host.tasks() else {
            return 1;
        };
        tasks.set_stopped(pid, false);
        self.jobs[index].stopped = false;
        self.write_out(io, &format!("{text}\n"));
        // A command Ctrl-Z stopped comes back to the host, which has its streams. A job
        // that was started with `&` is a task with streams of its own.
        let result = {
            let view = RefCell::new(StreamView {
                fds: &mut self.fds,
                io,
                cancel: &self.cancel,
                host: None,
                system: self.system,
                deadline: None,
                timed_out: &mut self.timed_out,
                calls: 0,
            });
            let mut stdin = FdInput { view: &view, fd: 0 };
            let mut stdout = FdOutput { view: &view, fd: 1 };
            let mut stderr = FdOutput { view: &view, fd: 2 };
            host.resume_job(pid, &mut stdin, &mut stdout, &mut stderr)
        };
        let status = match result {
            Some(status) => {
                if status == 130 {
                    self.cancel.cancel();
                    self.flow = Flow::Interrupted;
                }
                status
            }
            None => {
                self.foreground_wait = true;
                let status = self.wait_for_task(pid, io, host);
                self.foreground_wait = false;
                status
            }
        };
        // Stopped again: it stays a job.
        let again = host.take_suspended().is_some() || core::mem::take(&mut self.suspended_task);
        if again {
            if let Some(job) = self.jobs.iter_mut().find(|job| job.pid == pid) {
                job.stopped = true;
                let line = format!("\n[{}]+  Stopped  {}\n", job.id, job.text);
                self.write_err(io, &line);
            }
            return SUSPENDED_STATUS;
        }
        self.jobs.retain(|job| job.pid != pid);
        status
    }

    pub(crate) fn builtin_bg(
        &mut self,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let Some(index) = self.find_job(args.first().map(String::as_str)) else {
            self.error(io, "bg: no such job");
            return 1;
        };
        let Some(tasks) = host.tasks() else {
            return 1;
        };
        let pid = self.jobs[index].pid;
        tasks.set_stopped(pid, false);
        self.jobs[index].stopped = false;
        host.continue_job(pid);
        let line = format!("[{}]+ {} &\n", self.jobs[index].id, self.jobs[index].text);
        self.write_out(io, &line);
        0
    }

    pub(crate) fn builtin_wait(
        &mut self,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        if args.is_empty() {
            while let Some(job) = self.jobs.first() {
                let pid = job.pid;
                self.wait_for_task(pid, io, host);
                self.jobs.retain(|job| job.pid != pid);
                if self.flow.is_active() {
                    return 130;
                }
            }
            return 0;
        }
        let mut status = 0;
        for arg in args {
            let pid = match arg.strip_prefix('%') {
                Some(_) => self.find_job(Some(arg)).map(|index| self.jobs[index].pid),
                None => arg.parse::<u32>().ok(),
            };
            let Some(pid) = pid else {
                self.error(io, &format!("wait: {arg}: no such job"));
                status = 127;
                continue;
            };
            status = self.wait_for_task(pid, io, host);
            self.jobs.retain(|job| job.pid != pid);
            if self.flow.is_active() {
                return 130;
            }
        }
        status
    }

    /// `kill %1` is a job's process; everything else goes to the `kill` command.
    pub(crate) fn builtin_kill(
        &mut self,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Option<i32> {
        if !host.has_command("kill") {
            return None;
        }
        let mut mapped = Vec::new();
        for arg in args {
            if arg.starts_with('%') {
                let Some(index) = self.find_job(Some(arg)) else {
                    self.error(io, &format!("kill: {arg}: no such job"));
                    return Some(1);
                };
                mapped.push(self.jobs[index].pid.to_string());
            } else {
                mapped.push(arg.clone());
            }
        }
        Some(self.run_external("kill", &mapped, io, host, false))
    }

    /// Adds a command Ctrl-Z stopped to the job table, as Bash reports it.
    pub(crate) fn note_suspended(
        &mut self,
        pid: u32,
        name: &str,
        args: &[String],
        io: &mut Io<'_>,
    ) -> i32 {
        let mut text = String::from(name);
        for arg in args {
            text.push(' ');
            text.push_str(arg);
        }
        self.next_job += 1;
        let id = self.next_job;
        self.jobs.push(Job {
            id,
            pid,
            text: text.clone(),
            stopped: true,
        });
        self.last_background = Some(pid);
        self.write_err(io, &format!("\n[{id}]+  Stopped  {text}\n"));
        SUSPENDED_STATUS
    }

    /// Runs the traps for the signals that have arrived. A signal this shell has no trap
    /// for never gets here, since it's only asked for the ones it has.
    pub(crate) fn deliver_signals(&mut self, io: &mut Io<'_>, host: &mut dyn Host) {
        let pending = host.take_signals();
        if pending == 0 {
            return;
        }
        for (number, name) in CAUGHT_SIGNALS {
            if pending & (1 << number) != 0 {
                self.run_trap(name, io, host);
            }
        }
    }

    /// Whether a trap covers a signal, which the prompt then has to look out for.
    pub(crate) fn catches_signals(&self) -> bool {
        CAUGHT_SIGNALS.iter().any(|(_, name)| self.traps.contains_key(*name))
    }

    /// Tells the host which signals this shell's traps cover.
    pub(crate) fn sync_signal_mask(&self, host: &mut dyn Host) {
        let mut mask = 0;
        for (number, name) in CAUGHT_SIGNALS {
            if self.traps.contains_key(*name) {
                mask |= 1 << number;
            }
        }
        host.catch_signals(mask);
    }

    /// Runs what a terminal going away means: the HUP trap and then the EXIT trap.
    pub fn hang_up(&mut self, io: &mut Io<'_>, host: &mut dyn Host) {
        self.note_stack_base();
        self.run_trap("HUP", io, host);
        self.flow = Flow::None;
        self.run_exit_trap(io, host);
    }

    /// Fires what has arrived while no command runs, between lines at a prompt.
    pub fn poll_signals(&mut self, io: &mut Io<'_>, host: &mut dyn Host) {
        self.note_stack_base();
        self.deliver_signals(io, host);
        if let Flow::Exit(code) = core::mem::replace(&mut self.flow, Flow::None) {
            self.exit_requested = Some(code);
            self.last_status = code;
            self.run_exit_trap(io, host);
        }
    }

    /// When a `time` started: the clock and the CPU used so far.
    pub(crate) fn start_timing(&self) -> (u64, u64) {
        (self.system.uptime_ms(), self.system.cpu_ms())
    }

    /// Prints `time`'s report on standard error. All FEOS time in a command is either
    /// kernel-thread time or isn't told apart from it, so it's all called user time.
    pub(crate) fn report_timing(&mut self, start: (u64, u64), posix: bool, io: &mut Io<'_>) {
        let real = self.system.uptime_ms().saturating_sub(start.0);
        let user = self.system.cpu_ms().saturating_sub(start.1);
        let text = if posix {
            let short = |ms: u64| format!("{}.{:02}", ms / 1000, ms % 1000 / 10);
            format!("real {}\nuser {}\nsys {}\n", short(real), short(user), short(0))
        } else {
            let long = |ms: u64| format!("{}m{}.{:03}s", ms / 60_000, ms / 1000 % 60, ms % 1000);
            format!("\nreal\t{}\nuser\t{}\nsys\t{}\n", long(real), long(user), long(0))
        };
        self.write_err(io, &text);
    }

    pub(crate) fn builtin_trap(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let listing = args.is_empty() || args[0] == "-p";
        if listing {
            let names = if args.len() > 1 { &args[1..] } else { &[] };
            let mut text = String::new();
            for (name, action) in &self.traps {
                if names.is_empty() || names.iter().any(|wanted| signal_name(wanted) == Some(name)) {
                    text.push_str(&format!("trap -- '{action}' {name}\n"));
                }
            }
            self.write_out(io, &text);
            return 0;
        }
        if args[0] == "-l" {
            self.write_out(io, "EXIT INT TERM HUP QUIT ERR\n");
            return 0;
        }
        // `trap 2` and `trap - INT` both clear; one word is a reset, as in POSIX.
        let (action, signals) = match args.split_first() {
            Some((first, rest)) if first == "--" => match rest.split_first() {
                Some((action, signals)) => (action.as_str(), signals),
                None => return 0,
            },
            Some((first, rest)) if !rest.is_empty() => (first.as_str(), rest),
            _ => ("-", args),
        };
        let mut status = 0;
        for signal in signals {
            let Some(name) = signal_name(signal) else {
                self.error(io, &format!("trap: {signal}: invalid signal"));
                status = 1;
                continue;
            };
            if action == "-" {
                self.traps.remove(name);
            } else {
                self.traps.insert(String::from(name), action.to_string());
            }
        }
        self.sync_signal_mask(host);
        status
    }

    /// Runs a trap's command, keeping `$?` and not letting it fire again.
    fn run_trap(&mut self, name: &str, io: &mut Io<'_>, host: &mut dyn Host) {
        let Some(action) = self.traps.get(name).cloned() else {
            return;
        };
        if action.is_empty() || self.in_trap {
            return;
        }
        self.in_trap = true;
        let status = self.last_status;
        let flow = core::mem::replace(&mut self.flow, Flow::None);
        self.run_text_inner(&action, io, host);
        // `exit` in a trap wins; anything else leaves things as they were.
        if !matches!(self.flow, Flow::Exit(_)) {
            self.flow = flow;
            self.last_status = status;
        }
        self.in_trap = false;
    }

    /// Fires the EXIT trap once.
    pub(crate) fn run_exit_trap(&mut self, io: &mut Io<'_>, host: &mut dyn Host) {
        if self.traps.contains_key("EXIT") {
            self.run_trap("EXIT", io, host);
            self.traps.remove("EXIT");
        }
    }

    /// Fires the trap for an interrupt.
    pub(crate) fn run_interrupt_trap(&mut self, io: &mut Io<'_>, host: &mut dyn Host) {
        self.run_trap("INT", io, host);
    }

    /// Fires the ERR trap after a command failed outside a condition.
    pub(crate) fn run_err_trap(&mut self, io: &mut Io<'_>, host: &mut dyn Host) {
        if self.errexit_off == 0 {
            self.run_trap("ERR", io, host);
        }
    }

}

/// What a stopped job's status reads as: 128 plus SIGTSTP.
pub(crate) const SUSPENDED_STATUS: i32 = 148;

/// The signals a trap can catch, and what the trap table calls them.
const CAUGHT_SIGNALS: &[(u32, &str)] = &[(1, "HUP"), (2, "INT"), (3, "QUIT"), (15, "TERM")];

/// A trap's name from any of the ways to write it.
fn signal_name(text: &str) -> Option<&'static str> {
    let text = text.strip_prefix("SIG").unwrap_or(text);
    match text {
        "0" | "EXIT" => Some("EXIT"),
        "1" | "HUP" => Some("HUP"),
        "2" | "INT" => Some("INT"),
        "3" | "QUIT" => Some("QUIT"),
        "15" | "TERM" => Some("TERM"),
        "ERR" => Some("ERR"),
        _ => None,
    }
}

/// Reborrows a shell whose lifetime is already `'static`.
trait AsStatic {
    fn as_static(&mut self) -> &mut Shell<'static>;
}

impl AsStatic for Shell<'static> {
    fn as_static(&mut self) -> &mut Shell<'static> {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;
    use std::collections::BTreeMap;
    use std::string::String;
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::vec::Vec;

    use feos_coreutils::{BytesOutput, Input, NoSystem, NullInput, NullOutput, Output, System};

    use crate::{CancelToken, ConsoleStreams, Host, Io, JobStatus, Shell, TaskHost};

    struct ChannelInput {
        receiver: Receiver<Vec<u8>>,
        pending: Vec<u8>,
    }

    impl Input for ChannelInput {
        fn read(&mut self, out: &mut [u8]) -> usize {
            if self.pending.is_empty() {
                match self.receiver.recv() {
                    Ok(bytes) => self.pending = bytes,
                    Err(_) => return 0,
                }
            }
            let count = self.pending.len().min(out.len());
            out[..count].copy_from_slice(&self.pending[..count]);
            self.pending.drain(..count);
            count
        }
    }

    struct ChannelOutput {
        sender: SyncSender<Vec<u8>>,
        broken: bool,
    }

    impl Output for ChannelOutput {
        fn write(&mut self, bytes: &[u8]) {
            self.broken |= self.sender.send(bytes.to_vec()).is_err();
        }

        fn is_broken(&self) -> bool {
            self.broken
        }
    }

    static SYSTEM: NoSystem = NoSystem;

    /// Tasks as threads, each with a stack about as small as the kernel's.
    #[derive(Clone, Default)]
    struct ThreadHost {
        next: Arc<Mutex<u32>>,
        tasks: Arc<Mutex<BTreeMap<u32, JoinHandle<i32>>>>,
        done: Arc<Mutex<BTreeMap<u32, i32>>>,
        cancel: CancelToken,
    }

    impl Host for ThreadHost {
        fn run_external(
            &mut self,
            _name: &str,
            _args: &[String],
            _stdin: &mut dyn Input,
            _stdout: &mut dyn Output,
            _stderr: &mut dyn Output,
            _background: bool,
        ) -> Option<i32> {
            None
        }

        fn poll_cancel(&mut self) -> bool {
            self.cancel.is_cancelled()
        }

        fn tasks(&mut self) -> Option<&mut dyn TaskHost> {
            Some(self)
        }
    }

    impl TaskHost for ThreadHost {
        fn pipe(&mut self) -> Option<(Box<dyn Input + Send>, Box<dyn Output + Send>)> {
            let (sender, receiver) = sync_channel(8);
            Some((
                Box::new(ChannelInput {
                    receiver,
                    pending: Vec::new(),
                }),
                Box::new(ChannelOutput {
                    sender,
                    broken: false,
                }),
            ))
        }

        fn console_streams(&mut self) -> ConsoleStreams {
            (Box::new(NullInput), Box::new(NullOutput), Box::new(NullOutput))
        }

        fn fork_host(&mut self, cancel: CancelToken) -> Box<dyn Host + Send> {
            let mut host = self.clone();
            host.cancel = cancel;
            Box::new(host)
        }

        fn spawn(
            &mut self,
            _name: &str,
            _background: bool,
            job: Box<dyn FnOnce() -> i32 + Send>,
        ) -> Option<u32> {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let pid = *next + 100;
            let thread = std::thread::Builder::new()
                .stack_size(1024 * 1024)
                .spawn(job)
                .ok()?;
            self.tasks.lock().unwrap().insert(pid, thread);
            Some(pid)
        }

        fn status(&mut self, pid: u32) -> JobStatus {
            if let Some(code) = self.done.lock().unwrap().get(&pid) {
                return JobStatus::Exited(*code);
            }
            let mut tasks = self.tasks.lock().unwrap();
            let finished = tasks.get(&pid).map(JoinHandle::is_finished);
            match finished {
                None => JobStatus::Unknown,
                Some(false) => JobStatus::Running,
                Some(true) => {
                    let code = tasks.remove(&pid).unwrap().join().unwrap_or(99);
                    self.done.lock().unwrap().insert(pid, code);
                    JobStatus::Exited(code)
                }
            }
        }

        fn kill(&mut self, _pid: u32) {}

        fn set_stopped(&mut self, _pid: u32, _stopped: bool) {}

        fn pause(&mut self) {
            std::thread::yield_now();
        }

        fn system(&mut self) -> &'static dyn System {
            &SYSTEM
        }
    }

    fn run_lines(lines: &[&str]) -> (i32, String, String) {
        let mut shell = Shell::new(None, &SYSTEM);
        let mut host = ThreadHost::default();
        let mut stdin = feos_coreutils::BytesInput::empty();
        let mut out = BytesOutput::new();
        let mut err = BytesOutput::new();
        let mut status = 0;
        {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
            };
            for line in lines {
                status = shell.execute_line(line, &mut io, &mut host);
            }
        }
        (
            status,
            String::from_utf8_lossy(out.bytes()).into_owned(),
            String::from_utf8_lossy(err.bytes()).into_owned(),
        )
    }

    fn out(lines: &[&str]) -> String {
        run_lines(lines).1
    }

    #[test]
    fn an_endless_producer_stops_when_its_reader_goes() {
        assert_eq!(out(&["yes | head -2"]), "y\ny\n");
        assert_eq!(out(&["while :; do echo x; done | head -1"]), "x\n");
    }

    #[test]
    fn pipelines_stream_everything_through() {
        assert_eq!(out(&["seq 1 20000 | wc -l"]).trim(), "20000");
        assert_eq!(out(&["seq 1 3 | sed s/2/two/ | tr a-z A-Z"]), "1\nTWO\n3\n");
    }

    #[test]
    fn a_pipelines_status_is_its_last_stage_unless_pipefail() {
        assert_eq!(run_lines(&["false | true"]).0, 0);
        assert_eq!(run_lines(&["true | false"]).0, 1);
        assert_eq!(run_lines(&["set -o pipefail", "false | true"]).0, 1);
        assert_eq!(out(&["! false | false; echo $?"]), "0\n");
    }

    #[test]
    fn stages_are_subshells() {
        assert_eq!(out(&["x=1 | true; echo [$x]"]), "[]\n");
        assert_eq!(out(&["x=2; echo $x | cat"]), "2\n");
        assert_eq!(out(&["cd /tmp | true; pwd"]), "/\n");
        assert_eq!(out(&["f() { echo in-$1; }; f a | cat"]), "in-a\n");
    }

    #[test]
    fn redirected_stages_still_work() {
        assert_eq!(out(&["cat <<EOF | tr a b\naaa\nEOF"]), "bbb\n");
        assert_eq!(out(&["echo hi > /tmp/j; cat < /tmp/j | cat"]), "hi\n");
    }

    #[test]
    fn wait_returns_a_jobs_status() {
        assert_eq!(out(&["(exit 3) & wait $!; echo $?"]), "3\n");
        assert_eq!(out(&["true & x=$!; wait $x; echo $?"]), "0\n");
        assert_eq!(out(&["false & wait; echo $?"]), "0\n");
        assert_eq!(run_lines(&["wait 99"]).0, 127);
    }

    #[test]
    fn jobs_lists_what_is_running_and_fg_waits_for_it() {
        let lines = out(&["while :; do :; done &", "jobs"]);
        assert!(lines.contains("[1]+ Running"), "{lines}");
        assert!(lines.contains("while") || lines.contains("sh"), "{lines}");

        let (status, stdout, _) = run_lines(&["(exit 7) &", "fg"]);
        assert_eq!(status, 7, "{stdout}");
        assert_eq!(run_lines(&["fg"]).0, 1);
        assert_eq!(run_lines(&["bg %4"]).0, 1);
    }

    #[test]
    fn an_interactive_shell_reports_jobs_it_starts_and_finishes() {
        let (_, _, err) = run_lines(&["true &", "wait; :", ":"]);
        assert!(err.starts_with("[1] "), "{err}");
    }

    #[test]
    fn a_stage_that_writes_nothing_finishes_even_if_its_reader_went_first() {
        // The stage that creates the file may not start until `true` is done.
        let mut shell = Shell::new(None, &SYSTEM);
        let mut host = ThreadHost::default();
        let mut stdin = feos_coreutils::BytesInput::empty();
        let mut out = BytesOutput::new();
        let mut err = BytesOutput::new();
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut out,
            stderr: &mut err,
        };
        let status = shell.execute_line("set -o pipefail; (:; :) | true", &mut io, &mut host);
        assert_eq!(status, 0);
    }

    #[test]
    fn a_signal_fires_its_trap_before_the_next_command() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut host = SignalHost { pending: 0, mask: 0 };
        let mut out = BytesOutput::new();
        let mut err = BytesOutput::new();
        let mut input = NullInput;
        let mut io = Io { stdin: &mut input, stdout: &mut out, stderr: &mut err };
        shell.execute_line("trap 'echo got-term' TERM; trap 'echo got-hup' HUP; trap 'echo got-quit' 3", &mut io, &mut host);
        assert_eq!(host.mask, 1 << 15 | 1 << 1 | 1 << 3);
        host.pending = 1 << 15 | 1 << 3;
        shell.execute_line("echo next", &mut io, &mut host);
        assert_eq!(host.pending, 0);
        shell.execute_line("trap - TERM HUP QUIT", &mut io, &mut host);
        assert_eq!(host.mask, 0);
        assert_eq!(String::from_utf8_lossy(out.bytes()), "got-quit\ngot-term\nnext\n");
    }

    #[test]
    fn a_hang_up_runs_the_hup_and_exit_traps() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut host = SignalHost { pending: 0, mask: 0 };
        let mut out = BytesOutput::new();
        let mut err = BytesOutput::new();
        let mut input = NullInput;
        let mut io = Io { stdin: &mut input, stdout: &mut out, stderr: &mut err };
        shell.execute_line("trap 'echo hup' HUP; trap 'echo bye' EXIT", &mut io, &mut host);
        shell.hang_up(&mut io, &mut host);
        assert_eq!(String::from_utf8_lossy(out.bytes()), "hup\nbye\n");
    }

    struct SignalHost {
        pending: u32,
        mask: u32,
    }

    impl Host for SignalHost {
        fn run_external(
            &mut self,
            _name: &str,
            _args: &[String],
            _stdin: &mut dyn Input,
            _stdout: &mut dyn Output,
            _stderr: &mut dyn Output,
            _background: bool,
        ) -> Option<i32> {
            None
        }

        fn catch_signals(&mut self, mask: u32) {
            self.mask = mask;
        }

        fn take_signals(&mut self) -> u32 {
            core::mem::take(&mut self.pending)
        }
    }

    #[test]
    fn time_reports_on_standard_error_in_either_format() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut host = SignalHost { pending: 0, mask: 0 };
        let mut out = BytesOutput::new();
        let mut err = BytesOutput::new();
        let mut input = NullInput;
        let mut io = Io { stdin: &mut input, stdout: &mut out, stderr: &mut err };
        assert_eq!(shell.execute_line("time false", &mut io, &mut host), 1);
        assert_eq!(shell.execute_line("time -p echo hi | cat", &mut io, &mut host), 0);
        assert_eq!(shell.execute_line("time", &mut io, &mut host), 0);
        assert_eq!(shell.execute_line("! time true", &mut io, &mut host), 1);
        assert_eq!(String::from_utf8_lossy(out.bytes()), "hi\n");
        let report = String::from_utf8_lossy(err.bytes()).into_owned();
        assert!(report.starts_with("\nreal\t0m0.000s\nuser\t0m0.000s\nsys\t0m0.000s\n"), "{report}");
        assert!(report.contains("real 0.00\nuser 0.00\nsys 0.00\n"), "{report}");
    }

    #[test]
    fn traps_fire_and_can_be_cleared() {
        assert_eq!(out(&["sh -c 'trap \"echo bye\" EXIT; echo hi'"]), "hi\nbye\n");
        assert_eq!(out(&["(trap 'echo left' EXIT; echo in); echo out"]), "in\nleft\nout\n");
        assert_eq!(out(&["sh -c 'trap \"echo no\" EXIT; trap - EXIT'"]), "");
        assert_eq!(out(&["trap 'echo err-$?' ERR; false; echo next"]), "err-1\nnext\n");
        assert_eq!(out(&["trap 'echo err' ERR; if false; then :; fi; false || true"]), "");
        assert_eq!(out(&["trap 'echo x' INT; trap -p INT"]), "trap -- 'echo x' INT\n");
        assert_eq!(run_lines(&["trap 'x' NOPE"]).0, 1);
    }

    #[test]
    fn a_hosts_without_tasks_still_runs_pipelines_and_background_lists() {
        let mut shell = Shell::new(None, &SYSTEM);
        let (_, stdout, _) = crate::tests_support::run(
            &mut shell,
            &mut crate::tests_support::NoHost,
            "echo a | cat; echo b &",
        );
        assert_eq!(stdout, "a\nb\n");
    }
}
