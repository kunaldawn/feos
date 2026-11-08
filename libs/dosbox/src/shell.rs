//! A `COMMAND.COM`-style prompt.
//!
//! The prompt edits its line on the emulated text screen and implements the
//! usual built-ins. Batch files run one line per frame, with `%0`-`%9`, `%VAR%`,
//! `IF`, `GOTO`, `FOR`, `CALL`, `SHIFT`, `SET` and `PAUSE`. Command lines can
//! redirect with `<`, `>` and `>>`: built-ins through a capture buffer, programs
//! through the job file table they inherit. Pipes aren't implemented.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::bios::{clock_date, clock_time};
use crate::cpu::InterruptAction;
use crate::dos::Dos;
use crate::keyboard::KeyEvent;
use crate::memory::Memory;
use crate::vga::Vga;
use crate::{PRODUCT, VERSION};

/// How deep batch files can `CALL` each other.
const MAX_BATCH_DEPTH: usize = 16;
/// The most a batch file can queue up through `FOR`.
const MAX_PENDING: usize = 256;
/// How far `IF` and `FOR` can nest on one line.
const MAX_LINE_DEPTH: u32 = 8;
/// The largest batch file read.
const MAX_BATCH_SIZE: usize = 256 * 1024;

/// What the prompt wants the machine to do after a command.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommandResult {
    /// The command finished.
    None,
    /// Load and run a program, with the command tail to place in the PSP.
    Run(usize, String),
    /// Save a screenshot of the current screen to `C:\SCREEN.BMP`.
    Screenshot,
    /// Save the machine state to `C:\STATE.BIN`.
    SaveState,
    /// Restore the machine state from `C:\STATE.BIN`.
    LoadState,
    /// Change the CPU's speed, in instruction units per millisecond.
    SetCycles(u32),
    /// Run later programs under the memory manager, or not.
    SetMemoryManager(bool),
    /// When to offer the DPMI host.
    SetDpmi(crate::dpmi::Mode),
    /// Change which Sound Blaster the machine has.
    SetSoundBlaster(crate::sb::Model),
    /// Close the DOS window.
    Exit,
}

/// One running batch file.
struct BatchFile {
    lines: Vec<String>,
    next: usize,
    args: Vec<String>,
    shift: usize,
}

/// Where a built-in's output goes.
struct Out<'a> {
    vga: &'a mut Vga,
    mem: &'a mut Memory,
    capture: Option<Vec<u8>>,
}

impl Out<'_> {
    fn bytes(&mut self, text: &[u8]) {
        match self.capture.as_mut() {
            Some(buffer) => buffer.extend_from_slice(text),
            None => self.vga.write_str(self.mem, text),
        }
    }

    fn line(&mut self, text: &str) {
        self.bytes(text.as_bytes());
        self.bytes(b"\r\n");
    }
}

/// A handle redirected for a running program, to restore when it ends.
struct SavedHandle {
    handle: u16,
    saved: u8,
}

/// The prompt state.
pub struct Shell {
    pub line: Vec<u8>,
    pub prompt: String,
    pub exit: bool,
    pub echo_off: bool,
    batch: Vec<BatchFile>,
    /// Commands queued by `FOR`, which run before the next batch line.
    pending: VecDeque<String>,
    pub history: Vec<String>,
    pub history_index: usize,
    pub last_command: String,
    pub running: bool,
    /// `ERRORLEVEL`: the last program's exit code.
    pub errorlevel: u8,
    /// The environment, mirrored into the root environment block.
    pub env: Vec<(String, String)>,
    /// `PAUSE` waits for a key.
    waiting_for_key: bool,
    /// The running line came from a batch file.
    from_batch: bool,
    saved_handles: Vec<SavedHandle>,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// Copies data without aborting if the heap is short.
fn try_copy(data: &[u8]) -> Option<Vec<u8>> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(data.len()).ok()?;
    copy.extend_from_slice(data);
    Some(copy)
}

impl Shell {
    pub fn new() -> Self {
        let env = [
            ("COMSPEC", "C:\\COMMAND.COM"),
            ("PATH", "C:\\"),
            ("PROMPT", "$P$G"),
            ("BLASTER", "A220 I7 D1 H5 T6"),
        ]
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        Self {
            line: Vec::new(),
            prompt: "$P$G".to_string(),
            exit: false,
            echo_off: false,
            batch: Vec::new(),
            pending: VecDeque::new(),
            history: Vec::new(),
            history_index: 0,
            last_command: String::new(),
            running: false,
            errorlevel: 0,
            env,
            waiting_for_key: false,
            from_batch: false,
            saved_handles: Vec::new(),
        }
    }

    /// Whether a batch file or queued command is still in progress.
    pub fn in_batch(&self) -> bool {
        !self.batch.is_empty() || !self.pending.is_empty()
    }

    /// The next line to run from the queue or a batch file, with its parameters
    /// expanded.
    pub fn next_batch(&mut self) -> Option<String> {
        if self.waiting_for_key {
            return None;
        }
        if let Some(line) = self.pending.pop_front() {
            self.from_batch = true;
            return Some(line);
        }
        while let Some(file) = self.batch.last_mut() {
            let Some(raw) = file.lines.get(file.next) else {
                self.batch.pop();
                continue;
            };
            let raw = raw.clone();
            file.next += 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with(':') {
                continue;
            }
            self.from_batch = true;
            return Some(self.expand(&raw));
        }
        self.from_batch = false;
        None
    }

