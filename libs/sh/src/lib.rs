//! A Bash-compatible shell language for FEOS.
//!
//! The library owns the parser, expansion engine, builtins, interactive line
//! editor and the terminal emulator the desktop window renders. It does not know
//! about the app registry: the embedding application implements [`Host`] to
//! launch external commands.
//!
//! Supported syntax includes quoting (`'…'`, `"…"`, `$'…'`), parameter, command
//! and arithmetic substitution with the full `${}` operators, globbing, `IFS`
//! word splitting, pipelines, redirection of any descriptor including here-docs
//! and on compound commands, `&&`, `||`, `;`, `&`, functions with `local`,
//! `if`/`for`/`while`/`until`/`case`, `(( ))`, `[[ ]]`, subshells, aliases,
//! `set -e -u -x -o pipefail`, `getopts`, script execution by path, and, with a
//! [`TaskHost`], streaming pipelines, `&`, `jobs`, `fg`, `bg`, `wait` and `$!`.
//! `trap` handles EXIT, INT and ERR. Arrays are not implemented.
//!
//! Commands share one address space and one kernel stack, so every recursive
//! path is bounded and every buffer has a budget: a hostile script gets an error
//! message, not a dead kernel thread.
//!
//! # Example
//!
//! ```
//! extern crate alloc;
//!
//! use feos_coreutils::io::{BytesOutput, NullInput};
//! use feos_coreutils::{NoSystem, Vfs};
//! use feos_sh::{Host, Io, Shell};
//! use alloc::string::String;
//!
//! struct NoHost;
//! impl Host for NoHost {
//!     fn run_external(
//!         &mut self,
//!         _name: &str,
//!         _args: &[String],
//!         _stdin: &mut dyn feos_coreutils::Input,
//!         _stdout: &mut dyn feos_coreutils::Output,
//!         _stderr: &mut dyn feos_coreutils::Output,
//!         _background: bool,
//!     ) -> Option<i32> {
//!         None
//!     }
//! }
//!
//! let mut shell = Shell::new(None, &NoSystem);
//! let mut stdin = NullInput;
//! let mut stdout = BytesOutput::new();
//! let mut stderr = BytesOutput::new();
//! let mut io = Io {
//!     stdin: &mut stdin,
//!     stdout: &mut stdout,
//!     stderr: &mut stderr,
//! };
//! shell.execute_line("mkdir demo", &mut io, &mut NoHost);
//! assert!(shell.fs.is_dir("demo"));
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod arith;
mod builtins;
pub mod console;
mod editor;
mod exec;
mod expand;
mod fds;
mod jobs;
pub mod lexer;
pub mod line;
pub mod parser;
#[cfg(test)]
mod regress;
pub mod vt;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub use console::{ConsoleInput, ConsoleOutput};
use feos_coreutils::{Env, Input, Output, OverlayFs, System, Var, Vfs, COMMANDS};
use feos_fs::FileSystem;
use parser::Command;

/// Deepest nesting of functions, `eval`, `source`, subshells, command
/// substitutions and `${...}` operands.
pub const MAX_DEPTH: usize = 64;

/// How much stack the interpreter may use before it refuses to nest further.
/// Debug builds have far larger frames than release builds.
const DEFAULT_STACK_LIMIT: usize = if cfg!(debug_assertions) {
    192 * 1024
} else {
    88 * 1024
};

/// A byte stream bundle handed to a command.
pub struct Io<'a> {
    /// Standard input.
    pub stdin: &'a mut dyn Input,
    /// Standard output.
    pub stdout: &'a mut dyn Output,
    /// Standard error.
    pub stderr: &'a mut dyn Output,
}

/// External-command dispatch supplied by the embedding application.
pub trait Host {
    /// Runs a command that is not a builtin or coreutil.
    ///
    /// Returns `None` when the command does not exist. `background` is true
    /// when the user wrote `command &`.
    fn run_external(
        &mut self,
        name: &str,
        args: &[String],
        stdin: &mut dyn Input,
        stdout: &mut dyn Output,
        stderr: &mut dyn Output,
        background: bool,
    ) -> Option<i32>;

