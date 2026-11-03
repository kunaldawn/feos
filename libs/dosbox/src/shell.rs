//! A small `COMMAND.COM`-style prompt.
//!
//! The prompt edits its line on the emulated text screen and implements the
//! built-ins a DOS user expects: `DIR`, `CD`, `TYPE`, `COPY`, `DEL`, `REN`,
//! `MD`, `RD`, `CLS`, `ECHO`, `PROMPT`, `VER`, `MEM`, `VOL`, `DATE`, `TIME`,
//! `HELP`, `ABOUT`, `DEMO`, `RUN` and `EXIT`. Batch files are read line by
//! line, and a program line pauses the batch until the program returns.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::bios::{clock_date, clock_time};
use crate::cpu::InterruptAction;
use crate::dos::Dos;
use crate::keyboard::KeyEvent;
use crate::memory::Memory;
use crate::ramdisk::dos_name;
use crate::vga::Vga;
use crate::{PRODUCT, VERSION};

/// What the prompt wants the machine to do after a command.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommandResult {
    /// The command finished; show the prompt again.
    None,
    /// Load and run a program, with the command tail to place in the PSP.
    Run(usize, String),
    /// Save a screenshot of the current screen to `C:\SCREEN.BMP`.
    Screenshot,
    /// Save the machine state to `C:\STATE.BIN`.
    SaveState,
    /// Restore the machine state from `C:\STATE.BIN`.
    LoadState,
    /// Close the DOS window.
    Exit,
}

/// The prompt state.
pub struct Shell {
    pub line: Vec<u8>,
    pub prompt: String,
    pub exit: bool,
    pub echo_off: bool,
    pub batch: VecDeque<String>,
    pub history: Vec<String>,
    pub history_index: usize,
    pub last_command: String,
    pub running: bool,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    pub fn new() -> Self {
        Self {
            line: Vec::new(),
            prompt: "$P$G".to_string(),
            exit: false,
            echo_off: false,
            batch: VecDeque::new(),
            history: Vec::new(),
            history_index: 0,
            last_command: String::new(),
            running: false,
        }
    }

    pub fn next_batch(&mut self) -> Option<String> {
        self.batch.pop_front()
    }

    pub fn queue_batch(&mut self, lines: impl IntoIterator<Item = String>) {
        for line in lines {
            self.batch.push_back(line);
        }
    }