    /// Replaces `%0`-`%9`, `%VAR%` and `%%` in a batch line.
    fn expand(&self, line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '%' {
                out.push(ch);
                continue;
            }
            match chars.peek().copied() {
                Some('%') => {
                    chars.next();
                    out.push('%');
                }
                Some(digit) if digit.is_ascii_digit() => {
                    chars.next();
                    if let Some(file) = self.batch.last() {
                        let index = digit as usize - '0' as usize;
                        let at = if index == 0 { 0 } else { index + file.shift };
                        if let Some(arg) = file.args.get(at) {
                            out.push_str(arg);
                        }
                    }
                }
                Some(_) => {
                    let mut name = String::new();
                    let mut closed = false;
                    for next in chars.by_ref() {
                        if next == '%' {
                            closed = true;
                            break;
                        }
                        name.push(next);
                    }
                    if closed {
                        if let Some(value) = self.get_env(&name) {
                            out.push_str(value);
                        }
                    }
                }
                None => {}
            }
        }
        out
    }

    fn get_env(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Sets a variable in the prompt's environment, for host tools.
    pub fn set_env_public(&mut self, name: &str, value: &str, dos: &mut Dos, mem: &mut Memory) {
        self.set_env(name, value, dos, mem);
    }

    fn set_env(&mut self, name: &str, value: &str, dos: &mut Dos, mem: &mut Memory) {
        let name = name.to_ascii_uppercase();
        self.env.retain(|(key, _)| *key != name);
        if !value.is_empty() {
            self.env.push((name, value.to_string()));
        }
        dos.write_root_env(mem, &self.env);
    }

    /// Starts a `.BAT` file, nested inside whichever batch file is running.
    pub fn load_batch(&mut self, dos: &mut Dos, path: &str, args: &[String]) -> bool {
        let Some(node) = dos.resolve(path) else {
            return false;
        };
        if !dos.disk.is_file(node) || self.batch.len() >= MAX_BATCH_DEPTH {
            return false;
        }
        let data = dos
            .disk
            .node(node)
            .map_or(&[][..], |node| node.data.as_slice());
        let data = &data[..data.len().min(MAX_BATCH_SIZE)];
        let text = String::from_utf8_lossy(data);
        let lines: Vec<String> = text
            .lines()
            .map(|line| line.trim_end_matches('\r').to_string())
            .collect();
        let mut all_args = alloc::vec![path.to_string()];
        all_args.extend(args.iter().cloned());
        self.batch.push(BatchFile {
            lines,
            next: 0,
            args: all_args,
            shift: 0,
        });
        true
    }

    /// Expands the DOS `$` prompt codes.
    pub fn expand_prompt(&self, dos: &Dos, now_ms: u64) -> String {
        let path = dos.current_path();
        let (century, year, month, day) = clock_date(now_ms);
        let (hour, minute, _) = clock_time(now_ms);
        let mut out = String::new();
        let mut chars = self.prompt.chars();
        while let Some(ch) = chars.next() {
            if ch != '$' {
                out.push(ch);
                continue;
            }
            match chars.next().unwrap_or('$').to_ascii_uppercase() {
                'P' => {
                    out.push_str("C:");
                    out.push_str(&path);
                }
                'G' => out.push('>'),
                'L' => out.push('<'),
                'B' => out.push('|'),
                'N' => out.push('C'),
                'D' => out.push_str(&format!(
                    "{:02}-{:02}-{:02}",
                    (century as u16) * 100 + year as u16,
                    month,
                    day
                )),
                'T' => out.push_str(&format!("{:02}:{:02}", hour, minute)),
                '_' => {
                    out.push('\r');
                    out.push('\n');
                }
                '$' => out.push('$'),
                other => {
                    out.push('$');
                    out.push(other);
                }
            }
        }
        out
    }

    pub fn show_prompt(&mut self, dos: &Dos, vga: &mut Vga, mem: &mut Memory, now_ms: u64) {
        let prompt = self.expand_prompt(dos, now_ms);
        vga.write_str(mem, prompt.as_bytes());
    }

    /// Called after a command that didn't start a program.
    pub fn after_command(&mut self, dos: &mut Dos, vga: &mut Vga, mem: &mut Memory, now_ms: u64) {
        if !self.has_more_lines() {
            self.batch.clear();
            self.from_batch = false;
            if !self.waiting_for_key {
                self.show_prompt(dos, vga, mem, now_ms);
            }
        }
    }

    fn has_more_lines(&self) -> bool {
        if !self.pending.is_empty() {
            return true;
        }
        self.batch.iter().any(|file| {
            file.lines[file.next.min(file.lines.len())..]
                .iter()
                .any(|line| {
                    let line = line.trim();
                    !line.is_empty() && !line.starts_with(':')
                })
        })
    }

    /// Called when a program ends, or couldn't start.
    pub fn program_finished(
        &mut self,
        code: u8,
        dos: &mut Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        now_ms: u64,
    ) {
        self.errorlevel = code;
        for saved in self.saved_handles.drain(..) {
            dos.restore_handle(mem, saved.handle, saved.saved);
        }
        self.after_command(dos, vga, mem, now_ms);
    }

    /// Handles one key. Returns a completed command line when Enter is pressed.
    pub fn feed_key(
        &mut self,
        key: KeyEvent,
        dos: &Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        now_ms: u64,
    ) -> Option<String> {
        if self.waiting_for_key {
            self.waiting_for_key = false;
            if !self.in_batch() {
                self.show_prompt(dos, vga, mem, now_ms);
            }
            return None;
        }
        match key.ascii {
            0x0D => {
                vga.teletype(mem, b'\r');
                vga.teletype(mem, b'\n');
                let line = String::from_utf8_lossy(&self.line).into_owned();
                self.line.clear();
                if !line.trim().is_empty() {
                    self.history.push(line.clone());
                    if self.history.len() > 64 {
                        self.history.remove(0);
                    }
                    self.history_index = self.history.len();
                }
                self.from_batch = false;
                Some(line)
            }
            0x08 => {
                if self.line.pop().is_some() {
                    vga.teletype(mem, 0x08);
                    vga.teletype(mem, b' ');
                    vga.teletype(mem, 0x08);
                }
                None
            }
            0x1B => {
                self.line.clear();
                vga.teletype(mem, b'\r');
                vga.teletype(mem, b'\n');
                self.show_prompt(dos, vga, mem, now_ms);
                None
            }
            0x03 => {
                self.line.clear();
                vga.write_str(mem, b"^C\r\n");
                self.batch.clear();
                self.pending.clear();
                self.show_prompt(dos, vga, mem, now_ms);
                None
            }
            0x0C => {
                vga.clear(mem, 0x07);
                self.show_prompt(dos, vga, mem, now_ms);
                None
            }
            0x09 => {
                let next = (self.line.len() + 8) & !7;
                while self.line.len() < next && self.line.len() < 127 {
                    self.line.push(b' ');
                    vga.teletype(mem, b' ');
                }
                None
            }
            0x00 => {
                // Extended key: use the scan code for history navigation.
                match key.scan {
                    0x48 => self.history_previous(dos, vga, mem, now_ms),
                    0x50 => self.history_next(dos, vga, mem, now_ms),
                    _ => None,
                }
            }
            ch if (0x20..=0x7E).contains(&ch) => {
                if self.line.len() < 127 {
                    self.line.push(ch);
                    vga.teletype(mem, ch);
                }
                None
            }
            _ => None,
        }
    }

    fn history_previous(
        &mut self,
        dos: &Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        now_ms: u64,
    ) -> Option<String> {
        if self.history.is_empty() {
            return None;
        }
        self.history_index = self.history_index.saturating_sub(1);
        self.replace_line(dos, vga, mem, now_ms);
        None
    }

    fn history_next(
        &mut self,
        dos: &Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        now_ms: u64,
    ) -> Option<String> {
        if self.history_index + 1 >= self.history.len() {
            self.history_index = self.history.len();
            self.line.clear();
        } else {
            self.history_index += 1;
        }
        self.replace_line(dos, vga, mem, now_ms);
        None
    }

    fn replace_line(&mut self, dos: &Dos, vga: &mut Vga, mem: &mut Memory, now_ms: u64) {
        let old = self.line.len();
        vga.teletype(mem, b'\r');
        let prompt = self.expand_prompt(dos, now_ms);
        vga.write_str(mem, prompt.as_bytes());
        for _ in 0..old {
            vga.teletype(mem, b' ');
        }
        vga.teletype(mem, b'\r');
        vga.write_str(mem, prompt.as_bytes());
        if let Some(entry) = self.history.get(self.history_index) {
            self.line = entry.as_bytes().to_vec();
        }
        let line = self.line.clone();
        vga.write_str(mem, &line);
    }

    /// Executes one command line.
    pub fn execute(
        &mut self,
        line: &str,
        dos: &mut Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        now_ms: u64,
    ) -> CommandResult {
        let mut line = line.trim();
        let silent = line.starts_with('@');
        if let Some(rest) = line.strip_prefix('@') {
            line = rest.trim();
        }
        if self.from_batch && !self.echo_off && !silent && !line.is_empty() {
            self.show_prompt(dos, vga, mem, now_ms);
            vga.write_line(mem, line.as_bytes());
        }
        self.execute_inner(line, dos, vga, mem, now_ms, 0)
    }

    fn execute_inner(
        &mut self,
        line: &str,
        dos: &mut Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        now_ms: u64,
        depth: u32,
    ) -> CommandResult {
        let line = line.trim();
        if line.is_empty() || line.starts_with(':') || depth > MAX_LINE_DEPTH {
            return CommandResult::None;
        }
        let (command, rest) = split_command(line);
        let upper = command.to_ascii_uppercase();
        // These hand over a whole command, so its redirections are its own.
        match upper.as_str() {
            "IF" => return self.command_if(rest, dos, vga, mem, now_ms, depth),
            "FOR" => {
                self.command_for(rest, dos, vga, mem);
                return CommandResult::None;
            }
            "REM" => return CommandResult::None,
            _ => {}
        }

        let (body, redirect) = parse_redirects(line);
        let (command, rest) = split_command(&body);
        let upper = command.to_ascii_uppercase();
        if !is_builtin(&upper) {
            match self.find_program(command, dos) {
                Some(Target::Program(node)) => {
                    if !self.apply_redirects(&redirect, dos, mem) {
                        vga.write_line(mem, b"File not found");
                        return CommandResult::None;
                    }
                    return CommandResult::Run(node, rest.to_string());
                }
                Some(Target::Batch(path)) => {
                    let args = split_args(rest);
                    // Without CALL, a batch file replaces the one running.
                    self.batch.pop();
                    if !self.load_batch(dos, &path, &args) {
                        vga.write_line(mem, b"Batch file missing");
                    }
                    return CommandResult::None;
                }
                None => {
                    vga.write_line(mem, b"Bad command or file name");
                    return CommandResult::None;
                }
            }
        }
        let capture = redirect.output.is_some();
        let mut out = Out {
            vga,
            mem,
            capture: capture.then(Vec::new),
        };
        let result = self.builtin(&upper, rest, dos, &mut out, now_ms, depth);
        let Out { capture, .. } = out;
        if let (Some(buffer), Some((path, append))) = (capture, &redirect.output) {
            if !write_capture(dos, path, *append, &buffer) {
                vga.write_line(mem, b"Unable to write the redirected file");
            }
        }
        result
    }

    /// Searches the current directory, then `PATH`, as `COMMAND.COM` does.
    fn find_program(&self, target: &str, dos: &Dos) -> Option<Target> {
        if target.is_empty() {
            return None;
        }
        let upper = target.to_ascii_uppercase();
        let names: Vec<String> =
            if upper.ends_with(".COM") || upper.ends_with(".EXE") || upper.ends_with(".BAT") {
                alloc::vec![target.to_string()]
            } else {
                alloc::vec![
                    format!("{target}.COM"),
                    format!("{target}.EXE"),
                    format!("{target}.BAT"),
                ]
            };
        let mut dirs = alloc::vec![String::new()];
        if !target.contains(['\\', '/', ':']) {
            if let Some(path) = self.get_env("PATH") {
                for dir in path.split(';') {
                    if !dir.is_empty() {
                        let mut dir = dir.to_string();
                        if !dir.ends_with('\\') {
                            dir.push('\\');
                        }
                        dirs.push(dir);
                    }
                }
            }
        }
        for dir in &dirs {
            for name in &names {
                let path = format!("{dir}{name}");
                let Some(node) = dos.resolve(&path) else {
                    continue;
                };
                if !dos.disk.is_file(node) {
                    continue;
                }
                let real = dos
                    .disk
                    .node(node)
                    .map(|node| node.short.clone())
                    .unwrap_or_default();
                if real.ends_with(".BAT") {
                    return Some(Target::Batch(path));
                }
                return Some(Target::Program(node));
            }
        }
        None
    }

    /// Applies redirections to the root's handles, for a program to inherit.
    fn apply_redirects(&mut self, redirect: &Redirects, dos: &mut Dos, mem: &mut Memory) -> bool {
        if let Some(path) = &redirect.input {
            if let Some(saved) = dos.save_handle(mem, 0) {
                self.saved_handles.push(SavedHandle { handle: 0, saved });
            }
            if !dos.redirect(mem, 0, path, false, false) {
                return false;
            }
        }
        if let Some((path, append)) = &redirect.output {
            if let Some(saved) = dos.save_handle(mem, 1) {
                self.saved_handles.push(SavedHandle { handle: 1, saved });
            }
            if !dos.redirect(mem, 1, path, true, *append) {
                return false;
            }
        }
        true
    }

    /// `IF [NOT] ERRORLEVEL n`, `IF [NOT] EXIST file`, `IF [NOT] a==b`.
    fn command_if(
        &mut self,
        rest: &str,
        dos: &mut Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        now_ms: u64,
        depth: u32,
    ) -> CommandResult {
        let mut rest = rest.trim();
        let mut negate = false;
        if let Some(after) = strip_word(rest, "NOT") {
            negate = true;
            rest = after;
        }
        let (condition, command) = if let Some(after) = strip_word(rest, "ERRORLEVEL") {
            let (value, command) = split_command(after);
            let value: u16 = value.parse().unwrap_or(256);
            (self.errorlevel as u16 >= value, command)
        } else if let Some(after) = strip_word(rest, "EXIST") {
            let (path, command) = split_command(after);
            (
                dos.resolve(path).is_some() || !find_matches(dos, path).is_empty(),
                command,
            )
        } else if let Some((left, after)) = rest.split_once("==") {
            let after = after.trim_start();
            let (right, command) = split_command(after);
            (left.trim() == right, command)
        } else {
            vga.write_line(mem, b"Syntax error");
            return CommandResult::None;
        };
        if condition != negate {
            return self.execute_inner(command, dos, vga, mem, now_ms, depth + 1);
        }
        CommandResult::None
    }

    /// `FOR %v IN (set) DO command`. Each item queues one command.
    fn command_for(&mut self, rest: &str, dos: &Dos, vga: &mut Vga, mem: &mut Memory) {
        let rest = rest.trim();
        let (variable, after) = split_command(rest);
        let open = after.find('(');
        let close = after.find(')');
        if strip_word(after, "IN").is_none() {
            vga.write_line(mem, b"Syntax error");
            return;
        }
        let (Some(open), Some(close)) = (open, close) else {
            vga.write_line(mem, b"Syntax error");
            return;
        };
        if close < open {
            vga.write_line(mem, b"Syntax error");
            return;
        }
        let Some(command) = strip_word(after[close + 1..].trim_start(), "DO") else {
            vga.write_line(mem, b"Syntax error");
            return;
        };
        let mut items: Vec<String> = Vec::new();
        for item in after[open + 1..close].split_whitespace() {
            if item.contains(['*', '?']) {
                items.extend(find_matches(dos, item));
            } else {
                items.push(item.to_string());
            }
        }
        // Queued ahead of anything else, in the set's order.
        for item in items.iter().rev() {
            if self.pending.len() < MAX_PENDING {
                self.pending.push_front(command.replace(variable, item));
            }
        }
    }

    fn builtin(
        &mut self,
        upper: &str,
        rest: &str,
        dos: &mut Dos,
        out: &mut Out<'_>,
        now_ms: u64,
        depth: u32,
    ) -> CommandResult {
        let args: Vec<&str> = rest.split_whitespace().collect();
        match upper {
            "CLS" => out.vga.clear(out.mem, 0x07),
            "ECHO" | "ECHO." => {
                let text = rest.trim_start();
                if upper == "ECHO." {
                    out.line(text);
                } else if text.eq_ignore_ascii_case("OFF") {
                    self.echo_off = true;
                } else if text.eq_ignore_ascii_case("ON") {
                    self.echo_off = false;
                } else if text.is_empty() {
                    out.line(if self.echo_off {
                        "ECHO is off"
                    } else {
                        "ECHO is on"
                    });
                } else {
                    out.line(text);
                }
            }
            "PROMPT" => {
                let text = rest.trim_start();
                self.prompt = if text.is_empty() {
                    "$P$G".to_string()
                } else {
                    text.to_string()
                };
                let value = self.prompt.clone();
                self.set_env("PROMPT", &value, dos, out.mem);
            }
            "VER" => {
                out.line(&format!(
                    "{} Version {} (DOS {}.0 compatible)",
                    PRODUCT,
                    VERSION,
                    crate::DOS_VERSION
                ));
            }
            "VOL" => out.line(" Volume in drive C is FEOS-DOS"),
            "DATE" => {
                let (century, year, month, day) = clock_date(now_ms);
                let date = format!(
                    "{:02}-{:02}-{:02}",
                    (century as u16) * 100 + year as u16,
                    month,
                    day
                );
                out.line(&format!("Current date is {date}"));
            }
            "TIME" => {
                let (hour, minute, second) = clock_time(now_ms);
                out.line(&format!(
                    "Current time is {:02}:{:02}:{:02}",
                    hour, minute, second
                ));
            }
            "CD" | "CHDIR" => match args.first() {
                None => out.line(&format!("C:{}", dos.current_path())),
                Some(target) => match dos.resolve(target) {
                    Some(node) if dos.disk.is_dir(node) => dos.cwd = node,
                    _ => out.line("Invalid directory"),
                },
            },
            "MD" | "MKDIR" => {
                if let Some(target) = args.first() {
                    let (parent, name) = split_parent(dos, target);
                    match parent {
                        Some(parent) if dos.disk.create_dir(parent, &name).is_some() => {}
                        _ => out.line("Unable to create directory"),
                    }
                }
            }
            "RD" | "RMDIR" => {
                if let Some(target) = args.first() {
                    match dos.resolve(target) {
                        Some(node)
                            if dos.disk.is_dir(node)
                                && !dos.disk.is_within(dos.cwd, node)
                                && dos.disk.remove(node) => {}
                        _ => out.line("Invalid directory or directory not empty"),
                    }
                }
            }
            "DIR" => dir(dos, out, &args),
            "TYPE" => {
                let Some(target) = args.first() else {
                    out.line("Syntax: TYPE file");
                    return CommandResult::None;
                };
                match dos.resolve(target) {
                    Some(node) if dos.disk.is_file(node) => {
                        let data = dos
                            .disk
                            .node(node)
                            .map_or(&[][..], |node| node.data.as_slice());
                        out.bytes(data);
                        if !data.ends_with(b"\n") {
                            out.bytes(b"\r\n");
                        }
                    }
                    _ => out.line("File not found"),
                }
            }
            "COPY" => copy(dos, out, &args),
            "DEL" | "ERASE" => delete(dos, out, &args),
            "REN" | "RENAME" => rename(dos, out, &args),
            "MEM" => mem_command(dos, out),
            "MOUNT" => {
                let Some(name) = args.first() else {
                    out.line("Syntax: MOUNT <directory or game>");
                    return CommandResult::None;
                };
                match dos.mount_any(name, name) {
                    Ok(count) => out.line(&format!("Mounted C:\\{name} ({count} entries)")),
                    Err(error) => out.line(&format!("MOUNT: {error}")),
                }
            }
            "UNMOUNT" => {
                let Some(name) = args.first() else {
                    out.line("Syntax: UNMOUNT <directory>");
                    return CommandResult::None;
                };
                match dos.unmount(name) {
                    Ok(()) => out.line(&format!("Removed C:\\{name}")),
                    Err(error) => out.line(&format!("UNMOUNT: {error}")),
                }
            }
            "GAMES" => {
                if let Some(data) = dos.read_fs_file("GAMES.TXT") {
                    out.bytes(&data);
                } else {
                    let entries = dos.fs_entries();
                    if entries.is_empty() {
                        out.line("No FEOS filesystem is mounted on this board.");
                    } else {
                        out.line("Directories on the FEOS filesystem:");
                        for (name, is_dir) in entries {
                            out.line(&format!(
                                "  {:<12} {}",
                                name,
                                if is_dir { "<DIR>" } else { "" }
                            ));
                        }
                    }
                }
            }
            "BEEP" => out.vga.teletype(out.mem, 0x07),
            "SCREEN" | "SCREENSHOT" => return CommandResult::Screenshot,
            "SAVESTATE" => return CommandResult::SaveState,
            "LOADSTATE" => return CommandResult::LoadState,
            "SOUND" => out.line(
                "PC speaker, OPL3 FM on 388h and a Sound Blaster on 220h, IRQ 7, DMA 1 and 5.",
            ),
            "CYCLES" => match args.first().and_then(|value| value.parse::<u32>().ok()) {
                Some(cycles) => return CommandResult::SetCycles(cycles),
                None => out.line("Syntax: CYCLES <instructions per millisecond>"),
            },
            "HELP" => help(out),
            "ABOUT" => about(out),
            "DEMO" => {
                if let Some(node) = dos.resolve("BOX.COM") {
                    return CommandResult::Run(node, String::new());
                }
                out.line("BOX.COM is missing from C:");
            }
            "HELLO" => {
                if let Some(node) = dos.resolve("HELLO.COM") {
                    return CommandResult::Run(node, String::new());
                }
                out.line("HELLO.COM is missing from C:");
            }
            "RUN" => {
                let (target, tail) = split_command(rest);
                match self.find_program(target, dos) {
                    Some(Target::Program(node)) => {
                        return CommandResult::Run(node, tail.to_string())
                    }
                    Some(Target::Batch(path)) => {
                        let args = split_args(tail);
                        if !self.load_batch(dos, &path, &args) {
                            out.line("Batch file missing");
                        }
                    }
                    None if target.is_empty() => out.line("Syntax: RUN program"),
                    None => out.line("Bad command or file name"),
                }
            }
            "CALL" => {
                let (target, tail) = split_command(rest);
                match self.find_program(target, dos) {
                    Some(Target::Batch(path)) => {
                        let args = split_args(tail);
                        if !self.load_batch(dos, &path, &args) {
                            out.line("Batch files nest too deeply");
                        }
                    }
                    Some(Target::Program(node)) => {
                        return CommandResult::Run(node, tail.to_string())
                    }
                    None => {
                        return self.execute_inner(rest, dos, out.vga, out.mem, now_ms, depth + 1)
                    }
                }
            }
            "GOTO" => {
                let label = rest.trim().trim_start_matches(':').to_ascii_uppercase();
                let label: String = label.chars().take(8).collect();
                let Some(file) = self.batch.last_mut() else {
                    return CommandResult::None;
                };
                let found = file.lines.iter().position(|line| {
                    let Some(name) = line.trim().strip_prefix(':') else {
                        return false;
                    };
                    let name: String = name
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .chars()
                        .take(8)
                        .collect();
                    name == label
                });
                match found {
                    Some(at) => file.next = at + 1,
                    None => {
                        out.line("Label not found");
                        self.batch.pop();
                    }
                }
            }
            "SHIFT" => {
                if let Some(file) = self.batch.last_mut() {
                    file.shift = file.shift.saturating_add(1);
                }
            }
            "PAUSE" => {
                out.line("Press any key to continue . . .");
                self.waiting_for_key = true;
            }
            "EMM386" => {
                return match args.first().map(|word| word.to_ascii_uppercase()).as_deref() {
                    Some("ON") => CommandResult::SetMemoryManager(true),
                    Some("OFF") => CommandResult::SetMemoryManager(false),
                    _ => {
                        out.line("Syntax: EMM386 ON | OFF");
                        CommandResult::None
                    }
                };
            }
            "DPMI" => {
                use crate::dpmi::Mode;
                return match args.first().map(|word| word.to_ascii_uppercase()).as_deref() {
                    Some("ON") => CommandResult::SetDpmi(Mode::On),
                    Some("OFF") => CommandResult::SetDpmi(Mode::Off),
                    Some("AUTO") => CommandResult::SetDpmi(Mode::Auto),
                    _ => {
                        out.line("Syntax: DPMI ON | OFF | AUTO");
                        CommandResult::None
                    }
                };
            }
            "SBTYPE" => {
                use crate::sb::Model;
                let model = match args.first().map(|word| word.to_ascii_uppercase()).as_deref() {
                    Some("2") | Some("SB2") => Model::Sb2,
                    Some("PRO") | Some("SBPRO") => Model::SbPro,
                    Some("16") | Some("SB16") => Model::Sb16,
                    _ => {
                        out.line("Syntax: SBTYPE 2 | PRO | 16");
                        return CommandResult::None;
                    }
                };
                self.set_env("BLASTER", model.blaster(), dos, out.mem);
                out.line(&format!("{}: BLASTER={}", model.name(), model.blaster()));
                return CommandResult::SetSoundBlaster(model);
            }
            "SET" => {
                let text = rest.trim_start();
                if text.is_empty() {
                    let lines: Vec<String> = self
                        .env
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect();
                    for line in lines {
                        out.line(&line);
                    }
                } else if let Some((key, value)) = text.split_once('=') {
                    self.set_env(key.trim(), value, dos, out.mem);
                }
            }
            "PATH" => {
                let text = rest.trim_start();
                if text.is_empty() {
                    let value = self.get_env("PATH").unwrap_or("").to_string();
                    out.line(&format!("PATH={value}"));
                } else {
                    let value = text.trim_start_matches('=').to_string();
                    self.set_env("PATH", &value, dos, out.mem);
                }
            }
            "EXIT" => {
                self.exit = true;
                return CommandResult::Exit;
            }
            _ => out.line("Bad command or file name"),
        }
        CommandResult::None
    }
}