    /// Runs a user program: the file at the absolute `path`, which holds an ELF image.
    /// `args` doesn't include its name, and `cwd` is where relative paths start from.
    ///
    /// Returns `None` when this host can't run programs.
    #[allow(clippy::too_many_arguments)]
    fn run_program(
        &mut self,
        _path: &str,
        _args: &[String],
        _cwd: &str,
        _stdin: &mut dyn Input,
        _stdout: &mut dyn Output,
        _stderr: &mut dyn Output,
        _background: bool,
    ) -> Option<i32> {
        None
    }

    /// Names of external commands, used by tab completion.
    fn command_names(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// True when an external command exists, without running it.
    fn has_command(&self, name: &str) -> bool {
        self.command_names().contains(&name)
    }

    /// Asked at every checkpoint whether the running command should stop. A
    /// host returns true to deliver Ctrl-C or enforce a time limit.
    fn poll_cancel(&mut self) -> bool {
        false
    }

    /// Asked as a command runs whether the user pressed Ctrl-Z. A host that takes this
    /// to mean the command it's running has stopped reports that through
    /// [`take_suspended`](Self::take_suspended) instead.
    fn poll_suspend(&mut self) -> bool {
        false
    }

    /// The command that has just been stopped by Ctrl-Z: its process, which the host
    /// keeps the streams of until [`resume_job`](Self::resume_job) or
    /// [`continue_job`](Self::continue_job).
    fn take_suspended(&mut self) -> Option<u32> {
        None
    }

    /// Puts a stopped command back in the foreground, looking after its streams until
    /// it ends or stops again. `None` when this host isn't keeping one by that number.
    fn resume_job(
        &mut self,
        _pid: u32,
        _stdin: &mut dyn Input,
        _stdout: &mut dyn Output,
        _stderr: &mut dyn Output,
    ) -> Option<i32> {
        None
    }

    /// Lets a stopped command go on in the background, with its output on the terminal.
    fn continue_job(&mut self, _pid: u32) {}

    /// Says which signals this shell has traps for, as `1 << number`: the rest take
    /// their default action, which is to end it.
    fn catch_signals(&mut self, _mask: u32) {}

    /// Takes the signals that arrived for this shell, as `1 << number`.
    fn take_signals(&mut self) -> u32 {
        0
    }

    /// Concurrent tasks, when this host has them. Without them pipelines buffer
    /// and `&` runs in the foreground.
    fn tasks(&mut self) -> Option<&mut dyn TaskHost> {
        None
    }
}

/// What became of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    /// It is running.
    Running,
    /// Job control is holding it off the CPU.
    Stopped,
    /// It finished with this status.
    Exited(i32),
    /// There's no such task, or it finished so long ago that nobody knows.
    Unknown,
}

/// Standard input, output and error for a task.
pub type ConsoleStreams = (
    alloc::boxed::Box<dyn Input + Send>,
    alloc::boxed::Box<dyn Output + Send>,
    alloc::boxed::Box<dyn Output + Send>,
);

/// What a host offers to run shells side by side.
pub trait TaskHost {
    /// A bounded pipe, read end first. `None` when none are free. A write blocks
    /// while it is full and a read while it is empty; dropping an end closes it.
    fn pipe(
        &mut self,
    ) -> Option<(
        alloc::boxed::Box<dyn Input + Send>,
        alloc::boxed::Box<dyn Output + Send>,
    )>;

    /// New streams on the terminal this shell belongs to.
    fn console_streams(&mut self) -> ConsoleStreams;

    /// A host for a forked shell, which is stopped through `cancel`.
    fn fork_host(&mut self, cancel: CancelToken) -> alloc::boxed::Box<dyn Host + Send>;

    /// Starts `job` on a task of its own. Its value is the exit status.
    fn spawn(
        &mut self,
        name: &str,
        background: bool,
        job: alloc::boxed::Box<dyn FnOnce() -> i32 + Send>,
    ) -> Option<u32>;

    /// Looks at a task.
    fn status(&mut self, pid: u32) -> JobStatus;

    /// Asks a task to stop for good.
    fn kill(&mut self, pid: u32);

    /// Holds a task off the CPU, or lets it run again.
    fn set_stopped(&mut self, pid: u32, stopped: bool);

    /// Gives the CPU away for a moment, between looks at a task.
    fn pause(&mut self);

    /// System facts for forked shells.
    fn system(&mut self) -> &'static dyn System;
}

