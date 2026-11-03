//! Script execution: pipelines, redirection, builtins and control flow.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_coreutils::{is_command, BytesInput, BytesOutput, Ctx, FsError, Output, Vfs};

use crate::lexer::{self, RedirOp, Word};
use crate::parser::{
    Command, ForCommand, IfCommand, List, Logic, Pipeline, Script, SimpleCommand, WhileCommand,
};
use crate::{Host, Io, Shell};

impl Shell<'_> {
    pub(crate) fn run_script(
        &mut self,
        script: &Script,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let mut status = 0;
        for list in &script.lists {
            status = self.run_list(list, io, host);
            self.last_status = status;
            self.env.set("?", &status.to_string());
            if self.exit_requested.is_some() || self.return_requested.is_some() {
                break;
            }
            if self.break_level > 0 || self.continue_level > 0 {
                break;
            }
        }
        status
    }

    fn run_list(&mut self, list: &List, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let mut status = self.run_pipeline(&list.first, io, host, list.background);
        self.last_status = status;
        self.env.set("?", &status.to_string());
        for (logic, pipeline) in &list.rest {
            if self.exit_requested.is_some() || self.return_requested.is_some() {
                break;
            }
            let should_run = match logic {
                Logic::And => status == 0,
                Logic::Or => status != 0,
            };
            if should_run {
                status = self.run_pipeline(pipeline, io, host, list.background);
                self.last_status = status;
                self.env.set("?", &status.to_string());
            }
        }
        status
    }

    fn run_pipeline(
        &mut self,
        pipeline: &Pipeline,
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        let mut status;

        if pipeline.commands.len() == 1 {
            status = self.run_command(&pipeline.commands[0], io, host, background);
        } else {
            status = 0;
            let count = pipeline.commands.len();
            let mut previous: Vec<u8> = Vec::new();

            for (index, command) in pipeline.commands.iter().enumerate() {
                let last = index + 1 == count;
                let mut input = BytesInput::new(core::mem::take(&mut previous));
                let mut output = BytesOutput::new();

                status = {
                    let mut stage = Io {
                        stdin: if index == 0 {
                            &mut *io.stdin
                        } else {
                            &mut input
                        },
                        stdout: if last { &mut *io.stdout } else { &mut output },
                        stderr: &mut *io.stderr,
                    };
                    self.run_command(command, &mut stage, host, false)
                };

                previous = output.into_bytes();
            }
        }

        if pipeline.negate {
            status = if status == 0 { 1 } else { 0 };
        }
        status
    }

    fn run_command(
        &mut self,
        command: &Command,
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        match command {
            Command::Simple(simple) => self.run_simple(simple, io, host, background),
            Command::If(command) => self.run_if(command, io, host),
            Command::For(command) => self.run_for(command, io, host),
            Command::While(command) => self.run_while(command, io, host),
            Command::Group(script) => self.run_script(script, io, host),
        }
    }

    fn run_if(&mut self, command: &IfCommand, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        for (condition, body) in &command.branches {
            if self.run_script(condition, io, host) == 0 {
                return self.run_script(body, io, host);
            }
            if self.exit_requested.is_some() || self.return_requested.is_some() {
                return self.last_status;
            }
        }
        match &command.else_body {
            Some(body) => self.run_script(body, io, host),
            None => 0,
        }
    }

    fn run_for(&mut self, command: &ForCommand, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let values = match &command.words {
            Some(words) => self.expand_words(words, io, host),
            None => Vec::new(),
        };
        let mut status = 0;
        self.loop_depth += 1;
        for value in values {
            self.env.set(&command.variable, &value);
            status = self.run_script(&command.body, io, host);
            if self.exit_requested.is_some() || self.return_requested.is_some() {
                break;
            }
            if self.break_level > 0 {
                self.break_level -= 1;
                break;
            }
            if self.continue_level > 0 {
                self.continue_level -= 1;
                if self.continue_level > 0 {
                    break;
                }
            }
        }
        self.loop_depth -= 1;
        status
    }

    fn run_while(&mut self, command: &WhileCommand, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let mut status = 0;
        let mut iterations = 0usize;
        self.loop_depth += 1;

        loop {
            let condition = self.run_script(&command.condition, io, host);
            let keep_going = if command.until {
                condition != 0
            } else {
                condition == 0
            };
            if !keep_going {
                break;
            }

            status = self.run_script(&command.body, io, host);
            iterations += 1;
            if iterations > 1_000_000 {
                error(io, "loop limit reached (1000000 iterations)");
                status = 1;
                break;
            }
            if self.exit_requested.is_some() || self.return_requested.is_some() {
                break;
            }
            if self.break_level > 0 {
                self.break_level -= 1;
                break;
            }
            if self.continue_level > 0 {
                self.continue_level -= 1;
                if self.continue_level > 0 {
                    break;
                }
            }
        }

        self.loop_depth -= 1;
        status
    }

    fn expand_words(
        &mut self,
        words: &[Word],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Vec<String> {
        let mut result = Vec::new();
        for word in words {
            result.extend(self.expand_word(word, io, host));
        }
        result
    }

    fn run_simple(
        &mut self,
        command: &SimpleCommand,
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        let mut saved: Vec<(String, Option<String>)> = Vec::new();
        let has_command = !command.words.is_empty();

        for (name, value) in &command.assignments {
            let expanded = self.expand_one(value, io, host).unwrap_or_default();
            if has_command {
                saved.push((name.clone(), self.env.get(name).map(str::to_string)));
            }
            self.env.set(name, &expanded);
        }

        let mut argv = self.expand_words(&command.words, io, host);
        self.apply_alias(&mut argv, io, host);

        let status = if argv.is_empty() {
            self.run_redirections_only(&command.redirs, io, host)
        } else {
            self.run_with_redirections(&command.redirs, &argv, io, host, background)
        };

        if has_command {
            for (name, value) in saved.into_iter().rev() {
                match value {
                    Some(value) => self.env.set(&name, &value),
                    None => {
                        self.env.unset(&name);
                    }
                }
            }
        }
        status
    }

    fn apply_alias(&mut self, argv: &mut Vec<String>, io: &mut Io<'_>, host: &mut dyn Host) {
        let Some(name) = argv.first().cloned() else {
            return;
        };
        let Some(alias) = self.aliases.get(&name).cloned() else {
            return;
        };
        let Ok(tokens) = lexer::lex(&alias) else {
            return;
        };
        let Ok(script) = crate::parser::parse(&tokens) else {
            return;
        };
        let Some(List { first, .. }) = script.lists.first() else {
            return;
        };
        let Some(Command::Simple(simple)) = first.commands.first() else {
            return;
        };
        let mut expanded = self.expand_words(&simple.words, io, host);
        expanded.extend(argv.drain(1..));
        *argv = expanded;
    }

    fn run_redirections_only(
        &mut self,
        redirs: &[crate::parser::Redirection],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let mut status = 0;
        for redir in redirs {
            match redir.op {
                RedirOp::Write(fd) | RedirOp::Append(fd) => {
                    let Some(target) = redir.target.as_ref() else {
                        continue;
                    };
                    let Some(path) = self.expand_one(target, io, host) else {
                        continue;
                    };
                    let append = matches!(redir.op, RedirOp::Append(_));
                    if fd == 1 || fd == 2 {
                        if let Err(err) = self.fs.write_file(&path, b"", append) {
                            path_error(io, "sh", &path, err);
                            status = 1;
                        }
                    }
                }
                RedirOp::WriteBoth | RedirOp::AppendBoth => {
                    let Some(target) = redir.target.as_ref() else {
                        continue;
                    };
                    let Some(path) = self.expand_one(target, io, host) else {
                        continue;
                    };
                    let append = matches!(redir.op, RedirOp::AppendBoth);
                    if let Err(err) = self.fs.write_file(&path, b"", append) {
                        path_error(io, "sh", &path, err);
                        status = 1;
                    }
                }
                _ => {}
            }
        }
        status
    }

    fn run_with_redirections(
        &mut self,
        redirs: &[crate::parser::Redirection],
        argv: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        let mut input_data: Option<Vec<u8>> = None;
        let mut stdout_file: Option<(String, bool)> = None;
        let mut stderr_file: Option<(String, bool)> = None;
        let mut stderr_to_stdout = false;
        let mut stdout_both = false;

        for redir in redirs {
            match redir.op {
                RedirOp::Read(fd) => {
                    if fd != 0 {
                        error(io, "only file descriptor 0 can be redirected for input");
                        continue;
                    }
                    let Some(target) = redir.target.as_ref() else {
                        continue;
                    };
                    let Some(path) = self.expand_one(target, io, host) else {
                        continue;
                    };
                    match self.fs.read_file(&path) {
                        Ok(data) => input_data = Some(data),
                        Err(err) => {
                            path_error(io, "sh", &path, err);
                            return 1;
                        }
                    }
                }
                RedirOp::Write(fd) | RedirOp::Append(fd) => {
                    let Some(target) = redir.target.as_ref() else {
                        continue;
                    };
                    let Some(path) = self.expand_one(target, io, host) else {
                        continue;
                    };
                    let append = matches!(redir.op, RedirOp::Append(_));
                    if fd == 1 {
                        stdout_file = Some((path, append));
                        stdout_both = false;
                    } else if fd == 2 {
                        stderr_file = Some((path, append));
                    } else {
                        error(io, "only file descriptors 1 and 2 can be redirected");
                    }
                }
                RedirOp::WriteBoth | RedirOp::AppendBoth => {
                    let Some(target) = redir.target.as_ref() else {
                        continue;
                    };
                    let Some(path) = self.expand_one(target, io, host) else {
                        continue;
                    };
                    stdout_file = Some((path, matches!(redir.op, RedirOp::AppendBoth)));
                    stdout_both = true;
                }
                RedirOp::Dup(fd, target) => {
                    if fd == 2 && target == 1 {
                        stderr_to_stdout = true;
                    } else {
                        error(io, "unsupported file descriptor duplication");
                    }
                }
            }
        }

        let has_input = input_data.is_some();
        let mut file_input = BytesInput::new(input_data.unwrap_or_default());
        let mut output_buffer = BytesOutput::new();
        let mut error_buffer = BytesOutput::new();
        let buffer_output = stdout_file.is_some();
        let buffer_error = stderr_file.is_some() || stderr_to_stdout || stdout_both;

        let status = {
            let mut stage = Io {
                stdin: if has_input {
                    &mut file_input
                } else {
                    &mut *io.stdin
                },
                stdout: if buffer_output {
                    &mut output_buffer
                } else {
                    &mut *io.stdout
                },
                stderr: if buffer_error {
                    &mut error_buffer
                } else {
                    &mut *io.stderr
                },
            };
            self.dispatch(argv, &mut stage, host, background)
        };

        if stderr_to_stdout {
            if buffer_output {
                output_buffer.write(error_buffer.bytes());
            } else {
                io.stdout.write(error_buffer.bytes());
            }
        } else if stdout_both {
            output_buffer.write(error_buffer.bytes());
        }

        if let Some((path, append)) = stderr_file {
            if let Err(err) = self.fs.write_file(&path, error_buffer.bytes(), append) {
                path_error(io, "sh", &path, err);
            }
        }
        if let Some((path, append)) = stdout_file {
            if let Err(err) = self.fs.write_file(&path, output_buffer.bytes(), append) {
                path_error(io, "sh", &path, err);
                return 1;
            }
        }

        status
    }

    fn dispatch(
        &mut self,
        argv: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        let Some(name) = argv.first() else {
            return 0;
        };
        let args = &argv[1..];

        if let Some(status) = self.run_builtin(name, args, io, host) {
            return status;
        }

        if is_command(name) {
            let mut context = Ctx {
                fs: &mut self.fs,
                env: &mut self.env,
                stdin: io.stdin,
                stdout: io.stdout,
                stderr: io.stderr,
                system: self.system,
            };
            return feos_coreutils::run(name, args, &mut context);
        }

        if !io.stdin.is_terminal() || !io.stdout.is_terminal() {
            error(
                io,
                "external commands cannot be used in pipelines or with redirection",
            );
            return 1;
        }

        match host.run_external(name, args, io.stdin, io.stdout, io.stderr, background) {
            Some(status) => status,
            None => {
                let _ = io.stderr.write_str("sh: ");
                let _ = io.stderr.write_str(name);
                let _ = io.stderr.write_str(": command not found\n");
                127
            }
        }
    }

    fn run_builtin(
        &mut self,
        name: &str,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Option<i32> {
        match name {
            "cd" => Some(self.builtin_cd(args, io)),
            "pwd" => {
                io.stdout.write(self.fs.cwd().as_bytes());
                io.stdout.write(b"\n");
                Some(0)
            }
            "export" => Some(self.builtin_export(args, io)),
            "unset" => {
                for name in args {
                    self.env.unset(name);
                }
                Some(0)
            }
            "set" => Some(self.builtin_set(args, io)),
            "exit" => {
                let status = args
                    .first()
                    .and_then(|value| value.parse::<i32>().ok())
                    .unwrap_or(self.last_status);
                self.exit_requested = Some(status);
                Some(status)
            }
            "return" => {
                let status = args
                    .first()
                    .and_then(|value| value.parse::<i32>().ok())
                    .unwrap_or(self.last_status);
                if self.loop_depth == 0 {
                    self.return_requested = Some(status);
                }
                Some(status)
            }
            "break" => {
                let level = args
                    .first()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1)
                    .max(1);
                if self.loop_depth == 0 {
                    error(io, "break: only meaningful in a loop");
                    return Some(1);
                }
                self.break_level = level;
                Some(0)
            }
            "continue" => {
                let level = args
                    .first()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1)
                    .max(1);
                if self.loop_depth == 0 {
                    error(io, "continue: only meaningful in a loop");
                    return Some(1);
                }
                self.continue_level = level;
                Some(0)
            }
            "source" | "." => {
                let Some(path) = args.first() else {
                    error(io, "source: filename argument required");
                    return Some(2);
                };
                let status = self.source(path, io, host);
                if let Some(returned) = self.return_requested.take() {
                    Some(returned)
                } else {
                    Some(status)
                }
            }
            "eval" => {
                let text = args.join(" ");
                Some(self.execute_line(&text, io, host))
            }
            "alias" => Some(self.builtin_alias(args, io)),
            "unalias" => {
                if args.first().map(String::as_str) == Some("-a") {
                    self.aliases.clear();
                } else {
                    for name in args {
                        self.aliases.remove(name);
                    }
                }
                Some(0)
            }
            "history" => {
                if args.first().map(String::as_str) == Some("-c") {
                    self.history.clear();
                    return Some(0);
                }
                for (index, line) in self.history.iter().enumerate() {
                    io.stdout
                        .write(format!("{:>5}  {line}\n", index + 1).as_bytes());
                }
                Some(0)
            }
            "help" => Some(self.builtin_help(args, io)),
            "type" => Some(self.builtin_type(args, io, host, false)),
            "command" => {
                if args.first().map(String::as_str) == Some("-v") {
                    Some(self.builtin_type(&args[1..], io, host, true))
                } else if args.is_empty() {
                    Some(0)
                } else {
                    let status = self.dispatch(args, io, host, false);
                    Some(status)
                }
            }
            "read" => Some(self.builtin_read(args, io)),
            "clear" => {
                io.stdout.write(b"\x1b[2J\x1b[H");
                Some(0)
            }
            "true" | ":" => Some(0),
            "false" => Some(1),
            "sh" | "bash" => Some(self.builtin_sh(args, io, host)),
            _ => None,
        }
    }

    fn builtin_cd(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        if args.len() > 1 {
            error(io, "cd: too many arguments");
            return 1;
        }
        let target = match args.first().map(String::as_str) {
            None => self.env.get("HOME").unwrap_or("/").to_string(),
            Some("-") => {
                let old = self.env.get("OLDPWD").unwrap_or("/").to_string();
                io.stdout.write(old.as_bytes());
                io.stdout.write(b"\n");
                old
            }
            Some(path) => path.to_string(),
        };
        let previous = self.fs.cwd().to_string();
        match self.fs.chdir(&target) {
            Ok(()) => {
                self.env.set("OLDPWD", &previous);
                self.env.set("PWD", self.fs.cwd());
                0
            }
            Err(err) => {
                path_error(io, "cd", &target, err);
                1
            }
        }
    }

    fn builtin_export(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        if args.is_empty() {
            for (name, value) in self.env.iter() {
                io.stdout
                    .write(format!("declare -x {name}=\"{value}\"\n").as_bytes());
            }
            return 0;
        }
        for arg in args {
            match arg.split_once('=') {
                Some((name, value)) => self.env.set(name, value),
                None => {
                    if self.env.get(arg).is_none() {
                        self.env.set(arg, "");
                    }
                }
            }
        }
        0
    }

    fn builtin_set(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        if !args.is_empty() {
            // POSIX shell options are accepted but not implemented yet.
            return 0;
        }
        for (name, value) in self.env.iter() {
            io.stdout.write(format!("{name}='{value}'\n").as_bytes());
        }
        0
    }

    fn builtin_alias(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        if args.is_empty() {
            for (name, value) in &self.aliases {
                io.stdout
                    .write(format!("alias {name}='{value}'\n").as_bytes());
            }
            return 0;
        }
        let mut status = 0;
        for arg in args {
            if let Some((name, value)) = arg.split_once('=') {
                self.aliases
                    .insert(name.to_string(), value.trim_matches('\'').to_string());
            } else if let Some(value) = self.aliases.get(arg) {
                io.stdout
                    .write(format!("alias {arg}='{value}'\n").as_bytes());
            } else {
                let _ = io.stderr.write_str("alias: ");
                let _ = io.stderr.write_str(arg);
                let _ = io.stderr.write_str(": not found\n");
                status = 1;
            }
        }
        status
    }

    fn builtin_help(&self, args: &[String], io: &mut Io<'_>) -> i32 {
        if let Some(name) = args.first() {
            if let Some(spec) = feos_coreutils::command(name) {
                io.stdout.write(
                    format!(
                        "{}: {}\nusage: {}\n",
                        spec.name, spec.description, spec.usage
                    )
                    .as_bytes(),
                );
                return 0;
            }
            if crate::builtin_names().contains(&name.as_str()) {
                io.stdout
                    .write(format!("{name}: shell builtin\n").as_bytes());
                return 0;
            }
            let _ = io.stderr.write_str("help: no help for ");
            let _ = io.stderr.write_str(name);
            io.stderr.write(b"\n");
            return 1;
        }

        io.stdout.write(b"Shell builtins:\n");
        for name in crate::builtin_names() {
            io.stdout.write(format!("  {name}\n").as_bytes());
        }
        io.stdout.write(b"\nCore utilities:\n");
        for spec in feos_coreutils::COMMANDS {
            io.stdout
                .write(format!("  {:<18} {}\n", spec.usage, spec.description).as_bytes());
        }
        0
    }

    fn builtin_type(
        &self,
        args: &[String],
        io: &mut Io<'_>,
        host: &dyn Host,
        path_only: bool,
    ) -> i32 {
        let mut status = 0;
        for name in args {
            if let Some(value) = self.aliases.get(name) {
                if path_only {
                    io.stdout.write(format!("{name}\n").as_bytes());
                } else {
                    io.stdout
                        .write(format!("{name} is aliased to '{value}'\n").as_bytes());
                }
            } else if crate::builtin_names().contains(&name.as_str()) {
                if path_only {
                    io.stdout.write(format!("{name}\n").as_bytes());
                } else {
                    io.stdout
                        .write(format!("{name} is a shell builtin\n").as_bytes());
                }
            } else if is_command(name) {
                if path_only {
                    io.stdout.write(format!("/bin/{name}\n").as_bytes());
                } else {
                    io.stdout
                        .write(format!("{name} is a coreutils command\n").as_bytes());
                }
            } else if host.command_names().contains(&name.as_str()) {
                if path_only {
                    io.stdout.write(format!("/bin/{name}\n").as_bytes());
                } else {
                    io.stdout
                        .write(format!("{name} is an external command\n").as_bytes());
                }
            } else {
                let _ = io.stderr.write_str("type: ");
                let _ = io.stderr.write_str(name);
                let _ = io.stderr.write_str(": not found\n");
                status = 1;
            }
        }
        status
    }

    fn builtin_read(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let mut raw = false;
        let mut prompt: Option<String> = None;
        let mut names: Vec<String> = Vec::new();
        let mut index = 0usize;

        while index < args.len() {
            match args[index].as_str() {
                "-r" => raw = true,
                "-p" => {
                    index += 1;
                    prompt = args.get(index).cloned();
                }
                "-n" => {
                    index += 1;
                }
                other => names.push(other.to_string()),
            }
            index += 1;
        }
        if names.is_empty() {
            names.push(String::from("REPLY"));
        }
        if let Some(prompt) = prompt {
            io.stderr.write(prompt.as_bytes());
        }

        let Some(line) = io.stdin.read_line() else {
            return 1;
        };
        let mut text = String::from_utf8_lossy(&line).into_owned();
        if text.ends_with('\n') {
            text.pop();
        }
        if text.ends_with('\r') {
            text.pop();
        }
        if !raw {
            text = text.replace("\\ ", " ");
        }

        let fields: Vec<&str> = text.split_whitespace().collect();
        for (index, name) in names.iter().enumerate() {
            let value = if index + 1 == names.len() {
                fields[index.min(fields.len())..].join(" ")
            } else {
                fields.get(index).copied().unwrap_or("").to_string()
            };
            self.env.set(name, &value);
        }
        0
    }

    fn builtin_sh(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        match args.first().map(String::as_str) {
            None => crate::interactive(self, host, io),
            Some("-c") => {
                let text = args[1..].join(" ");
                self.execute_line(&text, io, host)
            }
            Some(path) => self.source(path, io, host),
        }
    }
}

fn error(io: &mut Io<'_>, message: &str) {
    let _ = io.stderr.write_str("sh: ");
    let _ = io.stderr.write_str(message);
    io.stderr.write(b"\n");
}

fn path_error(io: &mut Io<'_>, command: &str, path: &str, error: FsError) {
    let _ = io.stderr.write_str("sh: ");
    let _ = io.stderr.write_str(command);
    let _ = io.stderr.write_str(": ");
    let _ = io.stderr.write_str(path);
    let _ = io.stderr.write_str(": ");
    let _ = io.stderr.write_str(error.text());
    io.stderr.write(b"\n");
}