/// What a command word turned out to name.
enum Target {
    Program(usize),
    Batch(String),
}

/// The redirections on a command line.
#[derive(Default)]
struct Redirects {
    input: Option<String>,
    output: Option<(String, bool)>,
}

fn is_builtin(upper: &str) -> bool {
    matches!(
        upper,
        "CLS"
            | "ECHO"
            | "ECHO."
            | "PROMPT"
            | "VER"
            | "VOL"
            | "DATE"
            | "TIME"
            | "CD"
            | "CHDIR"
            | "MD"
            | "MKDIR"
            | "RD"
            | "RMDIR"
            | "DIR"
            | "TYPE"
            | "COPY"
            | "DEL"
            | "ERASE"
            | "REN"
            | "RENAME"
            | "MEM"
            | "MOUNT"
            | "UNMOUNT"
            | "GAMES"
            | "BEEP"
            | "SCREEN"
            | "SCREENSHOT"
            | "SAVESTATE"
            | "LOADSTATE"
            | "SOUND"
            | "CYCLES"
            | "HELP"
            | "ABOUT"
            | "DEMO"
            | "HELLO"
            | "RUN"
            | "CALL"
            | "GOTO"
            | "SHIFT"
            | "PAUSE"
            | "SET"
            | "SBTYPE"
            | "EMM386"
            | "DPMI"
            | "PATH"
            | "EXIT"
    )
}