    /// Loads a `.BAT` file into the batch queue.
    pub fn load_batch(&mut self, dos: &mut Dos, path: &str) -> bool {
        let Some(node) = dos.resolve(path) else {
            return false;
        };
        if !dos.disk.is_file(node) {
            return false;
        }
        let data = dos
            .disk
            .node(node)
            .map(|node| node.data.clone())
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&data);
        let mut lines = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r').trim();
            if line.is_empty()
                || line.starts_with(':')
                || line.to_ascii_uppercase().starts_with("REM ")
            {
                continue;
            }
            lines.push(line.to_string());
        }
        self.queue_batch(lines);
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

    /// Handles one key. Returns a completed command line when Enter is pressed.
    pub fn feed_key(
        &mut self,
        key: KeyEvent,
        dos: &Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        now_ms: u64,
    ) -> Option<String> {
        match key.ascii {
            0x0D => {
                vga.teletype(mem, b'\r');
                vga.teletype(mem, b'\n');
                let line = String::from_utf8_lossy(&self.line).into_owned();
                self.line.clear();
                if !line.trim().is_empty() {
                    self.history.push(line.clone());
                    self.history_index = self.history.len();
                }
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
                while self.line.len() < next {
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
        vga.teletype(mem, b'\r');
        let prompt = self.expand_prompt(dos, now_ms);
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
        if let Some(rest) = line.strip_prefix('@') {
            line = rest.trim();
        }
        if line.is_empty() {
            return CommandResult::None;
        }
        let mut parts = line.split_whitespace();
        let command = parts.next().unwrap_or("");
        let args: Vec<&str> = parts.collect();
        let upper = command.to_ascii_uppercase();

        match upper.as_str() {
            "CLS" => vga.clear(mem, 0x07),
            "ECHO" => {
                let text = line[command.len()..].trim_start();
                if text.eq_ignore_ascii_case("OFF") {
                    self.echo_off = true;
                } else if text.eq_ignore_ascii_case("ON") {
                    self.echo_off = false;
                } else if text.is_empty() {
                    vga.teletype(mem, b'\r');
                    vga.teletype(mem, b'\n');
                } else {
                    vga.write_line(mem, text.as_bytes());
                }
            }
            "PROMPT" => {
                let text = line[command.len()..].trim_start();
                self.prompt = if text.is_empty() {
                    "$P$G".to_string()
                } else {
                    text.to_string()
                };
            }
            "VER" => {
                vga.write_line(
                    mem,
                    format!(
                        "{} Version {} (DOS {}.0 compatible)",
                        PRODUCT,
                        VERSION,
                        crate::DOS_VERSION
                    )
                    .as_bytes(),
                );
            }
            "VOL" => {
                vga.write_line(mem, b" Volume in drive C is FEOS-DOS");
            }
            "DATE" => {
                let (century, year, month, day) = clock_date(now_ms);
                let date = format!(
                    "{:02}-{:02}-{:02}",
                    (century as u16) * 100 + year as u16,
                    month,
                    day
                );
                vga.write_line(mem, format!("Current date is {date}").as_bytes());
            }
            "TIME" => {
                let (hour, minute, second) = clock_time(now_ms);
                vga.write_line(
                    mem,
                    format!("Current time is {:02}:{:02}:{:02}", hour, minute, second).as_bytes(),
                );
            }
            "CD" | "CHDIR" => {
                let target = args.first().copied().unwrap_or("\\");
                match dos.resolve(target) {
                    Some(node) if dos.disk.is_dir(node) => dos.cwd = node,
                    _ => vga.write_line(mem, b"Invalid directory"),
                }
            }
            "MD" | "MKDIR" => {
                if let Some(target) = args.first() {
                    let (parent, name) = split_parent(dos, target);
                    match parent {
                        Some(parent) if dos.disk.create_dir(parent, &name).is_some() => {}
                        _ => vga.write_line(mem, b"Unable to create directory"),
                    }
                }
            }
            "RD" | "RMDIR" => {
                if let Some(target) = args.first() {
                    match dos.resolve(target) {
                        Some(node) if dos.disk.remove(node) => {}
                        _ => vga.write_line(mem, b"Invalid directory or directory not empty"),
                    }
                }
            }
            "DIR" => dir(self, dos, vga, mem, &args),
            "TYPE" => {
                let Some(target) = args.first() else {
                    vga.write_line(mem, b"Syntax: TYPE file");
                    return CommandResult::None;
                };
                match dos.resolve(target) {
                    Some(node) if dos.disk.is_file(node) => {
                        let data = dos
                            .disk
                            .node(node)
                            .map(|node| node.data.clone())
                            .unwrap_or_default();
                        vga.write_str(mem, &data);
                        if !data.ends_with(b"\n") {
                            vga.teletype(mem, b'\r');
                            vga.teletype(mem, b'\n');
                        }
                    }
                    _ => vga.write_line(mem, b"File not found"),
                }
            }
            "COPY" => copy(dos, vga, mem, &args),
            "DEL" | "ERASE" => delete(dos, vga, mem, &args),
            "REN" | "RENAME" => rename(dos, vga, mem, &args),
            "MEM" => mem_command(dos, vga, mem),
            "MOUNT" => {
                let Some(name) = args.first() else {
                    print_line(vga, mem, "Syntax: MOUNT <directory>");
                    return CommandResult::None;
                };
                match dos.mount_any(name, name) {
                    Ok(count) => {
                        print_line(vga, mem, &format!("Mounted C:\\{name} ({count} entries)"))
                    }
                    Err(error) => print_line(vga, mem, &format!("MOUNT: {error}")),
                }
            }
            "GAMES" => {
                if let Some(data) = dos.read_fs_file("GAMES.TXT") {
                    vga.write_str(mem, &data);
                } else {
                    let entries = dos.fs_entries();
                    if entries.is_empty() {
                        print_line(vga, mem, "No FEOS filesystem is mounted on this board.");
                    } else {
                        print_line(vga, mem, "Directories on the FEOS filesystem:");
                        for (name, is_dir) in entries {
                            print_line(
                                vga,
                                mem,
                                &format!("  {:<12} {}", name, if is_dir { "<DIR>" } else { "" }),
                            );
                        }
                    }
                }
            }
            "BEEP" => vga.teletype(mem, 0x07),
            "SCREEN" | "SCREENSHOT" => return CommandResult::Screenshot,
            "SAVESTATE" => return CommandResult::SaveState,
            "LOADSTATE" => return CommandResult::LoadState,
            "SOUND" => print_line(vga, mem, "Sound cards are not implemented yet."),
            "PROTECTED" => print_line(
                vga,
                mem,
                "Protected mode and DOS extenders are not implemented yet.",
            ),
            "HELP" => help(vga, mem),
            "ABOUT" => about(vga, mem),
            "DEMO" => {
                if let Some(node) = dos.resolve("BOX.COM") {
                    return CommandResult::Run(node, String::new());
                }
                vga.write_line(mem, b"DEMO.COM is missing from C:");
            }
            "HELLO" => {
                if let Some(node) = dos.resolve("HELLO.COM") {
                    return CommandResult::Run(node, String::new());
                }
                vga.write_line(mem, b"HELLO.COM is missing from C:");
            }
            "RUN" => {
                if let Some(target) = args.first() {
                    return self.run_target(dos, vga, mem, target, &args[1..]);
                }
                vga.write_line(mem, b"Syntax: RUN program");
            }
            "EXIT" => {
                self.exit = true;
                return CommandResult::Exit;
            }
            "PAUSE" => {
                vga.write_line(mem, b"Press any key to continue . . .");
            }
            "SET" | "PATH" => {
                if upper == "PATH" && args.is_empty() {
                    vga.write_line(mem, b"PATH=C:\\");
                }
            }
            "REM" => {}
            _ => {
                let tail = line[command.len()..].trim_start();
                return self.run_target(dos, vga, mem, command, &[tail]);
            }
        }
        CommandResult::None
    }

    fn run_target(
        &mut self,
        dos: &mut Dos,
        vga: &mut Vga,
        mem: &mut Memory,
        target: &str,
        tail: &[&str],
    ) -> CommandResult {
        let mut candidates = Vec::new();
        let upper = target.to_ascii_uppercase();
        if upper.ends_with(".COM") || upper.ends_with(".EXE") || upper.ends_with(".BAT") {
            candidates.push(target.to_string());
        } else {
            candidates.push(format!("{target}.COM"));
            candidates.push(format!("{target}.EXE"));
            candidates.push(format!("{target}.BAT"));
        }
        for candidate in candidates {
            let Some(node) = dos.resolve(&candidate) else {
                continue;
            };
            if !dos.disk.is_file(node) {
                continue;
            }
            let name = dos
                .disk
                .node(node)
                .map(|node| node.name.clone())
                .unwrap_or_default();
            if name.to_ascii_uppercase().ends_with(".BAT") {
                let _ = self.load_batch(dos, &candidate);
                return CommandResult::None;
            }
            let tail = tail.join(" ");
            return CommandResult::Run(node, tail);
        }
        vga.write_line(mem, b"Bad command or file name");
        CommandResult::None
    }
}

fn split_parent(dos: &Dos, path: &str) -> (Option<usize>, String) {
    let path = crate::ramdisk::strip_drive(path);
    let trimmed = path.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        return (None, String::new());
    }
    let absolute = trimmed.starts_with('\\') || trimmed.starts_with('/');
    let (dir_part, name) = match trimmed.rsplit_once(['\\', '/']) {
        Some((dir, name)) => (dir, name),
        None => ("", trimmed),
    };
    let dir = if dir_part.is_empty() {
        if absolute {
            Some(dos.disk.root)
        } else {
            Some(dos.cwd)
        }
    } else {
        dos.disk.resolve(dos.cwd, dir_part)
    };
    (dir, name.to_string())
}

fn print_line(vga: &mut Vga, mem: &mut Memory, text: &str) {
    vga.write_line(mem, text.as_bytes());
}

fn dir(shell: &Shell, dos: &Dos, vga: &mut Vga, mem: &mut Memory, args: &[&str]) {
    let _ = shell;
    let target = args
        .iter()
        .find(|arg| !arg.starts_with('/'))
        .copied()
        .unwrap_or(".");
    let wide = args.iter().any(|arg| arg.eq_ignore_ascii_case("/W"));
    let Some(node) = dos.resolve(target) else {
        print_line(vga, mem, "File Not Found");
        return;
    };
    if dos.disk.is_file(node) {
        if let Some(entry) = dos.disk.node(node) {
            print_line(
                vga,
                mem,
                &format!(
                    "{:<12} {:>10}  {}",
                    dos_name(&entry.name),
                    entry.data.len(),
                    entry.name
                ),
            );
        }
        return;
    }

    let path = dos.disk.path(node);
    print_line(vga, mem, " Volume in drive C is FEOS-DOS");
    print_line(vga, mem, &format!(" Directory of C:{}", path));
    print_line(vga, mem, "");

    let mut entries: Vec<usize> = dos.disk.children(node).to_vec();
    entries.sort_by_key(|&child| dos_name(&dos.disk.nodes[child].name));
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
            line.push_str(&format!("{:<14}", dos_name(&entry.name)));
            if line.len() >= 70 {
                print_line(vga, mem, &line);
                line.clear();
            }
        }
        if !line.is_empty() {
            print_line(vga, mem, &line);
        }
    } else {
        for &child in &entries {
            let entry = &dos.disk.nodes[child];
            if entry.is_dir {
                dirs += 1;
                print_line(
                    vga,
                    mem,
                    &format!(
                        "{:<12} {:<8} {:<8} {:<10} {}",
                        dos_name(&entry.name),
                        format_date(entry.date),
                        format_time(entry.time),
                        "<DIR>",
                        entry.name
                    ),
                );
            } else {
                files += 1;
                bytes += entry.data.len();
                print_line(
                    vga,
                    mem,
                    &format!(
                        "{:<12} {:<8} {:<8} {:>10} {}",
                        dos_name(&entry.name),
                        format_date(entry.date),
                        format_time(entry.time),
                        entry.data.len(),
                        entry.name
                    ),
                );
            }
        }
    }
    print_line(vga, mem, "");
    print_line(
        vga,
        mem,
        &format!("{:>10} file(s) {:>12} bytes", files, bytes),
    );
    print_line(
        vga,
        mem,
        &format!("{:>10} dir(s)  {:>12} bytes free", dirs, 640 * 1024),
    );
}

