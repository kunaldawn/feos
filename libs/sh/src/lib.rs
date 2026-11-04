//! A Bash-compatible shell language for FEOS.
//!
//! The library owns the parser, expansion engine, builtins and interactive
//! line editor. It does not know about the app registry: the embedding
//! application implements [`Host`] to launch external commands.
//!
//! Supported syntax includes quoting, parameter/command/arithmetic
//! substitution, globbing, pipelines, redirections, `&&`, `||`, `;`, `&`,
//! variables, aliases, `if`/`for`/`while`/`until`, `break`/`continue` and
//! `source`. Shell functions, arrays and full job control are not implemented
//! yet.
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

mod editor;
mod exec;
mod expand;
pub mod lexer;
pub mod parser;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_console::read_byte;
use feos_coreutils::{Env, Input, Output, OverlayFs, System, Vfs, COMMANDS};
use feos_fs::FileSystem;

/// A byte stream bundle handed to a command.
pub struct Io<'a> {
    /// Standard input.
    pub stdin: &'a mut dyn Input,
    /// Standard output.
    pub stdout: &'a mut dyn Output,
    /// Standard error.
    pub stderr: &'a mut dyn Output,
}

/// Console input backed by the registered FEOS console.
pub struct ConsoleInput;

impl Input for ConsoleInput {
    fn read(&mut self, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }
        loop {
            if let Some(byte) = read_byte() {
                out[0] = byte;
                return 1;
            }
            feos_console::wait_for_input();
        }
    }

    fn is_terminal(&self) -> bool {
        true
    }
}

/// Console output backed by the registered FEOS console.
pub struct ConsoleOutput;

impl Output for ConsoleOutput {
    fn write(&mut self, bytes: &[u8]) {
        feos_console::write_bytes(bytes);
    }

    fn is_terminal(&self) -> bool {
        true
    }
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

    /// Names of external commands, used by tab completion.
    fn command_names(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

/// A shell instance with its own filesystem overlay, variables and history.
pub struct Shell<'a> {
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
    exit_requested: Option<i32>,
    return_requested: Option<i32>,
    break_level: usize,
    continue_level: usize,
    loop_depth: usize,
}

impl<'a> Shell<'a> {
    /// Creates a shell over an optional read-only base filesystem.
    pub fn new(base: Option<&'static dyn FileSystem>, system: &'a dyn System) -> Self {
        let mut aliases = BTreeMap::new();
        aliases.insert(String::from("ll"), String::from("ls -l"));
        aliases.insert(String::from("la"), String::from("ls -a"));
        aliases.insert(String::from("l"), String::from("ls -1"));
        aliases.insert(String::from(".."), String::from("cd .."));
        aliases.insert(String::from("..."), String::from("cd ../.."));
        aliases.insert(String::from("h"), String::from("history"));

        let mut shell = Self {
            fs: OverlayFs::new(base),
            env: Env::feos(),
            aliases,
            history: Vec::new(),
            last_status: 0,
            system,
            exit_requested: None,
            return_requested: None,
            break_level: 0,
            continue_level: 0,
            loop_depth: 0,
        };
        shell.env.set("PWD", shell.fs.cwd());
        shell
    }

    /// Lexes, parses and runs `line`, returning its status.
    pub fn execute_line(&mut self, line: &str, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return self.last_status;
        }

        let tokens = match lexer::lex(line) {
            Ok(tokens) => tokens,
            Err(error) => {
                let _ = io.stderr.write_str("sh: ");
                let _ = io.stderr.write_str(error.message);
                io.stderr.write(b"\n");
                self.last_status = 2;
                return 2;
            }
        };
        let script = match parser::parse(&tokens) {
            Ok(script) => script,
            Err(error) => {
                let _ = io.stderr.write_str("sh: syntax error: ");
                let _ = io.stderr.write_str(error.message);
                io.stderr.write(b"\n");
                self.last_status = 2;
                return 2;
            }
        };