/// A shareable handle that stops whatever the shell is running.
///
/// The shell polls it between commands, on each loop pass and as commands read
/// and write. Another task or an interrupt handler can hold a clone.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// Asks the shell to stop the current command.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// True once cancellation was requested and not yet handled.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Forgets a cancellation that has been dealt with.
    pub fn clear(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

/// Non-local control flow travelling up through the interpreter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flow {
    None,
    Break(usize),
    Continue(usize),
    Return(i32),
    Exit(i32),
    /// An error that ends the current line of input.
    Abort(i32),
    /// Ctrl-C, a cancel request or an expired `timeout`.
    Interrupted,
}

impl Flow {
    pub(crate) fn is_active(self) -> bool {
        self != Self::None
    }
}

/// Options changed by `set`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Options {
    pub(crate) errexit: bool,
    pub(crate) nounset: bool,
    pub(crate) xtrace: bool,
    pub(crate) noglob: bool,
    pub(crate) noclobber: bool,
    pub(crate) pipefail: bool,
}

/// Saved state of a subshell's parent.
pub(crate) struct Snapshot {
    pub(crate) env: Env,
    pub(crate) cwd: String,
    pub(crate) aliases: BTreeMap<String, String>,
    pub(crate) functions: BTreeMap<String, Arc<Command>>,
    pub(crate) params: Vec<String>,
    pub(crate) arg0: String,
    pub(crate) opts: Options,
    pub(crate) locals: Vec<Vec<(String, Option<Var>)>>,
    pub(crate) loop_depth: usize,
    pub(crate) function_depth: usize,
    pub(crate) source_depth: usize,
    pub(crate) errexit_off: usize,
    pub(crate) interactive: bool,
    pub(crate) getopts_offset: usize,
    pub(crate) traps: BTreeMap<String, String>,
}

/// A shell instance with its own filesystem overlay, variables and history.
pub struct Shell<'a> {
    /// A job being waited for in the foreground stopped, by Ctrl-Z.
    pub(crate) suspended_task: bool,
    /// `fg` is what's waiting: a Ctrl-Z meanwhile stops the job.
    pub(crate) foreground_wait: bool,
    /// Writable filesystem view.
    pub fs: OverlayFs,
    /// Shell variables.
    pub env: Env,
    /// Command aliases.
    pub aliases: BTreeMap<String, String>,
    /// Interactive history, newest last.
    pub history: Vec<String>,
    /// Status of the last command (`$?`).
    pub last_status: i32,
    /// Platform facts for the system utilities.
    pub system: &'a dyn System,
    pub(crate) functions: BTreeMap<String, Arc<Command>>,
    pub(crate) params: Vec<String>,
    pub(crate) arg0: String,
    pub(crate) opts: Options,
    pub(crate) flow: Flow,
    pub(crate) exit_requested: Option<i32>,
    pub(crate) fds: fds::FdTable,
    pub(crate) keep_redirects: bool,
    pub(crate) loop_depth: usize,
    pub(crate) function_depth: usize,
    pub(crate) source_depth: usize,
    pub(crate) depth: usize,
    pub(crate) stack_base: Option<usize>,
    pub(crate) stack_limit: usize,
    pub(crate) locals: Vec<Vec<(String, Option<Var>)>>,
    pub(crate) errexit_off: usize,
    pub(crate) interactive: bool,
    pub(crate) cancel: CancelToken,
    pub(crate) deadline: Option<u64>,
    pub(crate) timed_out: bool,
    pub(crate) subst_status: Option<i32>,
    pub(crate) last_background: Option<u32>,
    pub(crate) getopts_offset: usize,
    pub(crate) getopts_index: usize,
    pub(crate) random: u64,
    pub(crate) columns: Option<usize>,
    pub(crate) jobs: Vec<jobs::Job>,
    pub(crate) next_job: usize,
    pub(crate) traps: BTreeMap<String, String>,
    pub(crate) in_trap: bool,
}

impl<'a> Shell<'a> {
    /// Creates a shell over an optional read-only base filesystem.
    pub fn new(base: Option<&'static dyn FileSystem>, system: &'a dyn System) -> Self {
        Self::with_fs(OverlayFs::new(base), system)
    }