fn copy(dos: &mut Dos, vga: &mut Vga, mem: &mut Memory, args: &[&str]) {
    if args.len() < 2 {
        print_line(vga, mem, "Syntax: COPY source destination");
        return;
    }
    let Some(source) = dos.resolve(args[0]) else {
        print_line(vga, mem, "File not found");
        return;
    };
    if !dos.disk.is_file(source) {
        print_line(vga, mem, "File not found");
        return;
    }
    let data = dos
        .disk
        .node(source)
        .map(|node| node.data.clone())
        .unwrap_or_default();
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
        print_line(vga, mem, "Invalid destination");
        return;
    };
    if dos.disk.create_file(parent, &name, data).is_some() {
        print_line(vga, mem, "        1 file(s) copied");
    } else {
        print_line(vga, mem, "Unable to copy file");
    }
}

fn delete(dos: &mut Dos, vga: &mut Vga, mem: &mut Memory, args: &[&str]) {
    let Some(target) = args.first() else {
        print_line(vga, mem, "Syntax: DEL file");
        return;
    };
    if target.contains('*') || target.contains('?') {
        let (dir, pattern) = match target.rsplit_once(['\\', '/']) {
            Some((dir, pattern)) => (dos.disk.resolve(dos.cwd, dir), pattern.to_string()),
            None => (Some(dos.cwd), target.to_string()),
        };
        let Some(dir) = dir else {
            print_line(vga, mem, "File not found");
            return;
        };
        let mut removed = 0;
        for node in dos.disk.find(dir, &pattern) {
            if dos.disk.is_file(node) && dos.disk.remove(node) {
                removed += 1;
            }
        }
        print_line(vga, mem, &format!("{removed} file(s) deleted"));
        return;
    }
    match dos.resolve(target) {
        Some(node) if dos.disk.is_file(node) && dos.disk.remove(node) => {}
        _ => print_line(vga, mem, "File not found"),
    }
}