        let status = self.run_script(&script, io, host);
        self.last_status = status;
        self.env.set("?", &status.to_string());
        status
    }

    /// Runs a script from a string (used by `source` and `sh script`).
    ///
    /// The whole text is parsed at once, so multi-line `if`/`for`/`while`
    /// constructs work exactly as they do in Bash.
    pub fn run_text(&mut self, text: &str, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let tokens = match lexer::lex(text) {
            Ok(tokens) => tokens,
            Err(error) => {
                let _ = io.stderr.write_str("sh: ");
                let _ = io.stderr.write_str(error.message);
                io.stderr.write(b"\n");
                self.last_status = 2;
                return 2;
            }
        };
        let script = match parser::parse(&tokens) {
            Ok(script) => script,
            Err(error) => {
                let _ = io.stderr.write_str("sh: syntax error: ");
                let _ = io.stderr.write_str(error.message);
                io.stderr.write(b"\n");
                self.last_status = 2;
                return 2;
            }
        };

        let status = self.run_script(&script, io, host);
        self.last_status = status;
        self.env.set("?", &status.to_string());
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
                Some('w') => prompt.push_str(&self.display_cwd()),
                Some('W') => prompt.push_str(feos_coreutils::base_name(self.fs.cwd())),
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
        let cursor = cursor.min(line.len());
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

        if candidates.is_empty() {
            return Completion {
                line: line.to_string(),
                cursor,
                candidates: Vec::new(),
            };
        }

        if candidates.len() == 1 {
            let mut replacement = candidates[0].clone();
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

    /// Runs a script read from `path`.
    pub(crate) fn source(&mut self, path: &str, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        match self.fs.read_file(path) {
            Ok(data) => {
                let text = String::from_utf8_lossy(&data);
                self.run_text(&text, io, host)
            }
            Err(error) => {
                let _ = io.stderr.write_str("sh: ");
                let _ = io.stderr.write_str(path);
                let _ = io.stderr.write_str(": ");
                let _ = io.stderr.write_str(error.text());
                io.stderr.write(b"\n");
                1
            }
        }
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
    for value in &values[1..] {
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
        "alias", "break", "cd", "clear", "command", "continue", "eval", "exit", "export", "help",
        "history", "pwd", "read", "return", "set", "source", ".", "type", "unalias", "unset",
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
                if shell.history.last() != Some(&line) {
                    shell.history.push(line.clone());
                    if shell.history.len() > 200 {
                        shell.history.remove(0);
                    }
                }
                shell.execute_line(&line, io, host);
                if let Some(status) = shell.exit_requested.take() {
                    return status;
                }
            }
            editor::ReadResult::Eof => {
                io.stdout.write(b"\n");
                return 0;
            }
            editor::ReadResult::Interrupted => {
                shell.last_status = 130;
                shell.env.set("?", "130");
            }
        }
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

    static CONSOLE_INPUT: std::sync::Mutex<std::collections::VecDeque<u8>> =
        std::sync::Mutex::new(std::collections::VecDeque::new());

    fn test_console_reader() -> Option<u8> {
        CONSOLE_INPUT.lock().unwrap().pop_front()
    }

    #[test]
    fn the_interactive_loop_reads_the_registered_console() {
        {
            let mut input = CONSOLE_INPUT.lock().unwrap();
            input.clear();
            input.extend(b"echo hi\r\x04");
        }
        feos_console::set_reader(test_console_reader);

        let mut shell = Shell::new(None, &NoSystem);
        let mut host = NoHost;
        let mut stdin = crate::ConsoleInput;
        let mut stdout = crate::ConsoleOutput;
        let mut stderr = crate::ConsoleOutput;
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };

        assert_eq!(crate::interactive(&mut shell, &mut host, &mut io), 0);
        assert_eq!(shell.history, ["echo hi"]);
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
}