/// Splits off the first word.
fn split_command(line: &str) -> (&str, &str) {
    let line = line.trim_start();
    let end = line
        .find(|ch: char| ch.is_ascii_whitespace())
        .unwrap_or(line.len());
    let (word, rest) = line.split_at(end);
    (word, rest.trim_start())
}

fn split_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(ToString::to_string).collect()
}

/// Removes a leading keyword, case-insensitively.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let (first, rest) = split_command(text);
    first.eq_ignore_ascii_case(word).then_some(rest)
}

/// Separates `<`, `>` and `>>` from a command line.
fn parse_redirects(line: &str) -> (String, Redirects) {
    let mut body = String::new();
    let mut redirects = Redirects::default();
    let mut rest = line;
    while let Some(at) = rest.find(['<', '>']) {
        body.push_str(&rest[..at]);
        let is_input = rest.as_bytes()[at] == b'<';
        let append = !is_input && rest[at..].starts_with(">>");
        let after = rest[at + if append { 2 } else { 1 }..].trim_start();
        let end = after
            .find(|ch: char| ch.is_ascii_whitespace() || ch == '<' || ch == '>')
            .unwrap_or(after.len());
        let (path, remainder) = after.split_at(end);
        if is_input {
            redirects.input = Some(path.to_string());
        } else {
            redirects.output = Some((path.to_string(), append));
        }
        rest = remainder;
    }
    body.push_str(rest);
    (body, redirects)
}

