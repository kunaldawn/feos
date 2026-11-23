//! Programs as processes, for a host that offers [`Processes`]: the user-mode `/bin/sh`.
//!
//! A program's descriptors are made from the shell's table by spawn actions, so one whose
//! output goes to the shell's own standard output writes there itself, a file redirection
//! is opened by the child, and a pipeline of programs is real pipes between processes that
//! all run at once. Only what the shell holds in memory (a here-document, a file it read in
//! full, a command substitution's capture) goes through a pipe the shell feeds or drains
//! while the program runs. Every descriptor the shell has of its own is closed in the
//! child: the actions end by closing everything above the last one the table names, or
//! above the last one the shell inherited and passes on, whichever is higher. (The shell's
//! own descriptors are all close-on-exec, so those between go at the program's `EXEC`.)
//!
//! With job control each job gets a process group of its own, which is the terminal's
//! foreground group while the shell waits for it, so Ctrl-C and Ctrl-Z reach the job and
//! not the shell.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_coreutils::{is_command, Vfs};
use feos_coreutils::process::{
    errno, signal, Action, Group, OpenMode, PollEntry, Processes, Spawn, WaitStatus,
};

use crate::fds::{FdTable, OpenFile, MAX_FDS};
use crate::jobs::{Job, Member, SUSPENDED_STATUS};
use crate::lexer::Word;
use crate::parser::Command;
use crate::{Flow, Host, Io, JobStatus, Shell};

/// Where one of a child's descriptors comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    /// One of the shell's own descriptors.
    Real(i32),
    /// A file opened afresh.
    Open(String, OpenMode),
    /// The same open file as an earlier descriptor of the child's.
    Same(i32),
    /// None.
    Closed,
}

/// Bytes the shell moves through a pipe while a program runs.
pub(crate) enum Pump {
    /// Into the program: what the shell holds for its input.
    Feed { fd: i32, data: Vec<u8>, position: usize },
    /// Out of it, into the capture at `index`.
    Drain { fd: i32, index: u16 },
}

impl Pump {
    fn fd(&self) -> i32 {
        match self {
            Self::Feed { fd, .. } | Self::Drain { fd, .. } => *fd,
        }
    }
}

/// How a child's descriptors are made, and the pipes made for it.
pub(crate) struct Plan {
    pub(crate) actions: Vec<Action>,
    /// The pipe ends only the child keeps: the shell closes its copies once it started.
    pub(crate) child_ends: Vec<i32>,
    pub(crate) pumps: Vec<Pump>,
}

impl Plan {
    /// Closes every pipe end it made, for a program that never started.
    fn abandon(self, backend: &mut dyn Processes) {
        for fd in self.child_ends {
            backend.close(fd);
        }
        for pump in self.pumps {
            backend.close(pump.fd());
        }
    }
}

/// A program the pipeline being started wants as one of its stages: started in `group`,
/// not waited for, its pid left in `started`.
pub(crate) struct StageSpawn {
    /// The interpreter's nesting depth at the stage, so a command substitution in the
    /// stage's words runs its own programs as usual.
    depth: usize,
    group: Group,
    foreground: bool,
    started: Option<u32>,
}

/// What became of the processes of a job the shell waited for.
struct Outcome {
    /// Each member's `$?`, `None` for one still running or stopped.
    statuses: Vec<Option<i32>>,
    /// One of them stopped.
    stopped: bool,
    /// One of them ended by `SIGINT`.
    interrupted: bool,
}

/// Works out how a child's descriptors 0 to 9 are made from the shell's table, opening
/// the pipes it needs. `null_stdin`: a background program of a shell without job control
/// reads `/dev/null`, as POSIX has it, not the terminal.
pub(crate) fn plan(
    fds: &mut FdTable,
    backend: &mut dyn Processes,
    null_stdin: bool,
) -> Result<Plan, i32> {
    let mut plan = Plan {
        actions: Vec::new(),
        child_ends: Vec::new(),
        pumps: Vec::new(),
    };
    let mut sources: Vec<Source> = Vec::with_capacity(MAX_FDS);
    let mut firsts: Vec<(u16, i32)> = Vec::new();
    for fd in 0..MAX_FDS {
        let Some(index) = fds.slot(fd) else {
            sources.push(Source::Closed);
            continue;
        };
        if let Some(&(_, first)) = firsts.iter().find(|(seen, _)| *seen == index) {
            sources.push(Source::Same(first));
            continue;
        }
        firsts.push((index, fd as i32));
        let source = match fds.file(index) {
            Some(OpenFile::Base(0)) if null_stdin && fd == 0 => {
                Source::Open(String::from("/dev/null"), OpenMode::Read)
            }
            Some(OpenFile::Base(n)) => Source::Real(i32::from(*n)),
            Some(OpenFile::Descriptor(real)) => Source::Real(*real),
            Some(OpenFile::Null) => Source::Open(String::from("/dev/null"), OpenMode::ReadWrite),
            Some(OpenFile::Zero) => Source::Open(String::from("/dev/zero"), OpenMode::Read),
            // The shell made or truncated it when the redirection was opened, and wrote
            // what it had for it just now.
            Some(OpenFile::Writer { path, .. }) => Source::Open(path.clone(), OpenMode::Append),
            Some(OpenFile::Reader { data, position }) => {
                // The program reads the rest, so it's gone from the shell's copy.
                let mut rest = core::mem::take(data);
                rest.drain(..(*position).min(rest.len()));
                *position = 0;
                let (read, write) = match backend.pipe() {
                    Ok(pair) => pair,
                    Err(error) => {
                        plan.abandon(backend);
                        return Err(error);
                    }
                };
                backend.set_nonblocking(write);
                plan.child_ends.push(read);
                plan.pumps.push(Pump::Feed {
                    fd: write,
                    data: rest,
                    position: 0,
                });
                Source::Real(read)
            }
            Some(OpenFile::Capture { .. }) => {
                let (read, write) = match backend.pipe() {
                    Ok(pair) => pair,
                    Err(error) => {
                        plan.abandon(backend);
                        return Err(error);
                    }
                };
                plan.child_ends.push(write);
                plan.pumps.push(Pump::Drain { fd: read, index });
                Source::Real(write)
            }
            // Another task's stream: only a host with tasks makes these, and it doesn't
            // start programs this way.
            Some(OpenFile::Stream(_)) | None => {
                Source::Open(String::from("/dev/null"), OpenMode::Read)
            }
        };
        sources.push(source);
    }
    plan.actions = actions_for(&sources, backend.inherited_above());
    Ok(plan)
}