fn rename(dos: &mut Dos, vga: &mut Vga, mem: &mut Memory, args: &[&str]) {
    if args.len() < 2 {
        print_line(vga, mem, "Syntax: REN old new");
        return;
    }
    let Some(node) = dos.resolve(args[0]) else {
        print_line(vga, mem, "File not found");
        return;
    };
    let (parent, name) = split_parent(dos, args[1]);
    let Some(parent) = parent else {
        print_line(vga, mem, "Invalid name");
        return;
    };
    if !dos.disk.rename(node, parent, &name) {
        print_line(vga, mem, "Duplicate file name or file not found");
    }
}

fn mem_command(dos: &Dos, vga: &mut Vga, mem: &mut Memory) {
    let used = dos.disk.total_bytes();
    let total: usize = 640 * 1024;
    let free = total.saturating_sub(used);
    print_line(vga, mem, "");
    print_line(vga, mem, "Memory Type        Total       Used       Free");
    print_line(vga, mem, "----------------  --------   --------   --------");
    print_line(
        vga,
        mem,
        &format!(
            "Conventional      {:>6}K    {:>6}K    {:>6}K",
            total / 1024,
            used / 1024,
            free / 1024
        ),
    );
    print_line(vga, mem, "Upper                  0K         0K         0K");
    print_line(vga, mem, "Extended               0K         0K         0K");
    print_line(vga, mem, "");
    print_line(
        vga,
        mem,
        &format!("Largest executable program size {:>6}K", free / 1024),
    );
}