/// Writes a built-in's captured output to a file.
fn write_capture(dos: &mut Dos, path: &str, append: bool, data: &[u8]) -> bool {
    if matches!(path.to_ascii_uppercase().as_str(), "NUL" | "NUL.") {
        return true;
    }
    let (parent, name) = split_parent(dos, path);
    let Some(parent) = parent else {
        return false;
    };
    if append {
        if let Some(node) = dos
            .disk
            .lookup(parent, &name)
            .filter(|&node| dos.disk.is_file(node))
        {
            let offset = dos.disk.size(node);
            return dos.disk.write(node, offset, data) == data.len();
        }
    }
    let Some(copy) = try_copy(data) else {
        return false;
    };
    dos.disk.create_file(parent, &name, copy).is_some()
}

/// The DOS names matching a wildcard path.
fn find_matches(dos: &Dos, pattern: &str) -> Vec<String> {
    let (dir_text, name) = match pattern.rsplit_once(['\\', '/']) {
        Some((dir, name)) => (Some(dir), name),
        None => (None, pattern),
    };
    let dir = match dir_text {
        Some(dir) => dos.disk.resolve(dos.cwd, dir),
        None => Some(dos.cwd),
    };
    let Some(dir) = dir else {
        return Vec::new();
    };
    dos.disk
        .find(dir, name)
        .into_iter()
        .filter(|&node| dos.disk.is_file(node))
        .map(|node| {
            let short = dos.disk.nodes[node].short.clone();
            match dir_text {
                Some(prefix) => format!("{prefix}\\{short}"),
                None => short,
            }
        })
        .collect()
}