    /// Creates a shell over an existing filesystem view.
    pub(crate) fn with_fs(fs: OverlayFs, system: &'a dyn System) -> Self {
        let mut aliases = BTreeMap::new();
        aliases.insert(String::from("ll"), String::from("ls -l"));
        aliases.insert(String::from("la"), String::from("ls -a"));
        aliases.insert(String::from("l"), String::from("ls -1"));
        aliases.insert(String::from(".."), String::from("cd .."));
        aliases.insert(String::from("..."), String::from("cd ../.."));
        aliases.insert(String::from("h"), String::from("history"));

        let mut shell = Self {
            suspended_task: false,
            foreground_wait: false,
            fs,
            env: Env::feos(),
            aliases,
            history: Vec::new(),
            last_status: 0,
            system,
            functions: BTreeMap::new(),
            params: Vec::new(),
            arg0: String::from("sh"),
            opts: Options::default(),
            flow: Flow::None,
            exit_requested: None,
            fds: fds::FdTable::new(),
            keep_redirects: false,
            loop_depth: 0,
            function_depth: 0,
            source_depth: 0,
            depth: 0,
            stack_base: None,
            stack_limit: DEFAULT_STACK_LIMIT,
            locals: Vec::new(),
            errexit_off: 0,
            interactive: true,
            cancel: CancelToken::default(),
            deadline: None,
            timed_out: false,
            subst_status: None,
            last_background: None,
            getopts_offset: 0,
            getopts_index: 1,
            random: u64::from(system.pid()).wrapping_mul(0x9e37_79b9) | 1,
            columns: None,
            jobs: Vec::new(),
            next_job: 0,
            traps: BTreeMap::new(),
            in_trap: false,
        };
        // What scripts assume exists. A failure only means the base image
        // already provides the path, or does not allow it.
        for directory in ["/tmp", "/dev"] {
            let _ = shell.fs.create_dir(directory);
        }
        let _ = shell.fs.add_null_device("/dev/null");
        let _ = shell.fs.add_zero_device("/dev/zero");
        let cwd = shell.fs.cwd().to_string();
        shell.env.set("PWD", &cwd);
        shell.env.set("OPTIND", "1");
        shell
    }

    /// A handle that stops whatever this shell is running.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Sets how much stack the interpreter may use, for hosts that know theirs.
    /// Leave room for a command's own frames, which the limit does not count.
    pub fn set_stack_limit(&mut self, bytes: usize) {
        self.stack_limit = bytes;
    }

    /// Lexes, parses and runs `line`, returning its status.
    pub fn execute_line(&mut self, line: &str, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        if line.trim().is_empty() {
            return self.last_status;
        }
        self.run_text(line, io, host)
    }

    /// Runs a script from a string, one command at a time.
    ///
    /// Multi-line `if`/`for`/`while` constructs work exactly as they do in Bash,
    /// and a syntax error stops the script without undoing what already ran.
    pub fn run_text(&mut self, text: &str, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let outermost = self.note_stack_base();
        if outermost {
            self.flow = Flow::None;
            self.cancel.clear();
            self.timed_out = false;
            self.depth = 0;
        }

        let mut status = self.run_text_inner(text, io, host);

        // Whatever was still travelling up stops here, so a stray `return` or
        // `break` cannot affect the next line.
        match core::mem::replace(&mut self.flow, Flow::None) {
            Flow::Exit(code) => {
                self.exit_requested = Some(code);
                status = code;
                self.last_status = status;
                if outermost {
                    self.run_exit_trap(io, host);
                }
            }
            Flow::Return(code) | Flow::Abort(code) => status = code,
            Flow::Interrupted => {
                self.cancel.clear();
                status = 130;
                self.last_status = status;
                self.run_interrupt_trap(io, host);
            }
            Flow::None | Flow::Break(_) | Flow::Continue(_) => {}
        }
        if outermost {
            if self.interactive {
                self.notify_jobs(io, host);
            }
            self.stack_base = None;
        }
        self.last_status = status;
        status
    }