fn help(vga: &mut Vga, mem: &mut Memory) {
    print_line(vga, mem, "");
    print_line(vga, mem, &format!("{} {} commands", PRODUCT, VERSION));
    print_line(vga, mem, "-----------------------------");
    print_line(
        vga,
        mem,
        "DIR [path] [/W]   list files        CD [path]      change directory",
    );
    print_line(
        vga,
        mem,
        "TYPE file         show a text file  COPY src dst   copy a file",
    );
    print_line(
        vga,
        mem,
        "DEL file          delete a file     REN old new    rename a file",
    );
    print_line(
        vga,
        mem,
        "MD dir            make a directory  RD dir         remove a directory",
    );
    print_line(
        vga,
        mem,
        "CLS               clear the screen  ECHO text      print text",
    );
    print_line(
        vga,
        mem,
        "PROMPT text       set the prompt    VER            show the version",
    );
    print_line(
        vga,
        mem,
        "MEM               memory report     VOL            drive label",
    );
    print_line(
        vga,
        mem,
        "DATE / TIME       show clock        SET / PATH     environment",
    );
    print_line(
        vga,
        mem,
        "DEMO / HELLO      run a .COM demo    RUN program    run a program",
    );
    print_line(
        vga,
        mem,
        "ABOUT             about FEOS-DOS     EXIT           close the window",
    );
    print_line(vga, mem, "");
}

fn about(vga: &mut Vga, mem: &mut Memory) {
    print_line(vga, mem, "");
    print_line(
        vga,
        mem,
        &format!("{} {} - the FEOS Disk Operating System", PRODUCT, VERSION),
    );
    print_line(
        vga,
        mem,
        "An original 8086 virtual machine written in Rust for feos.",
    );
    print_line(vga, mem, "");
    print_line(
        vga,
        mem,
        "  * 8086/80186 CPU interpreter with segmentation and interrupts",
    );
    print_line(
        vga,
        mem,
        "  * 1 MiB RAM, BIOS data area and interrupt vector table",
    );
    print_line(
        vga,
        mem,
        "  * VGA 80x25 text mode with the CP437 character set",
    );
    print_line(
        vga,
        mem,
        "  * INT 10h video, INT 16h keyboard, INT 1Ah clock, INT 21h DOS",
    );
    print_line(
        vga,
        mem,
        "  * writable C: RAM drive with MOUNT, GAMES and SCREEN",
    );
    print_line(vga, mem, "");
    print_line(
        vga,
        mem,
        "Mode X/VESA, transcendental x87, protected mode/DOS extenders, sound,",
    );
    print_line(
        vga,
        mem,
        "networking, save states and recording are not implemented yet.",
    );
    print_line(vga, mem, "");
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