fn split_parent(dos: &Dos, path: &str) -> (Option<usize>, String) {
    let path = crate::ramdisk::strip_drive(path);
    let trimmed = path.trim_end_matches(['\\', '/']);
    let (dir, name) = match trimmed.rsplit_once(['\\', '/']) {
        Some((dir, name)) => (dir, name),
        None => ("", trimmed),
    };
    let parent = if dir.is_empty() {
        if trimmed.starts_with(['\\', '/']) {
            Some(dos.disk.root)
        } else {
            Some(dos.cwd)
        }
    } else {
        dos.disk.resolve(dos.cwd, dir)
    };
    (parent, name.to_string())
}

fn dir(dos: &Dos, out: &mut Out<'_>, args: &[&str]) {
    let target = args
        .iter()
        .find(|arg| !arg.starts_with('/'))
        .copied()
        .unwrap_or(".");
    let wide = args.iter().any(|arg| arg.eq_ignore_ascii_case("/W"));
    let wildcard = target.contains(['*', '?']);
    let (node, pattern) = if wildcard {
        let (dir_text, name) = match target.rsplit_once(['\\', '/']) {
            Some((dir, name)) => (dir, name),
            None => (".", target),
        };
        (dos.resolve(dir_text), Some(name))
    } else {
        (dos.resolve(target), None)
    };
    let Some(node) = node else {
        out.line("File Not Found");
        return;
    };
    if dos.disk.is_file(node) {
        if let Some(entry) = dos.disk.node(node) {
            out.line(&format!(
                "{:<12} {:>10}  {}",
                entry.short,
                entry.data.len(),
                entry.name
            ));
        }
        return;
    }

    let path = dos.disk.path(node);
    out.line(" Volume in drive C is FEOS-DOS");
    out.line(&format!(" Directory of C:{}", path));
    out.line("");

    let mut entries: Vec<usize> = match pattern {
        Some(pattern) => dos.disk.find(node, pattern),
        None => dos.disk.children(node).to_vec(),
    };
    entries.sort_by_key(|&child| dos.disk.nodes[child].short.clone());
    let mut files = 0usize;
    let mut dirs = 0usize;
    let mut bytes = 0usize;
    if wide {
        let mut line = String::new();
        for &child in &entries {
            let entry = &dos.disk.nodes[child];
            if entry.is_dir {
                dirs += 1;
            } else {
                files += 1;
                bytes += entry.data.len();
            }
            line.push_str(&format!("{:<14}", entry.short));
            if line.len() >= 70 {
                out.line(&line);
                line.clear();
            }
        }
        if !line.is_empty() {
            out.line(&line);
        }
    } else {
        for &child in &entries {
            let entry = &dos.disk.nodes[child];
            let long = if entry.name.eq_ignore_ascii_case(&entry.short) {
                ""
            } else {
                entry.name.as_str()
            };
            if entry.is_dir {
                dirs += 1;
                out.line(&format!(
                    "{:<12} {:<10} {:<8} {:<10} {}",
                    entry.short,
                    format_date(entry.date),
                    format_time(entry.time),
                    "<DIR>",
                    long
                ));
            } else {
                files += 1;
                bytes += entry.data.len();
                out.line(&format!(
                    "{:<12} {:<10} {:<8} {:>10} {}",
                    entry.short,
                    format_date(entry.date),
                    format_time(entry.time),
                    entry.data.len(),
                    long
                ));
            }
        }
    }
    out.line("");
    out.line(&format!("{:>10} file(s) {:>12} bytes", files, bytes));
    out.line(&format!(
        "{:>10} dir(s)  {:>12} bytes free",
        dirs,
        dos.disk.free_bytes()
    ));
}