    /// Returns the prompt after expanding the `PS1` escapes FEOS supports.
    pub fn prompt(&self) -> String {
        let pattern = self
            .env
            .get("PS1")
            .filter(|value| !value.is_empty())
            .unwrap_or("feos:\\w\\$ ");

        let mut prompt = String::new();
        let mut chars = pattern.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                prompt.push(ch);
                continue;
            }
            match chars.next() {
                // A directory's name mustn't get to drive the terminal.
                Some('w') => prompt.push_str(&feos_text::display_name(&self.display_cwd())),
                Some('W') => {
                    prompt.push_str(&feos_text::display_name(feos_coreutils::base_name(self.fs.cwd())))
                }
                Some('u') => prompt.push_str(self.env.get("USER").unwrap_or("feos")),
                Some('h') => prompt.push_str(self.env.get("HOSTNAME").unwrap_or("feos")),
                Some('$') => {
                    if self.system.uid() == 0 {
                        prompt.push('#');
                    } else {
                        prompt.push('$');
                    }
                }
                Some('n') => prompt.push('\n'),
                Some('\\') => prompt.push('\\'),
                Some(other) => {
                    prompt.push('\\');
                    prompt.push(other);
                }
                None => prompt.push('\\'),
            }
        }
        prompt
    }
    /// The continuation prompt.
    pub fn continuation_prompt(&self) -> String {
        self.env.get("PS2").unwrap_or("> ").to_string()
    }

    fn display_cwd(&self) -> String {
        let cwd = self.fs.cwd();
        if let Some(home) = self.env.get("HOME") {
            if cwd == home {
                return String::from("~");
            }
            if let Some(rest) = cwd.strip_prefix(home) {
                if rest.starts_with('/') {
                    let mut result = String::from("~");
                    result.push_str(rest);
                    return result;
                }
            }
        }
        cwd.to_string()
    }

    /// Computes a tab completion for `line` at byte offset `cursor`.
    pub fn complete(&mut self, line: &str, cursor: usize, host: &dyn Host) -> Completion {
        let mut cursor = cursor.min(line.len());
        while !line.is_char_boundary(cursor) {
            cursor -= 1;
        }
        let mut start = cursor;
        for (index, ch) in line[..cursor].char_indices().rev() {
            if is_word_separator(ch) {
                start = index + ch.len_utf8();
                break;
            }
            start = index;
        }
        let prefix = &line[start..cursor];

        let command_context = start == 0
            || line[..start]
                .chars()
                .rev()
                .find(|ch| !ch.is_whitespace())
                .map(|ch| matches!(ch, '|' | '&' | ';' | '(' | '{'))
                .unwrap_or(true);

        let (candidates, single_suffix) = if command_context {
            (self.command_candidates(prefix, host), " ")
        } else {
            (self.path_candidates(prefix), "")
        };

        let Some(first) = candidates.first() else {
            return Completion {
                line: line.to_string(),
                cursor,
                candidates: Vec::new(),
            };
        };

        if candidates.len() == 1 {
            let mut replacement = first.clone();
            if single_suffix == " " {
                replacement.push(' ');
            }
            let mut result = String::from(&line[..start]);
            result.push_str(&replacement);
            result.push_str(&line[cursor..]);
            return Completion {
                cursor: start + replacement.len(),
                line: result,
                candidates: Vec::new(),
            };
        }

        let common = common_prefix(&candidates);
        let mut result = String::from(&line[..start]);
        result.push_str(&common);
        result.push_str(&line[cursor..]);
        Completion {
            cursor: start + common.len(),
            line: result,
            candidates,
        }
    }

    fn command_candidates(&self, prefix: &str, host: &dyn Host) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for spec in COMMANDS {
            names.push(spec.name.to_string());
            for alias in spec.aliases {
                names.push((*alias).to_string());
            }
        }
        for name in builtin_names() {
            names.push((*name).to_string());
        }
        for name in host.command_names() {
            names.push(name.to_string());
        }
        for name in self.aliases.keys() {
            names.push(name.clone());
        }
        for name in self.functions.keys() {
            names.push(name.clone());
        }
        finish_candidates(names, prefix)
    }

    fn path_candidates(&mut self, prefix: &str) -> Vec<String> {
        let expanded = if let Some(rest) = prefix.strip_prefix('~') {
            let home = self.env.get("HOME").unwrap_or("/").to_string();
            let mut text = home;
            text.push_str(rest);
            text
        } else {
            prefix.to_string()
        };

        let (directory, name_prefix) = match expanded.rfind('/') {
            Some(index) => (&expanded[..=index], &expanded[index + 1..]),
            None => ("", expanded.as_str()),
        };
        let lookup = if directory.is_empty() { "." } else { directory };
        let Ok(entries) = self.fs.read_dir(lookup) else {
            return Vec::new();
        };

        let mut candidates = Vec::new();
        for entry in entries {
            if !entry.name.starts_with(name_prefix) {
                continue;
            }
            if name_prefix.is_empty() && entry.name.starts_with('.') {
                continue;
            }
            let mut candidate = String::from(directory);
            candidate.push_str(&entry.name);
            if entry.kind == feos_coreutils::Kind::Dir {
                candidate.push('/');
            }
            candidates.push(candidate);
        }
        candidates.sort();
        candidates
    }

    /// Takes a pending `exit` status, if the shell was asked to leave.
    pub fn take_exit_status(&mut self) -> Option<i32> {
        self.exit_requested.take()
    }
}