/// The actions that make a child's descriptors `sources`, in order: copies and opens,
/// then closes, then everything above the highest descriptor named closed, or above
/// `inherited`, the highest one past the table the shell passes on, if that's higher.
fn actions_for(sources: &[Source], inherited: Option<i32>) -> Vec<Action> {
    let mut actions = Vec::new();
    // A descriptor of the shell's that one child descriptor copies but an earlier one
    // replaces first is copied out of the way, above every descriptor any source names.
    let highest = sources
        .iter()
        .filter_map(|source| match source {
            Source::Real(fd) => Some(*fd),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .max(MAX_FDS as i32 - 1)
        // A copy out of the way mustn't land on an inherited descriptor the child keeps.
        .max(inherited.unwrap_or(0));
    let mut moved: Vec<(i32, i32)> = Vec::new();
    for (target, source) in sources.iter().enumerate() {
        let Source::Real(fd) = *source else {
            continue;
        };
        let replaced = usize::try_from(fd)
            .ok()
            .and_then(|at| sources.get(at))
            .is_some_and(|at| *at != Source::Real(fd) && *at != Source::Closed);
        if replaced && (fd as usize) < target && !moved.iter().any(|(from, _)| *from == fd) {
            let to = highest + 1 + moved.len() as i32;
            actions.push(Action::Dup2 { from: fd, to });
            moved.push((fd, to));
        }
    }
    let mut last_open = None;
    for (target, source) in sources.iter().enumerate() {
        let to = target as i32;
        match source {
            Source::Real(fd) => {
                let from = moved
                    .iter()
                    .find(|(original, _)| original == fd)
                    .map_or(*fd, |(_, copy)| *copy);
                // The shell's own standard streams are already where they go. A pipe end
                // that happens to be its own target is still copied onto itself, which
                // clears its close-on-exec flag.
                if from != to || to > 2 {
                    actions.push(Action::Dup2 { from, to });
                }
            }
            Source::Open(path, mode) => actions.push(Action::Open {
                fd: to,
                path: path.clone(),
                mode: *mode,
            }),
            Source::Same(first) => actions.push(Action::Dup2 { from: *first, to }),
            Source::Closed => continue,
        }
        last_open = Some(to);
    }
    // Past an inherited descriptor kept above the table, a closed one in the table (an
    // inherited one a redirection closed, say) is closed by itself.
    let first_free = last_open.map_or(0, |fd| fd + 1).max(inherited.map_or(0, |fd| fd + 1));
    for (target, source) in sources.iter().enumerate() {
        if *source == Source::Closed && (target as i32) < first_free {
            actions.push(Action::Close(target as i32));
        }
    }
    actions.push(Action::CloseFrom(first_free));
    actions
}

/// Moves bytes through the pipes of a program the shell started, until each is done:
/// a feed when it has given everything or its reader has gone, a drain at its end.
fn pump(fds: &mut FdTable, backend: &mut dyn Processes, mut pumps: Vec<Pump>) {
    let mut buffer = alloc::vec![0u8; 4096];
    while !pumps.is_empty() {
        // A feed with nothing left to give is closed at once, so its reader sees its end.
        pumps.retain(|pump| match pump {
            Pump::Feed { fd, data, position } if *position >= data.len() => {
                backend.close(*fd);
                false
            }
            _ => true,
        });
        if pumps.is_empty() {
            break;
        }
        let mut entries: Vec<PollEntry> = pumps
            .iter()
            .map(|pump| PollEntry {
                fd: pump.fd(),
                read: matches!(pump, Pump::Drain { .. }),
                write: matches!(pump, Pump::Feed { .. }),
                ready: false,
            })
            .collect();
        match backend.poll(&mut entries, -1) {
            Ok(_) | Err(errno::EINTR) => {}
            Err(_) => break,
        }
        let mut keep = Vec::with_capacity(pumps.len());
        for (mut pump, entry) in pumps.into_iter().zip(entries) {
            if !entry.ready {
                keep.push(pump);
                continue;
            }
            let done = match &mut pump {
                Pump::Feed { fd, data, position } => {
                    let end = data.len().min(*position + buffer.len());
                    match backend.write(*fd, &data[*position..end]) {
                        Ok(count) => {
                            *position += count;
                            false
                        }
                        Err(errno::EAGAIN | errno::EINTR) => false,
                        // Its reader went without reading it all (`EPIPE`).
                        Err(_) => true,
                    }
                }
                Pump::Drain { fd, index } => match backend.read(*fd, &mut buffer) {
                    Ok(0) => true,
                    Ok(count) => {
                        fds.append_capture(*index, &buffer[..count]);
                        false
                    }
                    Err(errno::EAGAIN | errno::EINTR) => false,
                    Err(_) => true,
                },
            };
            if done {
                backend.close(pump.fd());
            } else {
                keep.push(pump);
            }
        }
        pumps = keep;
    }
    for pump in pumps {
        backend.close(pump.fd());
    }
}

/// A signal's number from how `kill` and `trap` write it: `TERM`, `SIGTERM` or `15`.
pub(crate) fn signal_number(text: &str) -> Option<u32> {
    if let Ok(number) = text.parse::<u32>() {
        return (number <= 64).then_some(number);
    }
    let name = text.strip_prefix("SIG").unwrap_or(text);
    SIGNAL_NAMES
        .iter()
        .position(|known| known.eq_ignore_ascii_case(name))
        .map(|index| index as u32 + 1)
}

/// Linux's signal names, from 1.
const SIGNAL_NAMES: &[&str] = &[
    "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV", "USR2",
    "PIPE", "ALRM", "TERM", "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU", "URG",
    "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO", "PWR", "SYS",
];

impl Shell<'_> {
    /// Runs a program as a process: started with its descriptors made from this shell's,
    /// then waited for, unless it's a stage of a pipeline being started, a background
    /// job, or what `exec` replaces this shell with.
    pub(crate) fn spawn_program(
        &mut self,
        path: &str,
        absolute: &str,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        // What the shell wrote to a file is in it before what the program writes.
        self.flush_writers(io);
        let exec = core::mem::take(&mut self.exec_next);
        let stage = self
            .stage
            .as_ref()
            .filter(|stage| stage.depth == self.depth && stage.started.is_none())
            .map(|stage| (stage.group, stage.foreground));
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(path.to_string());
        argv.extend(args.iter().cloned());
        let env = self.env.entries();
        let cwd = self.fs.cwd().to_string();
        let Some(backend) = host.processes() else {
            return 126;
        };
        let job_control = backend.job_control();
        let null_stdin = background && !job_control && stage.is_none();
        let plan = match plan(&mut self.fds, backend, null_stdin) {
            Ok(plan) => plan,
            Err(_) => {
                self.error(io, &format!("{path}: no pipe for its streams"));
                return 1;
            }
        };
        let (group, foreground) = match stage {
            Some(stage) => stage,
            None if job_control && !exec => (Group::New, !background),
            None => (Group::Inherit, false),
        };
        let request = Spawn {
            path: absolute,
            argv: &argv,
            env: &env,
            cwd: &cwd,
            actions: &plan.actions,
            group,
            foreground,
        };
        // A program fed from the shell's memory can't replace the shell: it's run as a
        // child, and the shell ends with its status, which is all `exec` could tell.
        if exec && plan.pumps.is_empty() {
            let error = backend.exec(&request);
            plan.abandon(backend);
            return self.spawn_failed(path, error, io);
        }
        let pid = match backend.spawn(&request) {
            Ok(pid) => pid,
            Err(error) => {
                plan.abandon(backend);
                return self.spawn_failed(path, error, io);
            }
        };
        for fd in &plan.child_ends {
            backend.close(*fd);
        }
        pump(&mut self.fds, backend, plan.pumps);
        if stage.is_some() {
            if let Some(stage) = self.stage.as_mut() {
                stage.started = Some(pid);
            }
            return 0;
        }
        let mut text = path.to_string();
        for arg in args {
            text.push(' ');
            text.push_str(arg);
        }
        if background {
            self.add_job(alloc::vec![pid], text, false, io);
            return 0;
        }
        let outcome = self.wait_members(&[pid], host, foreground);
        self.finish_job(alloc::vec![pid], outcome, text, io)
    }

    /// What a program that didn't start leaves in `$?`, with its message.
    fn spawn_failed(&mut self, path: &str, error: i32, io: &mut Io<'_>) -> i32 {
        let (text, status) = match error {
            errno::ENOENT => ("No such file or directory", 127),
            errno::EACCES => ("Permission denied", 126),
            errno::ENOEXEC => ("cannot execute binary file", 126),
            errno::EAGAIN => ("no room for another process", 126),
            _ => ("cannot execute", 126),
        };
        self.error(io, &format!("{path}: {text}"));
        status
    }

    /// The status a job the shell waited for leaves: its last member's, or with
    /// `pipefail` the last that failed. A stopped job stays a job; one ended by Ctrl-C
    /// ends the line in an interactive shell, as if the shell had had the `SIGINT`.
    fn finish_job(&mut self, pids: Vec<u32>, outcome: Outcome, text: String, io: &mut Io<'_>) -> i32 {
        if outcome.stopped {
            let members = pids
                .iter()
                .zip(&outcome.statuses)
                .map(|(&pid, status)| Member { pid, status: *status })
                .collect();
            let id = self.push_job(members, text.clone(), true);
            self.write_err(io, &format!("\n[{id}]+  Stopped  {text}\n"));
            return SUSPENDED_STATUS;
        }
        if outcome.interrupted && self.interactive {
            self.cancel.cancel();
            self.flow = Flow::Interrupted;
            return 130;
        }
        let statuses: Vec<i32> = outcome.statuses.iter().map(|status| status.unwrap_or(0)).collect();
        pipeline_status(&statuses, self.opts.pipefail)
    }

    /// Adds a job of programs that are running (or stopped), reporting a background one
    /// as Bash does in an interactive shell.
    fn add_job(&mut self, pids: Vec<u32>, text: String, stopped: bool, io: &mut Io<'_>) {
        let leader = pids.first().copied().unwrap_or(0);
        let members = pids.into_iter().map(|pid| Member { pid, status: None }).collect();
        let id = self.push_job(members, text, stopped);
        if self.interactive {
            self.write_err(io, &format!("[{id}] {leader}\n"));
        }
    }

    fn push_job(&mut self, members: Vec<Member>, text: String, stopped: bool) -> usize {
        self.next_job += 1;
        let id = self.next_job;
        let pid = members.first().map_or(0, |member: &Member| member.pid);
        self.jobs.push(Job {
            id,
            pid,
            text,
            stopped,
            members,
        });
        self.last_background = Some(pid);
        id
    }

    /// Waits for each of `pids` to end, or for one to stop. `foreground`: the job had the
    /// terminal, which comes back to the shell after.
    fn wait_members(&mut self, pids: &[u32], host: &mut dyn Host, foreground: bool) -> Outcome {
        let mut outcome = Outcome {
            statuses: alloc::vec![None; pids.len()],
            stopped: false,
            interrupted: false,
        };
        let Some(backend) = host.processes() else {
            return outcome;
        };
        for (index, &pid) in pids.iter().enumerate() {
            match wait_one(backend, pid) {
                Some(status) if status.is_final() => {
                    outcome.interrupted |= status == WaitStatus::Signaled(signal::INT);
                    outcome.statuses[index] = Some(status.shell_status());
                }
                Some(_) => {
                    outcome.stopped = true;
                    break;
                }
                None => outcome.statuses[index] = Some(127),
            }
        }
        if foreground {
            backend.take_terminal();
        }
        outcome
    }

    /// Whether a pipeline stage is a program: a plain name that isn't a function,
    /// builtin, utility or host command, naming an ELF file. Decided from the words as
    /// written, so nothing is expanded twice.
    pub(crate) fn is_program_stage(&mut self, command: &Command, host: &mut dyn Host) -> bool {
        let Command::Simple(simple) = command else {
            return false;
        };
        let Some(name) = simple.words.first().and_then(Word::plain) else {
            return false;
        };
        if self.functions.contains_key(name)
            || crate::builtin_names().contains(&name)
            || is_command(name)
            || host.has_command(name)
        {
            return false;
        }
        let path = if name.contains('/') {
            name.to_string()
        } else {
            match self.find_in_path(name) {
                Some(path) => path,
                None => return false,
            }
        };
        self.is_program(&path)
    }

    /// Runs a pipeline whose every stage is a program as processes joined by pipes, all at
    /// once, in one process group under job control. `None` when the host doesn't start
    /// processes or a stage isn't a program: the buffered path runs it then.
    pub(crate) fn run_program_pipeline(
        &mut self,
        commands: &[Command],
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> Option<i32> {
        let job_control = host.processes()?.job_control();
        for command in commands {
            if !self.is_program_stage(command, host) {
                return None;
            }
        }
        let count = commands.len();
        // Each stage's pid, or the status of one that didn't start.
        let mut started: Vec<Result<u32, i32>> = Vec::with_capacity(count);
        let mut leader: Option<u32> = None;
        let mut previous: Option<i32> = None;
        for (index, command) in commands.iter().enumerate() {
            let next = if index + 1 < count {
                match host.processes()?.pipe() {
                    Ok(pair) => Some(pair),
                    Err(_) => {
                        self.error(io, "no pipe for this pipeline");
                        started.push(Err(1));
                        break;
                    }
                }
            } else {
                None
            };
            let saved = self.fds.save();
            let mut leftovers = Vec::new();
            if let Some(read) = previous {
                let file = self.fds.open(OpenFile::Descriptor(read));
                self.fds.bind(0, file, &mut leftovers);
            }
            if let Some((_, write)) = next {
                let file = self.fds.open(OpenFile::Descriptor(write));
                self.fds.bind(1, file, &mut leftovers);
            }
            let group = match (job_control, leader) {
                (false, _) => Group::Inherit,
                (true, None) => Group::New,
                (true, Some(pgid)) => Group::Join(pgid),
            };
            self.stage = Some(StageSpawn {
                depth: self.depth,
                group,
                foreground: job_control && !background && leader.is_none(),
                started: None,
            });
            let status = self.run_command(command, io, host, false);
            let stage = self.stage.take();
            self.finish_redirects(saved, io);
            self.flush_leftovers(leftovers, io);
            // The shell's copies of this stage's ends go now: the reader of the pipe it
            // writes sees its end when the stage's own copy goes, and the writer of the
            // one it reads gets `EPIPE` once the stage does.
            let backend = host.processes()?;
            if let Some(read) = previous.take() {
                backend.close(read);
            }
            if let Some((read, write)) = next {
                backend.close(write);
                previous = Some(read);
            }
            match stage.and_then(|stage| stage.started) {
                Some(pid) => {
                    leader.get_or_insert(pid);
                    started.push(Ok(pid));
                }
                None => started.push(Err(status)),
            }
        }
        if let Some(read) = previous {
            host.processes()?.close(read);
        }
        let pids: Vec<u32> = started.iter().filter_map(|result| result.ok()).collect();
        let text = commands
            .iter()
            .map(crate::jobs::describe_command)
            .collect::<Vec<_>>()
            .join(" | ");
        if background {
            if !pids.is_empty() {
                self.add_job(pids, text, false, io);
            }
            return Some(0);
        }
        let outcome = self.wait_members(&pids, host, job_control && leader.is_some());
        if outcome.stopped || (outcome.interrupted && self.interactive) {
            return Some(self.finish_job(pids, outcome, text, io));
        }
        let mut waited = outcome.statuses.into_iter();
        let statuses: Vec<i32> = started
            .iter()
            .map(|result| match result {
                Ok(_) => waited.next().flatten().unwrap_or(0),
                Err(status) => *status,
            })
            .collect();
        Some(pipeline_status(&statuses, self.opts.pipefail))
    }

    /// What a job of processes is doing now, looking without waiting. `None` for a job of
    /// another kind.
    pub(crate) fn process_job_status(&mut self, index: usize, host: &mut dyn Host) -> Option<JobStatus> {
        let job = self.jobs.get_mut(index)?;
        if job.members.is_empty() {
            return None;
        }
        let Some(backend) = host.processes() else {
            return Some(JobStatus::Unknown);
        };
        for member in job.members.iter_mut().filter(|member| member.status.is_none()) {
            match backend.wait(member.pid, false) {
                Ok(Some(status)) if status.is_final() => member.status = Some(status.shell_status()),
                Ok(Some(WaitStatus::Stopped(_))) => job.stopped = true,
                Ok(Some(WaitStatus::Continued)) => job.stopped = false,
                Ok(_) | Err(errno::EINTR) => {}
                Err(_) => member.status = Some(127),
            }
        }
        if job.members.iter().all(|member| member.status.is_some()) {
            let statuses: Vec<i32> = job.members.iter().map(|member| member.status.unwrap_or(0)).collect();
            return Some(JobStatus::Exited(pipeline_status(&statuses, self.opts.pipefail)));
        }
        Some(if job.stopped {
            JobStatus::Stopped
        } else {
            JobStatus::Running
        })
    }

    /// Waits for a job of processes, as `wait` and `fg` do: `fg` (`foreground`) gives it
    /// the terminal and lets it go on first, and a stop leaves it a job.
    pub(crate) fn wait_process_job(
        &mut self,
        index: usize,
        io: &mut Io<'_>,
        host: &mut dyn Host,
        foreground: bool,
    ) -> i32 {
        let pending: Vec<u32> = self.jobs[index]
            .members
            .iter()
            .filter(|member| member.status.is_none())
            .map(|member| member.pid)
            .collect();
        let leader = self.jobs[index].pid;
        let mut job_control = false;
        if let Some(backend) = host.processes() {
            job_control = backend.job_control();
            if foreground {
                if job_control {
                    backend.give_terminal(leader);
                }
                continue_members(backend, job_control, leader, &pending);
            }
        }
        self.jobs[index].stopped = false;
        let outcome = self.wait_members(&pending, host, foreground && job_control);
        let id = self.jobs[index].id;
        let job = &mut self.jobs[index];
        for (pid, status) in pending.iter().zip(&outcome.statuses) {
            if let Some(member) = job.members.iter_mut().find(|member| member.pid == *pid) {
                member.status = member.status.or(*status);
            }
        }
        if outcome.stopped {
            job.stopped = true;
            let line = format!("\n[{id}]+  Stopped  {}\n", job.text);
            self.write_err(io, &line);
            return SUSPENDED_STATUS;
        }
        let statuses: Vec<i32> = job.members.iter().map(|member| member.status.unwrap_or(0)).collect();
        self.jobs.remove(index);
        if outcome.interrupted && self.interactive && foreground {
            self.cancel.cancel();
            self.flow = Flow::Interrupted;
            return 130;
        }
        pipeline_status(&statuses, self.opts.pipefail)
    }

    /// `bg`: lets a stopped job of processes go on.
    pub(crate) fn continue_process_job(&mut self, index: usize, host: &mut dyn Host) {
        let job = &mut self.jobs[index];
        job.stopped = false;
        let pending: Vec<u32> = job
            .members
            .iter()
            .filter(|member| member.status.is_none())
            .map(|member| member.pid)
            .collect();
        if let Some(backend) = host.processes() {
            let job_control = backend.job_control();
            continue_members(backend, job_control, job.pid, &pending);
        }
    }

    /// `kill [-s name | -name | -number] target...` with a host that starts processes:
    /// a target is a pid, `-pgid`, or a job (`%n`, or the pid `$!` gave), which is its
    /// group under job control and each of its processes otherwise. `kill -l` lists the
    /// names.
    pub(crate) fn kill_processes(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let mut number = signal::TERM;
        let mut rest = args;
        match rest.first().map(String::as_str) {
            Some("-l" | "-L") => {
                let mut text = String::new();
                for (index, name) in SIGNAL_NAMES.iter().enumerate() {
                    text.push_str(&format!("{}) SIG{name}\n", index + 1));
                }
                self.write_out(io, &text);
                return 0;
            }
            Some("-s" | "-n") => {
                let Some(found) = rest.get(1).and_then(|name| signal_number(name)) else {
                    self.error(io, "kill: invalid signal specification");
                    return 1;
                };
                number = found;
                rest = &rest[2..];
            }
            Some(option) if option.starts_with('-') && option.len() > 1 && option != "--" => {
                // `-15` and `-TERM` are signals; a negative pid comes after `--` or one.
                let Some(found) = signal_number(&option[1..]) else {
                    self.error(io, &format!("kill: {option}: invalid signal specification"));
                    return 1;
                };
                number = found;
                rest = &rest[1..];
            }
            _ => {}
        }
        if rest.first().map(String::as_str) == Some("--") {
            rest = &rest[1..];
        }
        if rest.is_empty() {
            self.error(io, "kill: usage: kill [-s sigspec | -signum | -sigspec] pid | jobspec ...");
            return 2;
        }
        let job_control = host.processes().is_some_and(|backend| backend.job_control());
        let mut targets: Vec<i32> = Vec::new();
        let mut status = 0;
        for arg in rest {
            let index = if arg.starts_with('%') {
                match self.find_job(Some(arg)) {
                    Some(index) => Some(index),
                    None => {
                        self.error(io, &format!("kill: {arg}: no such job"));
                        status = 1;
                        continue;
                    }
                }
            } else {
                arg.parse::<u32>()
                    .ok()
                    .and_then(|pid| self.jobs.iter().position(|job| job.pid == pid && !job.members.is_empty()))
            };
            match index {
                Some(index) if job_control => targets.push(-(self.jobs[index].pid as i32)),
                Some(index) => targets.extend(
                    self.jobs[index]
                        .members
                        .iter()
                        .filter(|member| member.status.is_none())
                        .map(|member| member.pid as i32),
                ),
                None => match arg.parse::<i32>() {
                    Ok(pid) => targets.push(pid),
                    Err(_) => {
                        self.error(io, &format!("kill: {arg}: arguments must be process or job IDs"));
                        status = 1;
                    }
                },
            }
        }
        let Some(backend) = host.processes() else {
            return 1;
        };
        let mut failed = Vec::new();
        for target in targets {
            if backend.signal(target, number).is_err() {
                failed.push(target);
            }
        }
        for target in failed {
            self.error(io, &format!("kill: ({target}) - No such process"));
            status = 1;
        }
        status
    }
}

/// Sends `SIGCONT` to a job: its group under job control, otherwise each process, since
/// they're in the shell's own group then.
fn continue_members(backend: &mut dyn Processes, job_control: bool, leader: u32, pending: &[u32]) {
    if job_control {
        let _ = backend.signal(-(leader as i32), signal::CONT);
    } else {
        for &pid in pending {
            let _ = backend.signal(pid as i32, signal::CONT);
        }
    }
}

/// Waits for one child to end or stop, through signals that cut the wait short. `None`
/// when there's no such child.
fn wait_one(backend: &mut dyn Processes, pid: u32) -> Option<WaitStatus> {
    loop {
        match backend.wait(pid, true) {
            Ok(Some(WaitStatus::Continued)) | Ok(None) | Err(errno::EINTR) => {}
            Ok(Some(status)) => return Some(status),
            Err(_) => return None,
        }
    }
}

/// A pipeline's status from its stages': the last one's, or with `pipefail` the last
/// that failed.
fn pipeline_status(statuses: &[i32], pipefail: bool) -> i32 {
    if pipefail {
        statuses.iter().rev().find(|status| **status != 0).copied().unwrap_or(0)
    } else {
        statuses.last().copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::NoHost;
    use alloc::collections::BTreeMap;
    use feos_coreutils::{BytesInput, BytesOutput, NoSystem};

    /// What a [`Processes`] mock saw a shell do.
    #[derive(Default)]
    struct Mock {
        next_fd: i32,
        next_pid: u32,
        /// Every request, as path, argv, actions, group and whether it took the terminal.
        spawned: Vec<(String, Vec<String>, Vec<Action>, Group, bool)>,
        /// Descriptors open in the shell: the pipe ends it made, until it closed them.
        open: Vec<i32>,
        closed: Vec<i32>,
        /// What each pid ends with.
        ends: BTreeMap<u32, WaitStatus>,
        /// What each program writes to the pipe its descriptor 1 is, if any.
        output: BTreeMap<String, Vec<u8>>,
        /// The pipe ends readable with this much, by descriptor.
        readable: BTreeMap<i32, Vec<u8>>,
        signals: Vec<(i32, u32)>,
        execs: Vec<String>,
        job_control: bool,
        terminal: Vec<u32>,
        /// What [`Processes::inherited_above`] says.
        inherited_above: Option<i32>,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                next_fd: 3,
                next_pid: 100,
                ..Self::default()
            }
        }
    }

    impl Processes for Mock {
        fn spawn(&mut self, request: &Spawn<'_>) -> Result<u32, i32> {
            if request.path.ends_with("missing") {
                return Err(errno::ENOENT);
            }
            let pid = self.next_pid;
            self.next_pid += 1;
            // A program whose descriptor 1 is a pipe end the shell made writes its output
            // there, for the drain to read.
            let out = request.actions.iter().find_map(|action| match action {
                Action::Dup2 { from, to: 1 } if self.open.contains(from) => Some(*from),
                _ => None,
            });
            if let (Some(fd), Some(bytes)) = (out, self.output.get(request.path)) {
                self.readable.insert(fd - 1, bytes.clone());
            }
            self.spawned.push((
                request.path.to_string(),
                request.argv.to_vec(),
                request.actions.to_vec(),
                request.group,
                request.foreground,
            ));
            Ok(pid)
        }

        fn exec(&mut self, request: &Spawn<'_>) -> i32 {
            self.execs.push(request.path.to_string());
            errno::ENOEXEC
        }

        fn wait(&mut self, pid: u32, block: bool) -> Result<Option<WaitStatus>, i32> {
            // A program runs until it's waited for, unless it was told how it ends.
            match self.ends.remove(&pid) {
                Some(status) => Ok(Some(status)),
                None if block => Ok(Some(WaitStatus::Exited(0))),
                None => Ok(None),
            }
        }

        fn pipe(&mut self) -> Result<(i32, i32), i32> {
            let read = self.next_fd;
            self.next_fd += 2;
            self.open.extend([read, read + 1]);
            Ok((read, read + 1))
        }

        fn set_nonblocking(&mut self, _fd: i32) {}

        fn close(&mut self, fd: i32) {
            assert!(self.open.contains(&fd), "closed {fd}, which isn't open");
            self.open.retain(|open| *open != fd);
            self.closed.push(fd);
        }

        fn read(&mut self, fd: i32, out: &mut [u8]) -> Result<usize, i32> {
            let bytes = self.readable.remove(&fd).unwrap_or_default();
            let count = bytes.len().min(out.len());
            out[..count].copy_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn write(&mut self, _fd: i32, bytes: &[u8]) -> Result<usize, i32> {
            Ok(bytes.len())
        }

        fn poll(&mut self, entries: &mut [PollEntry], _timeout_ms: i32) -> Result<usize, i32> {
            for entry in entries.iter_mut() {
                entry.ready = true;
            }
            Ok(entries.len())
        }

        fn signal(&mut self, pid: i32, signal: u32) -> Result<(), i32> {
            self.signals.push((pid, signal));
            Ok(())
        }

        fn job_control(&self) -> bool {
            self.job_control
        }

        fn inherited_above(&self) -> Option<i32> {
            self.inherited_above
        }

        fn give_terminal(&mut self, pgid: u32) {
            self.terminal.push(pgid);
        }

        fn take_terminal(&mut self) {
            self.terminal.push(0);
        }
    }

    /// A host whose `/bin` has the programs `ptest` and `prog`, started through the mock.
    struct ProcessHost {
        mock: Mock,
    }

    impl Host for ProcessHost {
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

        fn processes(&mut self) -> Option<&mut dyn Processes> {
            Some(&mut self.mock)
        }
    }

    fn shell_with_programs() -> Shell<'static> {
        let mut shell = Shell::new(None, &NoSystem);
        let mut host = NoHost;
        crate::tests_support::run(
            &mut shell,
            &mut host,
            "mkdir /bin; printf '\\177ELF' > /bin/ptest; printf '\\177ELF' > /bin/prog; \
             printf '\\177ELF' > /bin/missing",
        );
        shell
    }

    fn run(shell: &mut Shell<'_>, host: &mut ProcessHost, line: &str) -> (i32, String, String) {
        let mut stdin = BytesInput::empty();
        let mut stdout = BytesOutput::new();
        let mut stderr = BytesOutput::new();
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let status = shell.execute_line(line, &mut io, host);
        (
            status,
            String::from_utf8_lossy(stdout.bytes()).into_owned(),
            String::from_utf8_lossy(stderr.bytes()).into_owned(),
        )
    }

    fn open(fd: i32, path: &str, mode: OpenMode) -> Action {
        Action::Open {
            fd,
            path: path.to_string(),
            mode,
        }
    }

    #[test]
    fn spawn_actions_for_redirections() {
        let mut shell = shell_with_programs();
        let mut host = ProcessHost { mock: Mock::new() };

        // Nothing redirected: the standard streams are the shell's, everything else closed.
        run(&mut shell, &mut host, "ptest one 'two words'");
        let (path, argv, actions, group, foreground) = host.mock.spawned.remove(0);
        assert_eq!(path, "/bin/ptest");
        assert_eq!(argv, ["/bin/ptest", "one", "two words"]);
        assert_eq!(actions, [Action::CloseFrom(3)]);
        assert_eq!((group, foreground), (Group::Inherit, false));

        // `2>/dev/null`, `>file` (made by the shell, opened by the child), `2>&1`.
        run(&mut shell, &mut host, "ptest 2>/dev/null");
        assert_eq!(
            host.mock.spawned.remove(0).2,
            [open(2, "/dev/null", OpenMode::ReadWrite), Action::CloseFrom(3)]
        );
        run(&mut shell, &mut host, "ptest >/tmp/out");
        assert_eq!(
            host.mock.spawned.remove(0).2,
            [open(1, "/tmp/out", OpenMode::Append), Action::CloseFrom(3)]
        );
        assert!(shell.fs.exists("/tmp/out"));
        run(&mut shell, &mut host, "ptest >/tmp/out 2>&1");
        assert_eq!(
            host.mock.spawned.remove(0).2,
            [
                open(1, "/tmp/out", OpenMode::Append),
                Action::Dup2 { from: 1, to: 2 },
                Action::CloseFrom(3)
            ]
        );
        run(&mut shell, &mut host, "ptest 2>&1 >/tmp/out");
        // Standard error is the shell's standard output, which descriptor 1 is replaced
        // after: so that's copied out of the way first.
        assert_eq!(
            host.mock.spawned.remove(0).2,
            [
                Action::Dup2 { from: 1, to: 10 },
                open(1, "/tmp/out", OpenMode::Append),
                Action::Dup2 { from: 10, to: 2 },
                Action::CloseFrom(3)
            ]
        );
        // Swapped streams, a closed one, and a descriptor above 2.
        run(&mut shell, &mut host, "ptest 3>&1 1>&2 2>&3 3>&-");
        assert_eq!(
            host.mock.spawned.remove(0).2,
            [
                Action::Dup2 { from: 1, to: 10 },
                Action::Dup2 { from: 2, to: 1 },
                Action::Dup2 { from: 10, to: 2 },
                Action::CloseFrom(3)
            ]
        );
        run(&mut shell, &mut host, "ptest <&- 4>/tmp/four");
        assert_eq!(
            host.mock.spawned.remove(0).2,
            [
                open(4, "/tmp/four", OpenMode::Append),
                Action::Close(0),
                Action::Close(3),
                Action::CloseFrom(5)
            ]
        );
        // A here-document is fed through a pipe the shell writes, closed when it's done.
        run(&mut shell, &mut host, "ptest <<EOF\nfed\nEOF");
        let actions = host.mock.spawned.remove(0).2;
        assert_eq!(actions, [Action::Dup2 { from: 3, to: 0 }, Action::CloseFrom(3)]);
        assert!(host.mock.open.is_empty(), "left open: {:?}", host.mock.open);
        // Output the shell captures comes back through a pipe it drains.
        host.mock.output.insert("/bin/prog".into(), b"captured\n".to_vec());
        let (status, out, _) = run(&mut shell, &mut host, "x=$(prog); echo \"[$x]\"");
        assert_eq!((status, out.as_str()), (0, "[captured]\n"));
        assert!(host.mock.open.is_empty(), "left open: {:?}", host.mock.open);
        // A program that isn't there.
        let (status, _, err) = run(&mut shell, &mut host, "/bin/missing");
        assert_eq!(status, 127);
        assert!(err.contains("No such file"));
        // `exec` asks for the shell to be replaced, with the same actions.
        let (status, _, _) = run(&mut shell, &mut host, "exec ptest >/tmp/out; echo after");
        assert_eq!(host.mock.execs, ["/bin/ptest"]);
        assert_eq!(status, 126);
    }

    #[test]
    fn inherited_descriptors_reach_programs() {
        // `sh -c prog` started with descriptor 3 open (a jobserver's pipe, say): the
        // program gets it as itself, and only what's above it is closed.
        let mut shell = shell_with_programs();
        shell.inherit_descriptor(3);
        let mut host = ProcessHost { mock: Mock::new() };
        run(&mut shell, &mut host, "ptest");
        assert_eq!(
            host.mock.spawned.remove(0).2,
            [Action::Dup2 { from: 3, to: 3 }, Action::CloseFrom(4)]
        );
        // A redirection can still close it for one program.
        run(&mut shell, &mut host, "ptest 3>&-");
        assert_eq!(host.mock.spawned.remove(0).2, [Action::CloseFrom(3)]);
        run(&mut shell, &mut host, "ptest 4>&3");
        assert_eq!(
            host.mock.spawned.remove(0).2,
            [Action::Dup2 { from: 3, to: 3 }, Action::Dup2 { from: 3, to: 4 }, Action::CloseFrom(5)]
        );

        // One past the table, 12, is kept too: the table's closed descriptors are closed
        // one by one, a copy out of the way goes above 12, and the closing starts there.
        host.mock.inherited_above = Some(12);
        run(&mut shell, &mut host, "ptest");
        let mut expected = alloc::vec![Action::Dup2 { from: 3, to: 3 }];
        expected.extend((4..10).map(Action::Close));
        expected.push(Action::CloseFrom(13));
        assert_eq!(host.mock.spawned.remove(0).2, expected);
        run(&mut shell, &mut host, "ptest 2>&1 >/tmp/out 3>&-");
        let mut expected = alloc::vec![
            Action::Dup2 { from: 1, to: 13 },
            open(1, "/tmp/out", OpenMode::Append),
            Action::Dup2 { from: 13, to: 2 },
        ];
        expected.extend((3..10).map(Action::Close));
        expected.push(Action::CloseFrom(13));
        assert_eq!(host.mock.spawned.remove(0).2, expected);
    }

    #[test]
    fn pipeline_closes_unused_ends() {
        let mut shell = shell_with_programs();
        let mut host = ProcessHost { mock: Mock::new() };
        host.mock.job_control = true;
        let (status, _, _) = run(&mut shell, &mut host, "ptest --emit | prog --count | ptest 2>&1");
        assert_eq!(status, 0);
        let spawned = core::mem::take(&mut host.mock.spawned);
        assert_eq!(spawned.len(), 3);
        // The first stage writes the first pipe (3, 4); the second reads it and writes the
        // second (5, 6); the last reads that. Each child has only its own ends, as 0 or
        // 1: everything from 3 up is closed in it.
        assert_eq!(spawned[0].2, [Action::Dup2 { from: 4, to: 1 }, Action::CloseFrom(3)]);
        assert_eq!(
            spawned[1].2,
            [Action::Dup2 { from: 3, to: 0 }, Action::Dup2 { from: 6, to: 1 }, Action::CloseFrom(3)]
        );
        assert_eq!(
            spawned[2].2,
            [Action::Dup2 { from: 5, to: 0 }, Action::Dup2 { from: 1, to: 2 }, Action::CloseFrom(3)]
        );
        // One group, the first stage's, which has the terminal until the shell takes it back.
        assert_eq!((spawned[0].3, spawned[0].4), (Group::New, true));
        assert_eq!((spawned[1].3, spawned[1].4), (Group::Join(100), false));
        assert_eq!((spawned[2].3, spawned[2].4), (Group::Join(100), false));
        assert_eq!(host.mock.terminal, [0]);
        // The shell closed every end it made, the write end of each pipe as soon as its
        // writer had started and the read end as soon as its reader had.
        assert!(host.mock.open.is_empty(), "left open: {:?}", host.mock.open);
        assert_eq!(host.mock.closed, [4, 3, 6, 5]);

        // `pipefail`, and a stage that fails to start.
        host.mock.ends.insert(103, WaitStatus::Exited(3));
        let (status, _, _) = run(&mut shell, &mut host, "set -o pipefail; ptest | prog | ptest");
        assert_eq!(status, 3);
        let (status, _, _) = run(&mut shell, &mut host, "set +o pipefail; ptest | /bin/missing");
        assert_eq!(status, 127);
        assert!(host.mock.open.is_empty(), "left open: {:?}", host.mock.open);

        // A background pipeline is one job, which `kill %1` signals as a group.
        host.mock.spawned.clear();
        let (status, _, _) = run(&mut shell, &mut host, "ptest | prog &");
        assert_eq!(status, 0);
        let leader = host.mock.next_pid - 2;
        assert!(host.mock.spawned.iter().all(|spawn| !spawn.4));
        run(&mut shell, &mut host, "kill %1");
        assert_eq!(host.mock.signals, [(-(leader as i32), 15)]);
        let (status, out, _) = run(&mut shell, &mut host, "echo $!");
        assert_eq!((status, out), (0, format!("{leader}\n")));
    }

    #[test]
    fn exit_status_of_signal_reaches_dollar_question() {
        let mut shell = shell_with_programs();
        let mut host = ProcessHost { mock: Mock::new() };
        host.mock.ends.insert(100, WaitStatus::Signaled(15));
        let (_, out, _) = run(&mut shell, &mut host, "ptest; echo $?");
        assert_eq!(out, "143\n");
        host.mock.ends.insert(101, WaitStatus::Exited(7));
        let (_, out, _) = run(&mut shell, &mut host, "ptest; echo $?");
        assert_eq!(out, "7\n");
        // A stop leaves a job, `fg` lets it go on and waits for it.
        host.mock.job_control = true;
        host.mock.ends.insert(102, WaitStatus::Stopped(20));
        let (status, _, err) = run(&mut shell, &mut host, "ptest");
        assert_eq!(status, 148);
        assert!(err.contains("Stopped"));
        let (status, _, _) = run(&mut shell, &mut host, "fg");
        assert_eq!(status, 0);
        assert_eq!(host.mock.signals, [(-102, 18)]);
        assert_eq!(host.mock.terminal, [0, 102, 0]);
    }

    #[test]
    fn signal_names_parse() {
        assert_eq!(signal_number("TERM"), Some(15));
        assert_eq!(signal_number("SIGKILL"), Some(9));
        assert_eq!(signal_number("int"), Some(2));
        assert_eq!(signal_number("19"), Some(19));
        assert_eq!(signal_number("BOGUS"), None);
    }
}