fn copy(dos: &mut Dos, out: &mut Out<'_>, args: &[&str]) {
    if args.len() < 2 {
        out.line("Syntax: COPY source destination");
        return;
    }
    let Some(source) = dos.resolve(args[0]) else {
        out.line("File not found");
        return;
    };
    if !dos.disk.is_file(source) {
        out.line("File not found");
        return;
    }
    let Some(data) = dos.disk.node(source).and_then(|node| try_copy(&node.data)) else {
        out.line("Unable to copy file");
        return;
    };
    let source_name = dos
        .disk
        .node(source)
        .map(|node| node.name.clone())
        .unwrap_or_default();
    let destination = args[1];
    let (parent, name) = match dos.resolve(destination) {
        Some(node) if dos.disk.is_dir(node) => (Some(node), source_name),
        _ => split_parent(dos, destination),
    };
    let Some(parent) = parent else {
        out.line("Invalid destination");
        return;
    };
    if dos.disk.create_file(parent, &name, data).is_some() {
        out.line("        1 file(s) copied");
    } else {
        out.line("Unable to copy file");
    }
}

fn delete(dos: &mut Dos, out: &mut Out<'_>, args: &[&str]) {
    let Some(target) = args.first() else {
        out.line("Syntax: DEL file");
        return;
    };
    if target.contains('*') || target.contains('?') {
        let (dir, pattern) = match target.rsplit_once(['\\', '/']) {
            Some((dir, pattern)) => (dos.disk.resolve(dos.cwd, dir), pattern.to_string()),
            None => (Some(dos.cwd), target.to_string()),
        };
        let Some(dir) = dir else {
            out.line("File not found");
            return;
        };
        let mut removed = 0;
        for node in dos.disk.find(dir, &pattern) {
            if dos.disk.is_file(node) && dos.disk.remove(node) {
                removed += 1;
            }
        }
        out.line(&format!("{removed} file(s) deleted"));
        return;
    }
    match dos.resolve(target) {
        Some(node) if dos.disk.is_file(node) && dos.disk.remove(node) => {}
        _ => out.line("File not found"),
    }
}