/// Result of a tab-completion request.
pub struct Completion {
    /// The edited line.
    pub line: String,
    /// New cursor offset.
    pub cursor: usize,
    /// Candidates to display when there was no unique completion.
    pub candidates: Vec<String>,
}

fn finish_candidates(mut names: Vec<String>, prefix: &str) -> Vec<String> {
    names.sort();
    names.dedup();
    names.retain(|name| name.starts_with(prefix));
    names
}

fn common_prefix(values: &[String]) -> String {
    let Some(first) = values.first() else {
        return String::new();
    };
    let mut length = first.len();
    for value in values.iter().skip(1) {
        length = length.min(value.len());
        while !first.is_char_boundary(length) || !value.is_char_boundary(length) {
            length -= 1;
        }
        while length > 0 && first[..length] != value[..length] {
            length -= 1;
            while !first.is_char_boundary(length) || !value.is_char_boundary(length) {
                length -= 1;
            }
        }
    }
    first[..length].to_string()
}

fn is_word_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}')
}

/// Names of the shell's built-in commands.
pub fn builtin_names() -> &'static [&'static str] {
    &[
        ".", ":", "alias", "bash", "bg", "break", "builtin", "cd", "clear", "command", "continue",
        "declare", "eval", "exec", "exit", "export", "false", "fg", "getopts", "help", "history",
        "jobs", "kill", "let", "local", "pwd", "read", "readonly", "return", "set", "sh", "shift",
        "source", "timeout", "trap", "true", "type", "typeset", "unalias", "unset", "wait", "which",
    ]
}

/// Runs the interactive editor until the user presses Ctrl-D or types `exit`.
pub fn interactive(shell: &mut Shell<'_>, host: &mut dyn Host, io: &mut Io<'_>) -> i32 {
    loop {
        match editor::read_line(shell, host, io) {
            editor::ReadResult::Line(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                remember(shell, &line);
                shell.execute_line(&line, io, host);
                if let Some(status) = shell.exit_requested.take() {
                    return status;
                }
            }
            editor::ReadResult::Eof => {
                // A trap at the prompt may have asked to leave.
                if let Some(status) = shell.exit_requested.take() {
                    return status;
                }
                io.stdout.write(b"\n");
                return 0;
            }
            editor::ReadResult::Interrupted => shell.last_status = 130,
        }
    }
}