fn rename(dos: &mut Dos, out: &mut Out<'_>, args: &[&str]) {
    if args.len() < 2 {
        out.line("Syntax: REN old new");
        return;
    }
    let Some(node) = dos.resolve(args[0]) else {
        out.line("File not found");
        return;
    };
    let (parent, name) = split_parent(dos, args[1]);
    let Some(parent) = parent else {
        out.line("Invalid name");
        return;
    };
    if !dos.disk.rename(node, parent, &name) {
        out.line("Duplicate file name or file not found");
    }
}

fn mem_command(dos: &mut Dos, out: &mut Out<'_>) {
    let total = crate::memory::CONVENTIONAL_KB as usize;
    let free = dos.largest_block(out.mem) as usize / 64;
    let extended = crate::memory::EXTENDED_KB as usize;
    out.line("");
    out.line("Memory Type        Total       Used       Free");
    out.line("----------------  --------   --------   --------");
    out.line(&format!(
        "Conventional      {:>6}K    {:>6}K    {:>6}K",
        total,
        total.saturating_sub(free),
        free
    ));
    out.line(&format!("Extended          {:>6}K", extended));
    out.line("");
    out.line(&format!("Largest executable program size {:>6}K", free));
    out.line(&format!(
        "Drive C: uses {}K of {}K",
        dos.disk.total_bytes() / 1024,
        dos.disk.quota / 1024
    ));
}

fn help(out: &mut Out<'_>) {
    out.line("");
    out.line(&format!("{} {} commands", PRODUCT, VERSION));
    out.line("-----------------------------");
    for line in [
        "DIR [path] [/W]   list files        CD [path]      change directory",
        "TYPE file         show a text file  COPY src dst   copy a file",
        "DEL file          delete a file     REN old new    rename a file",
        "MD dir            make a directory  RD dir         remove a directory",
        "CLS               clear the screen  ECHO text      print text",
        "PROMPT text       set the prompt    VER            show the version",
        "MEM               memory report     VOL            drive label",
        "DATE / TIME       show clock        SET / PATH     environment",
        "MOUNT name        copy a game in    UNMOUNT name   remove it again",
        "GAMES             list the library  SOUND          sound hardware",
        "SBTYPE 2|PRO|16   pick the card     EMM386 ON|OFF  memory manager, VCPI",
        "DPMI ON|OFF|AUTO  the DPMI host",
        "SCREEN            save a BMP        SAVESTATE      save the machine",
        "LOADSTATE         restore it        CYCLES n       CPU speed",
        "IF FOR GOTO CALL SHIFT PAUSE REM    batch file language; > >> < redirect",
        "DEMO / HELLO      run a .COM demo   RUN program    run a program",
        "ABOUT             about FEOS-DOS    EXIT           close the window",
    ] {
        out.line(line);
    }
    out.line("");
}

fn about(out: &mut Out<'_>) {
    out.line("");
    out.line(&format!(
        "{} {} - the FEOS Disk Operating System",
        PRODUCT, VERSION
    ));
    out.line("An x86 virtual machine and DOS written in Rust for feos.");
    out.line("");
    for line in [
        "  * 80386 interpreter: protected, virtual-8086 mode, paging, an x87",
        "  * 16 MiB RAM, XMS and EMS, 8259 PIC, 8254 PIT, 8042 and 8237 DMA",
        "  * VGA register core with text, CGA, EGA, VGA and tweaked modes",
        "  * BIOS and DOS services reached through the guest's own vectors",
        "  * OPL2/OPL3 FM, PC speaker and a Sound Blaster 2.0, Pro or 16",
        "  * a DPMI 0.9 host, and VCPI under EMM386 ON",
        "  * writable C: RAM drive with MOUNT, GAMES, SCREEN and SAVESTATE",
    ] {
        out.line(line);
    }
    out.line("");
    out.line("CD-ROM, networking, MIDI and recording are not implemented.");
    out.line("See docs/dosbox.md for game status.");
    out.line("");
}

fn format_date(date: u16) -> String {
    let year = 1980 + (date >> 9);
    let month = (date >> 5) & 0x0F;
    let day = date & 0x1F;
    format!("{:04}-{:02}-{:02}", year, month, day)
}

fn format_time(time: u16) -> String {
    let hour = time >> 11;
    let minute = (time >> 5) & 0x3F;
    let second = (time & 0x1F) * 2;
    format!("{:02}:{:02}:{:02}", hour, minute, second)
}

/// Formats an interrupt action for diagnostics.
pub fn action_name(action: InterruptAction) -> &'static str {
    match action {
        InterruptAction::Continue => "continue",
        InterruptAction::Blocked => "blocked",
        InterruptAction::Halt => "halt",
    }
}