/// Adds a line to the history unless it repeats the last one.
pub fn remember(shell: &mut Shell<'_>, line: &str) {
    if line.trim().is_empty() || shell.history.last().map(String::as_str) == Some(line) {
        return;
    }
    shell.history.push(line.to_string());
    if shell.history.len() > 200 {
        shell.history.remove(0);
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use feos_coreutils::{BytesInput, BytesOutput};

    pub struct NoHost;

    impl Host for NoHost {
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
    }

    pub fn run(shell: &mut Shell<'_>, host: &mut NoHost, line: &str) -> (i32, String, String) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_coreutils::{BytesInput, BytesOutput, NoSystem};

    struct NoHost;

    impl Host for NoHost {
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
    }

    #[test]
    fn the_line_editor_consumes_console_bytes() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut host = NoHost;
        let mut stdin = BytesInput::new(b"echo hi\r".to_vec());
        let mut stdout = BytesOutput::new();
        let mut stderr = BytesOutput::new();
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        match editor::read_line(&mut shell, &mut host, &mut io) {
            editor::ReadResult::Line(line) => assert_eq!(line, "echo hi"),
            _ => panic!("expected a line"),
        }
    }

    fn run(shell: &mut Shell<'_>, line: &str) -> (i32, String, String) {
        let mut stdin = BytesInput::empty();
        let mut stdout = BytesOutput::new();
        let mut stderr = BytesOutput::new();
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let status = shell.execute_line(line, &mut io, &mut NoHost);
        let out = String::from_utf8_lossy(stdout.bytes()).into_owned();
        let err = String::from_utf8_lossy(stderr.bytes()).into_owned();
        (status, out, err)
    }

    #[test]
    fn runs_simple_commands_and_builtins() {
        let mut shell = Shell::new(None, &NoSystem);
        let (status, out, _) = run(&mut shell, "echo hello");
        assert_eq!(status, 0);
        assert_eq!(out, "hello\n");

        let (status, out, _) = run(&mut shell, "mkdir demo && cd demo && pwd");
        assert_eq!(status, 0);
        assert_eq!(out, "/demo\n");
    }

    #[test]
    fn expands_variables_and_globs() {
        let mut shell = Shell::new(None, &NoSystem);
        run(&mut shell, "mkdir docs");
        run(&mut shell, "touch docs/a.txt docs/b.md");
        let (_, out, _) = run(&mut shell, "for f in docs/*.txt; do echo $f; done");
        assert_eq!(out, "docs/a.txt\n");
    }

    #[test]
    fn pipes_and_redirections_work() {
        let mut shell = Shell::new(None, &NoSystem);
        let (status, out, _) = run(&mut shell, "printf 'b\\na\\n' | sort -r");
        assert_eq!(status, 0);
        assert_eq!(out, "b\na\n");

        let (status, _, _) = run(&mut shell, "echo one > file; echo two >> file");
        assert_eq!(status, 0);
        let (_, out, _) = run(&mut shell, "cat file");
        assert_eq!(out, "one\ntwo\n");
    }

    #[test]
    fn conditionals_and_loops_work() {
        let mut shell = Shell::new(None, &NoSystem);
        let (_, out, _) = run(&mut shell, "if true; then echo yes; else echo no; fi");
        assert_eq!(out, "yes\n");

        let (_, out, _) = run(
            &mut shell,
            "i=0; while test $i -lt 3; do echo $i; i=$((i + 1)); done",
        );
        assert_eq!(out, "0\n1\n2\n");
    }

    #[test]
    fn command_substitution_and_arithmetic_work() {
        let mut shell = Shell::new(None, &NoSystem);
        let (_, out, _) = run(&mut shell, "echo $(echo inner) $((2 + 3))");
        assert_eq!(out, "inner 5\n");

        let (_, out, _) = run(&mut shell, "name=world; echo ${name:-nobody}-$name");
        assert_eq!(out, "world-world\n");

        let (_, out, _) = run(&mut shell, "false; echo $?");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn redirections_and_stderr_work() {
        let mut shell = Shell::new(None, &NoSystem);
        run(&mut shell, "cat missing 2> err.txt");
        let (_, out, _) = run(&mut shell, "cat err.txt");
        assert!(out.contains("missing"));

        run(&mut shell, "echo data > out.txt");
        run(&mut shell, "echo more >> out.txt");
        let (_, out, _) = run(&mut shell, "cat out.txt");
        assert_eq!(out, "data\nmore\n");
    }

    #[test]
    fn else_if_and_nested_loops_work() {
        let mut shell = Shell::new(None, &NoSystem);
        let (_, out, _) = run(
            &mut shell,
            "if false; then echo one; elif true; then echo two; else echo three; fi",
        );
        assert_eq!(out, "two\n");

        let (_, out, _) = run(
            &mut shell,
            "for x in a b c; do for y in 1 2; do echo $x$y; done; done",
        );
        assert_eq!(out, "a1\na2\nb1\nb2\nc1\nc2\n");
    }

    #[test]
    fn aliases_and_help_are_available() {
        let mut shell = Shell::new(None, &NoSystem);
        run(&mut shell, "alias hi='echo hello'");
        let (_, out, _) = run(&mut shell, "hi world");
        assert_eq!(out, "hello world\n");

        let (status, out, _) = run(&mut shell, "help grep");
        assert_eq!(status, 0);
        assert!(out.contains("grep"));
    }

    #[test]
    fn completion_handles_commands_paths_and_odd_cursors() {
        let mut shell = Shell::new(None, &NoSystem);
        run(&mut shell, "mkdir project; touch project/file.txt");
        let done = shell.complete("cd pro", 6, &NoHost);
        assert_eq!(done.line, "cd project/");
        let done = shell.complete("whoam", 5, &NoHost);
        assert_eq!(done.line, "whoami ");
        // A cursor inside a multi-byte character must not panic.
        let done = shell.complete("é", 1, &NoHost);
        assert_eq!(done.line, "é");
    }
}
