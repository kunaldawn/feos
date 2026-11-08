//! The shell's built-in commands.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_coreutils::{is_command, FsError, Vfs, MAX_STREAM_BYTES};

use crate::lexer::valid_name;
use crate::{Flow, Host, Io, Shell};

/// Puts single quotes around a value for `set`, `export -p` and `alias`.
fn shell_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

impl Shell<'_> {
    pub(crate) fn run_builtin(
        &mut self,
        name: &str,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Option<i32> {
        Some(match name {
            "cd" => self.builtin_cd(args, io),
            "pwd" => {
                let cwd = format!("{}\n", self.fs.cwd());
                self.write_out(io, &cwd);
                0
            }
            "export" => self.builtin_export(args, io),
            "readonly" => self.builtin_readonly(args, io),
            "local" => self.builtin_local(args, io),
            "declare" | "typeset" => self.builtin_declare(args, io),
            "unset" => self.builtin_unset(args, io),
            "set" => self.builtin_set(args, io),
            "shift" => self.builtin_shift(args, io),
            "exit" => self.builtin_exit(args, io),
            "return" => self.builtin_return(args, io),
            "break" | "continue" => self.builtin_loop_control(name, args, io),
            "source" | "." => self.builtin_source(args, io, host),
            "eval" => self.builtin_eval(args, io, host),
            "exec" => self.builtin_exec(args, io, host),
            "alias" => self.builtin_alias(args, io),
            "unalias" => {
                if args.first().map(String::as_str) == Some("-a") {
                    self.aliases.clear();
                } else {
                    for name in args {
                        self.aliases.remove(name);
                    }
                }
                0
            }
            "history" => {
                if args.first().map(String::as_str) == Some("-c") {
                    self.history.clear();
                    return Some(0);
                }
                let mut text = String::new();
                for (index, line) in self.history.iter().enumerate() {
                    text.push_str(&format!("{:>5}  {line}\n", index + 1));
                }
                self.write_out(io, &text);
                0
            }
            "help" => self.builtin_help(args, io, host),
            "type" => self.builtin_type(args, io, host, false),
            "which" => self.builtin_which(args, io, host),
            "command" => match args.first().map(String::as_str) {
                Some("-v") => self.builtin_type(&args[1..], io, host, true),
                Some("-V") => self.builtin_type(&args[1..], io, host, false),
                Some("--") => self.dispatch(&args[1..], io, host, false, false),
                None => 0,
                Some(_) => self.dispatch(args, io, host, false, false),
            },
            "builtin" => match args.split_first() {
                None => 0,
                Some((name, rest)) => match self.run_builtin(name, rest, io, host) {
                    Some(status) => status,
                    None => {
                        self.error(io, &format!("builtin: {name}: not a shell builtin"));
                        1
                    }
                },
            },
            "read" => self.builtin_read(args, io),
            "getopts" => self.builtin_getopts(args, io),
            "let" => self.builtin_let(args, io, host),
            "timeout" => self.builtin_timeout(args, io, host),
            "jobs" => self.builtin_jobs(args, io, host),
            "fg" => self.builtin_fg(args, io, host),
            "bg" => self.builtin_bg(args, io, host),
            "wait" => self.builtin_wait(args, io, host),
            "trap" => self.builtin_trap(args, io, host),
            "kill" => return self.builtin_kill(args, io, host),
            "clear" => {
                self.write_out(io, "\x1b[2J\x1b[H");
                0
            }
            "true" | ":" => 0,
            "false" => 1,
            "sh" | "bash" => self.builtin_sh(args, io, host),
            _ => return None,
        })
    }

    fn builtin_cd(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let args = match args.first().map(String::as_str) {
            Some("-L" | "-P") => &args[1..],
            Some("--") => &args[1..],
            _ => args,
        };
        if args.len() > 1 {
            self.error(io, "cd: too many arguments");
            return 1;
        }
        let target = match args.first().map(String::as_str) {
            None => self.env.get("HOME").unwrap_or("/").to_string(),
            Some("-") => {
                let old = self.env.get("OLDPWD").unwrap_or("/").to_string();
                self.write_out(io, &format!("{old}\n"));
                old
            }
            Some(path) => path.to_string(),
        };
        let previous = self.fs.cwd().to_string();
        match self.fs.chdir(&target) {
            Ok(()) => {
                self.env.set("OLDPWD", &previous);
                let cwd = self.fs.cwd().to_string();
                self.env.set("PWD", &cwd);
                0
            }
            Err(error) => {
                self.error(io, &format!("cd: {target}: {}", error.text()));
                1
            }
        }
    }

    /// Handles `name` or `name=value` for the declaration builtins.
    fn declare_one(&mut self, arg: &str, io: &mut Io<'_>) -> Result<String, i32> {
        let (name, value) = match arg.split_once('=') {
            Some((name, value)) => match name.strip_suffix('+') {
                Some(name) => {
                    let mut combined = self.env.get(name).unwrap_or("").to_string();
                    combined.push_str(value);
                    (name.to_string(), Some(combined))
                }
                None => (name.to_string(), Some(value.to_string())),
            },
            None => (arg.to_string(), None),
        };
        if !valid_name(&name) {
            self.error(io, &format!("`{arg}': not a valid identifier"));
            return Err(1);
        }
        if let Some(value) = value {
            if let Err(error) = self.env.try_set(&name, &value) {
                self.error(io, &format!("{name}: {}", error.text()));
                return Err(1);
            }
        }
        Ok(name)
    }

    fn print_declarations(&mut self, io: &mut Io<'_>, exported: bool) {
        let mut text = String::new();
        for (name, value) in self.env.iter() {
            let selected = if exported {
                self.env.is_exported(name)
            } else {
                self.env.is_readonly(name)
            };
            if selected {
                let keyword = if exported { "export" } else { "readonly" };
                text.push_str(&format!("{keyword} {name}={}\n", shell_quote(value)));
            }
        }
        self.write_out(io, &text);
    }

    fn builtin_export(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let mut remove = false;
        let mut names = args;
        while let Some(flag) = names.first().filter(|arg| arg.starts_with('-')) {
            match flag.as_str() {
                "-n" => remove = true,
                "-p" => {}
                "--" => {
                    names = &names[1..];
                    break;
                }
                other => {
                    self.error(io, &format!("export: {other}: invalid option"));
                    return 2;
                }
            }
            names = &names[1..];
        }
        if names.is_empty() {
            self.print_declarations(io, true);
            return 0;
        }
        let mut status = 0;
        for arg in names {
            match self.declare_one(arg, io) {
                Ok(name) => {
                    if remove {
                        self.env.unexport(&name);
                    } else {
                        // `export x` on an unset name marks it for when it is set.
                        if self.env.get(&name).is_none() {
                            self.env.set(&name, "");
                        }
                        self.env.export(&name);
                    }
                }
                Err(code) => status = code,
            }
        }
        status
    }

    fn builtin_readonly(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let names = match args.first().map(String::as_str) {
            Some("-p") => &args[1..],
            _ => args,
        };
        if names.is_empty() {
            self.print_declarations(io, false);
            return 0;
        }
        let mut status = 0;
        for arg in names {
            match self.declare_one(arg, io) {
                Ok(name) => self.env.set_readonly(&name),
                Err(code) => status = code,
            }
        }
        status
    }

    fn builtin_local(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        if self.locals.is_empty() {
            self.error(io, "local: can only be used in a function");
            return 1;
        }
        let mut status = 0;
        let mut readonly = false;
        let mut export = false;
        for arg in args {
            match arg.as_str() {
                "-r" => {
                    readonly = true;
                    continue;
                }
                "-x" => {
                    export = true;
                    continue;
                }
                _ => {}
            }
            let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
            let name = name.strip_suffix('+').unwrap_or(name).to_string();
            if self.env.is_readonly(&name) {
                self.error(io, &format!("{name}: readonly variable"));
                status = 1;
                continue;
            }
            let saved = self.env.var(&name);
            if let Some(frame) = self.locals.last_mut() {
                if !frame.iter().any(|(known, _)| *known == name) {
                    frame.push((name.clone(), saved));
                }
            }
            // A local starts empty unless the name is assigned or re-declared.
            if !arg.contains('=') {
                self.env.unset(&name);
            }
            match self.declare_one(arg, io) {
                Ok(_) => {
                    if export {
                        self.env.export(&name);
                    }
                    if readonly {
                        self.env.set_readonly(&name);
                    }
                }
                Err(code) => status = code,
            }
        }
        status
    }

    fn builtin_declare(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        if !self.locals.is_empty() {
            return self.builtin_local(args, io);
        }
        let mut export = false;
        let mut readonly = false;
        let mut status = 0;
        for arg in args {
            match arg.as_str() {
                "-x" => export = true,
                "-r" => readonly = true,
                _ => match self.declare_one(arg, io) {
                    Ok(name) => {
                        if export {
                            self.env.export(&name);
                        }
                        if readonly {
                            self.env.set_readonly(&name);
                        }
                    }
                    Err(code) => status = code,
                },
            }
        }
        status
    }

    fn builtin_unset(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let mut functions = false;
        let mut status = 0;
        for arg in args {
            match arg.as_str() {
                "-f" => functions = true,
                "-v" => functions = false,
                name if functions => {
                    self.functions.remove(name);
                }
                name => {
                    if self.env.is_readonly(name) {
                        self.error(io, &format!("unset: {name}: cannot unset: readonly variable"));
                        status = 1;
                        continue;
                    }
                    // `unset` falls back to a function, as POSIX allows.
                    if self.env.unset(name).is_none() {
                        self.functions.remove(name);
                    }
                }
            }
        }
        status
    }

    pub(crate) fn option_flags(&self) -> String {
        let mut flags = String::new();
        for (enabled, flag) in [
            (self.opts.errexit, 'e'),
            (self.opts.noglob, 'f'),
            (self.opts.nounset, 'u'),
            (self.opts.xtrace, 'x'),
            (self.opts.noclobber, 'C'),
        ] {
            if enabled {
                flags.push(flag);
            }
        }
        flags
    }

    fn set_short_option(&mut self, flag: char, enabled: bool) -> bool {
        match flag {
            'e' => self.opts.errexit = enabled,
            'u' => self.opts.nounset = enabled,
            'x' => self.opts.xtrace = enabled,
            'f' => self.opts.noglob = enabled,
            'C' => self.opts.noclobber = enabled,
            _ => return false,
        }
        true
    }

    fn set_long_option(&mut self, name: &str, enabled: bool) -> bool {
        match name {
            "errexit" => self.opts.errexit = enabled,
            "nounset" => self.opts.nounset = enabled,
            "xtrace" => self.opts.xtrace = enabled,
            "noglob" => self.opts.noglob = enabled,
            "noclobber" => self.opts.noclobber = enabled,
            "pipefail" => self.opts.pipefail = enabled,
            _ => return false,
        }
        true
    }

    fn builtin_set(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        if args.is_empty() {
            let mut text = String::new();
            for (name, value) in self.env.iter() {
                text.push_str(&format!("{name}={}\n", shell_quote(value)));
            }
            self.write_out(io, &text);
            return 0;
        }

        let mut index = 0usize;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                self.params = args[index + 1..].to_vec();
                return 0;
            }
            let (enabled, flags) = match arg.as_bytes().first() {
                Some(b'-') => (true, &arg[1..]),
                Some(b'+') => (false, &arg[1..]),
                _ => break,
            };
            if flags.is_empty() {
                // A lone `-` ends the options, keeping them as they were.
                index += 1;
                break;
            }
            for flag in flags.chars() {
                if flag == 'o' {
                    index += 1;
                    let Some(name) = args.get(index) else {
                        self.print_options(io);
                        return 0;
                    };
                    let name = name.clone();
                    if !self.set_long_option(&name, enabled) {
                        self.error(io, &format!("set: {name}: invalid option name"));
                        return 2;
                    }
                } else if !self.set_short_option(flag, enabled) {
                    self.error(io, &format!("set: -{flag}: invalid option"));
                    return 2;
                }
            }
            index += 1;
        }
        if index < args.len() {
            self.params = args[index..].to_vec();
        }
        0
    }

    fn print_options(&mut self, io: &mut Io<'_>) {
        let mut text = String::new();
        for (name, enabled) in [
            ("errexit", self.opts.errexit),
            ("noclobber", self.opts.noclobber),
            ("noglob", self.opts.noglob),
            ("nounset", self.opts.nounset),
            ("pipefail", self.opts.pipefail),
            ("xtrace", self.opts.xtrace),
        ] {
            let state = if enabled { "on" } else { "off" };
            text.push_str(&format!("{name:<15} {state}\n"));
        }
        self.write_out(io, &text);
    }

    fn builtin_shift(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let count = match args.first() {
            None => 1,
            Some(text) => match text.parse::<usize>() {
                Ok(count) => count,
                Err(_) => {
                    self.error(io, &format!("shift: {text}: numeric argument required"));
                    return 1;
                }
            },
        };
        if count > self.params.len() {
            return 1;
        }
        self.params.drain(..count);
        0
    }

    fn status_argument(&mut self, name: &str, args: &[String], io: &mut Io<'_>) -> i32 {
        match args.first() {
            None => self.last_status,
            Some(text) => match text.parse::<i64>() {
                Ok(status) => (status & 0xff) as i32,
                Err(_) => {
                    self.error(io, &format!("{name}: {text}: numeric argument required"));
                    2
                }
            },
        }
    }

    fn builtin_exit(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let status = self.status_argument("exit", args, io);
        self.flow = Flow::Exit(status);
        status
    }

    fn builtin_return(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let status = self.status_argument("return", args, io);
        if self.function_depth == 0 && self.source_depth == 0 {
            self.error(io, "return: can only be used in a function or sourced script");
            return 1;
        }
        self.flow = Flow::Return(status);
        status
    }

    fn builtin_loop_control(&mut self, name: &str, args: &[String], io: &mut Io<'_>) -> i32 {
        let levels = match args.first() {
            None => 1,
            Some(text) => match text.parse::<usize>() {
                Ok(levels) if levels > 0 => levels,
                _ => {
                    self.error(io, &format!("{name}: {text}: loop count out of range"));
                    return 1;
                }
            },
        };
        if self.loop_depth == 0 {
            self.error(io, &format!("{name}: only meaningful in a loop"));
            return 0;
        }
        // More levels than loops means all of them.
        let levels = levels.min(self.loop_depth);
        self.flow = if name == "break" {
            Flow::Break(levels)
        } else {
            Flow::Continue(levels)
        };
        0
    }

    fn builtin_source(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let Some((path, rest)) = args.split_first() else {
            self.error(io, "source: filename argument required");
            return 2;
        };
        let data = match self.fs.read_file(path) {
            Ok(data) => data,
            Err(FsError::NotFound) if !path.contains('/') => {
                match self.find_in_path(path).map(|found| self.fs.read_file(&found)) {
                    Some(Ok(data)) => data,
                    _ => {
                        self.error(io, &format!("{path}: {}", FsError::NotFound.text()));
                        return 1;
                    }
                }
            }
            Err(error) => {
                self.error(io, &format!("{path}: {}", error.text()));
                return 1;
            }
        };
        if !self.enter_nested(io) {
            return 2;
        }
        let text = String::from_utf8_lossy(&data);
        let saved = (!rest.is_empty()).then(|| core::mem::replace(&mut self.params, rest.to_vec()));
        self.source_depth += 1;
        let mut status = self.run_text_inner(&text, io, host);
        self.source_depth -= 1;
        if let Flow::Return(code) = self.flow {
            self.flow = Flow::None;
            status = code;
        }
        if let Some(saved) = saved {
            self.params = saved;
        }
        self.leave_nested();
        status
    }

    fn builtin_eval(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        if !self.enter_nested(io) {
            return 2;
        }
        let text = args.join(" ");
        let status = self.run_text_inner(&text, io, host);
        self.leave_nested();
        status
    }

    fn builtin_exec(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        if args.is_empty() {
            // `exec > f` keeps its redirections after the command ends.
            self.keep_redirects = true;
            return 0;
        }
        let status = self.dispatch(args, io, host, false, true);
        self.flow = Flow::Exit(status);
        status
    }

    fn builtin_alias(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        if args.is_empty() {
            let mut text = String::new();
            for (name, value) in &self.aliases {
                text.push_str(&format!("alias {name}={}\n", shell_quote(value)));
            }
            self.write_out(io, &text);
            return 0;
        }
        let mut status = 0;
        for arg in args {
            if let Some((name, value)) = arg.split_once('=') {
                self.aliases.insert(name.to_string(), value.to_string());
            } else if let Some(value) = self.aliases.get(arg).cloned() {
                self.write_out(io, &format!("alias {arg}={}\n", shell_quote(&value)));
            } else {
                self.write_err(io, &format!("alias: {arg}: not found\n"));
                status = 1;
            }
        }
        status
    }

    fn builtin_help(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        if let Some(name) = args.first() {
            if let Some(spec) = feos_coreutils::command(name) {
                let text = format!(
                    "{}: {}\nusage: {}\n",
                    spec.name, spec.description, spec.usage
                );
                self.write_out(io, &text);
                return 0;
            }
            if crate::builtin_names().contains(&name.as_str()) {
                self.write_out(io, &format!("{name}: shell builtin\n"));
                return 0;
            }
            if host.has_command(name) {
                self.write_out(io, &format!("{name}: application\n"));
                return 0;
            }
            self.write_err(io, &format!("help: no help for {name}\n"));
            return 1;
        }

        let mut text = String::from("Shell builtins:\n");
        for name in crate::builtin_names() {
            text.push_str(&format!("  {name}\n"));
        }
        text.push_str("\nCore utilities:\n");
        for spec in feos_coreutils::COMMANDS {
            text.push_str(&format!("  {:<18} {}\n", spec.usage, spec.description));
        }
        let names = host.command_names();
        if !names.is_empty() {
            text.push_str("\nApplications:\n");
            for name in names {
                text.push_str(&format!("  {name}\n"));
            }
        }
        self.write_out(io, &text);
        0
    }

    fn builtin_type(
        &mut self,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
        short: bool,
    ) -> i32 {
        let mut status = 0;
        for name in args {
            let line = if let Some(value) = self.aliases.get(name) {
                if short {
                    format!("alias {name}={}\n", shell_quote(value))
                } else {
                    format!("{name} is aliased to '{value}'\n")
                }
            } else if self.functions.contains_key(name) {
                if short {
                    format!("{name}\n")
                } else {
                    format!("{name} is a function\n")
                }
            } else if crate::builtin_names().contains(&name.as_str()) {
                if short {
                    format!("{name}\n")
                } else {
                    format!("{name} is a shell builtin\n")
                }
            } else if let Some(path) = self.locate(name, host) {
                if short {
                    format!("{path}\n")
                } else if is_command(name) {
                    format!("{name} is a coreutils command\n")
                } else if host.has_command(name) {
                    format!("{name} is an external command\n")
                } else {
                    format!("{name} is {path}\n")
                }
            } else {
                if !short {
                    self.write_err(io, &format!("type: {name}: not found\n"));
                }
                status = 1;
                continue;
            };
            self.write_out(io, &line);
        }
        status
    }

    /// Finds the path a command would run from, without shell-level names.
    fn locate(&mut self, name: &str, host: &dyn Host) -> Option<String> {
        if name.contains('/') {
            return self.fs.exists(name).then(|| name.to_string());
        }
        if is_command(name) || host.has_command(name) {
            return Some(format!("/bin/{name}"));
        }
        self.find_in_path(name)
    }

    fn builtin_which(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let names = match args.first().map(String::as_str) {
            Some("-a") => &args[1..],
            _ => args,
        };
        let mut status = 0;
        for name in names {
            match self.locate(name, host) {
                Some(path) => self.write_out(io, &format!("{path}\n")),
                None => status = 1,
            }
        }
        status
    }

    fn builtin_let(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        if args.is_empty() {
            self.error(io, "let: expression expected");
            return 1;
        }
        let mut last = 0;
        for expression in args {
            match self.eval_arith_text(expression, io, host) {
                Ok(value) => last = value,
                Err(_) => return 1,
            }
        }
        i32::from(last == 0)
    }

    fn builtin_timeout(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let Some((limit, command)) = args.split_first() else {
            self.error(io, "timeout: usage: timeout SECONDS command [arg...]");
            return 125;
        };
        // `1.5`, `30s`, `2m` and `1h`, as in coreutils.
        let (number, scale) = match limit.as_bytes().last() {
            Some(b's') => (&limit[..limit.len() - 1], 1000.0),
            Some(b'm') => (&limit[..limit.len() - 1], 60_000.0),
            Some(b'h') => (&limit[..limit.len() - 1], 3_600_000.0),
            _ => (limit.as_str(), 1000.0),
        };
        let Ok(seconds) = number.parse::<f64>() else {
            self.error(io, &format!("timeout: invalid time interval '{limit}'"));
            return 125;
        };
        if command.is_empty() {
            self.error(io, "timeout: missing command");
            return 125;
        }
        if seconds <= 0.0 {
            return self.dispatch(command, io, host, false, true);
        }

        let now = self.system.uptime_ms();
        let saved = self.deadline;
        let limit = now.saturating_add((seconds * scale) as u64);
        self.deadline = Some(saved.map_or(limit, |outer| outer.min(limit)));
        let timed_out_before = core::mem::take(&mut self.timed_out);

        let status = self.dispatch(command, io, host, false, true);

        self.deadline = saved;
        if core::mem::replace(&mut self.timed_out, timed_out_before) {
            self.cancel.clear();
            if self.flow == Flow::Interrupted {
                self.flow = Flow::None;
            }
            return 124;
        }
        status
    }

    fn builtin_read(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let mut raw = false;
        let mut prompt: Option<String> = None;
        let mut names: Vec<String> = Vec::new();
        let mut limit: Option<usize> = None;
        let mut delimiter = b'\n';
        let mut index = 0usize;

        while index < args.len() {
            let arg = args[index].as_str();
            if names.is_empty() && arg.len() > 1 && arg.starts_with('-') {
                let flags = arg[1..].char_indices();
                for (at, flag) in flags {
                    match flag {
                        'r' | 's' => raw = raw || flag == 'r',
                        'p' | 'n' | 'N' | 'd' | 't' => {
                            let attached = &arg[1 + at + 1..];
                            let value = if attached.is_empty() {
                                index += 1;
                                args.get(index).cloned().unwrap_or_default()
                            } else {
                                attached.to_string()
                            };
                            match flag {
                                'p' => prompt = Some(value),
                                'n' | 'N' => limit = value.parse().ok(),
                                'd' => delimiter = value.bytes().next().unwrap_or(0),
                                _ => {}
                            }
                            break;
                        }
                        other => {
                            self.error(io, &format!("read: -{other}: invalid option"));
                            return 2;
                        }
                    }
                }
            } else if !valid_name(arg) {
                self.error(io, &format!("read: `{arg}': not a valid identifier"));
                return 2;
            } else {
                names.push(arg.to_string());
            }
            index += 1;
        }
        let reply = names.is_empty();
        if reply {
            names.push(String::from("REPLY"));
        }
        if let Some(prompt) = prompt {
            self.write_err(io, &prompt);
        }

        // Read byte by byte: whatever follows the line belongs to the next
        // reader of this descriptor.
        let mut line: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        let mut hit_delimiter = false;
        let mut pending_escape = false;
        loop {
            if limit.is_some_and(|limit| line.len() >= limit) {
                hit_delimiter = true;
                break;
            }
            if self.read_fd(io, 0, &mut byte) == 0 {
                break;
            }
            if pending_escape {
                pending_escape = false;
                // A backslash-newline continues the line.
                if byte[0] != b'\n' {
                    line.push(0x01);
                    line.push(byte[0]);
                }
                continue;
            }
            if !raw && byte[0] == b'\\' {
                pending_escape = true;
                continue;
            }
            if byte[0] == delimiter {
                hit_delimiter = true;
                break;
            }
            if line.len() >= MAX_STREAM_BYTES || line.try_reserve(1).is_err() {
                break;
            }
            line.push(byte[0]);
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }

        let ifs = self.env.get("IFS").unwrap_or(" \t\n").to_string();
        let fields = if reply {
            // `read` with no name keeps the line exactly as it was.
            alloc::vec![unmark_escapes(&line)]
        } else {
            split_read_fields(&line, &ifs, names.len())
        };
        let mut status = i32::from(!hit_delimiter);
        for (index, name) in names.iter().enumerate() {
            let value = fields.get(index).map_or("", String::as_str);
            if let Err(error) = self.env.try_set(name, value) {
                self.error(io, &format!("{name}: {}", error.text()));
                status = 2;
            }
        }
        status
    }

    fn builtin_getopts(&mut self, args: &[String], io: &mut Io<'_>) -> i32 {
        let Some((spec, rest)) = args.split_first() else {
            self.error(io, "getopts: usage: getopts optstring name [arg...]");
            return 2;
        };
        let Some((name, explicit)) = rest.split_first() else {
            self.error(io, "getopts: usage: getopts optstring name [arg...]");
            return 2;
        };
        let values = if explicit.is_empty() {
            self.params.clone()
        } else {
            explicit.to_vec()
        };
        let silent = spec.starts_with(':');
        let spec_body = spec.strip_prefix(':').unwrap_or(spec);

        let mut optind: usize = self
            .env
            .get("OPTIND")
            .and_then(|text| text.parse().ok())
            .unwrap_or(1)
            .max(1);
        // A script resets OPTIND to rescan.
        if optind != self.getopts_index {
            self.getopts_offset = 0;
        }

        let finish = |shell: &mut Self, optind: usize, option: &str| {
            shell.getopts_index = optind;
            shell.env.set("OPTIND", &optind.to_string());
            shell.env.set(name, option);
        };

        let Some(arg) = values.get(optind - 1) else {
            self.env.unset("OPTARG");
            finish(self, optind, "?");
            return 1;
        };
        if self.getopts_offset == 0 {
            if arg == "--" {
                self.env.unset("OPTARG");
                finish(self, optind + 1, "?");
                return 1;
            }
            if arg.len() < 2 || !arg.starts_with('-') {
                self.env.unset("OPTARG");
                finish(self, optind, "?");
                return 1;
            }
            self.getopts_offset = 1;
        }

        let body = arg.get(self.getopts_offset..).unwrap_or("");
        let Some(flag) = body.chars().next() else {
            self.getopts_offset = 0;
            finish(self, optind + 1, "?");
            return 1;
        };
        let after = self.getopts_offset + flag.len_utf8();
        let at_end = after >= arg.len();

        let position = spec_body
            .char_indices()
            .find(|(_, known)| *known == flag && *known != ':')
            .map(|(at, _)| at);
        let Some(position) = position else {
            if at_end {
                self.getopts_offset = 0;
                optind += 1;
            } else {
                self.getopts_offset = after;
            }
            if silent {
                self.env.set("OPTARG", &flag.to_string());
            } else {
                self.env.unset("OPTARG");
                self.write_err(io, &format!("sh: illegal option -- {flag}\n"));
            }
            finish(self, optind, "?");
            return 0;
        };

        let takes_value = spec_body[position + flag.len_utf8()..].starts_with(':');
        if !takes_value {
            self.env.unset("OPTARG");
            if at_end {
                self.getopts_offset = 0;
                optind += 1;
            } else {
                self.getopts_offset = after;
            }
            finish(self, optind, &flag.to_string());
            return 0;
        }

        self.getopts_offset = 0;
        if !at_end {
            let value = arg[after..].to_string();
            self.env.set("OPTARG", &value);
            finish(self, optind + 1, &flag.to_string());
            return 0;
        }
        match values.get(optind) {
            Some(value) => {
                let value = value.clone();
                self.env.set("OPTARG", &value);
                finish(self, optind + 2, &flag.to_string());
            }
            None => {
                if silent {
                    self.env.set("OPTARG", &flag.to_string());
                    finish(self, optind + 1, ":");
                } else {
                    self.env.unset("OPTARG");
                    self.write_err(io, &format!("sh: option requires an argument -- {flag}\n"));
                    finish(self, optind + 1, "?");
                }
            }
        }
        0
    }

    fn builtin_sh(&mut self, args: &[String], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let mut options: Vec<(char, bool)> = Vec::new();
        let mut long_options: Vec<(String, bool)> = Vec::new();
        let mut command: Option<bool> = None;
        let mut from_stdin = false;
        let mut index = 0usize;

        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index += 1;
                break;
            }
            let (enabled, flags) = match arg.as_bytes().first() {
                Some(b'-') if arg.len() > 1 => (true, &arg[1..]),
                Some(b'+') if arg.len() > 1 => (false, &arg[1..]),
                _ => break,
            };
            for flag in flags.chars() {
                match flag {
                    'c' => command = Some(true),
                    's' => from_stdin = true,
                    'i' | 'l' => {}
                    'o' => {
                        index += 1;
                        match args.get(index) {
                            Some(name) => long_options.push((name.clone(), enabled)),
                            None => {
                                self.error(io, "sh: -o: option name required");
                                return 2;
                            }
                        }
                    }
                    other => options.push((other, enabled)),
                }
            }
            index += 1;
        }
        let rest = args.get(index..).unwrap_or(&[]);

        let (text, arg0, params): (String, String, Vec<String>) = if command.is_some() {
            let Some((script, more)) = rest.split_first() else {
                self.error(io, "sh: -c: option requires an argument");
                return 2;
            };
            let (arg0, params) = match more.split_first() {
                Some((name, params)) => (name.clone(), params.to_vec()),
                None => (String::from("sh"), Vec::new()),
            };
            (script.clone(), arg0, params)
        } else if let (Some((path, params)), false) = (rest.split_first(), from_stdin) {
            match self.fs.read_file(path) {
                Ok(data) => (
                    String::from_utf8_lossy(&data).into_owned(),
                    path.clone(),
                    params.to_vec(),
                ),
                Err(error) => {
                    self.error(io, &format!("{path}: {}", error.text()));
                    return if error == FsError::NotFound { 127 } else { 126 };
                }
            }
        } else if !from_stdin && self.fd_is_terminal(io, 0) {
            // A new prompt on the same terminal.
            return self.in_subshell(io, host, |shell, io, host| {
                shell.interactive = true;
                let status = crate::interactive(shell, host, io);
                shell.exit_requested = None;
                status
            });
        } else {
            // `echo 'echo hi' | sh` runs what it is given.
            let data = match self.read_all_fd(io, 0, MAX_STREAM_BYTES) {
                Ok(data) => data,
                Err(error) => {
                    self.error(io, error.text());
                    return 1;
                }
            };
            (
                String::from_utf8_lossy(&data).into_owned(),
                String::from("sh"),
                rest.to_vec(),
            )
        };

        self.in_subshell(io, host, |shell, io, host| {
            shell.env.retain_exported();
            shell.functions.clear();
            shell.opts = crate::Options::default();
            shell.function_depth = 0;
            shell.source_depth = 0;
            shell.errexit_off = 0;
            shell.interactive = false;
            shell.arg0 = arg0;
            shell.params = params;
            for (flag, enabled) in options {
                if !shell.set_short_option(flag, enabled) {
                    shell.error(io, &format!("-{flag}: invalid option"));
                    return 2;
                }
            }
            for (name, enabled) in long_options {
                if !shell.set_long_option(&name, enabled) {
                    shell.error(io, &format!("{name}: invalid option name"));
                    return 2;
                }
            }
            shell.run_child_body(&text, io, host)
        })
    }
}

/// `read` marks backslash-escaped bytes with 0x01 so they do not split.
fn unmark_escapes(line: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(line.len());
    let mut iter = line.iter();
    while let Some(byte) = iter.next() {
        if *byte == 0x01 {
            if let Some(next) = iter.next() {
                bytes.push(*next);
            }
        } else {
            bytes.push(*byte);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Splits a `read` line on `IFS`. The last name takes the rest of the line, with
/// its separators but without the white space around it.
fn split_read_fields(line: &[u8], ifs: &str, count: usize) -> Vec<String> {
    let in_ifs = |byte: u8| byte != 0x01 && byte < 0x80 && ifs.contains(byte as char);
    let is_white = |byte: u8| in_ifs(byte) && matches!(byte, b' ' | b'\t' | b'\n');

    let mut fields = Vec::new();
    let mut index = 0usize;
    while index < line.len() && is_white(line[index]) {
        index += 1;
    }
    while index < line.len() {
        if fields.len() + 1 == count {
            let mut end = line.len();
            while end > index && is_white(line[end - 1]) {
                end -= 1;
            }
            fields.push(unmark_escapes(&line[index..end]));
            return fields;
        }
        let start = index;
        while index < line.len() && !in_ifs(line[index]) {
            if line[index] == 0x01 {
                index += 1;
            }
            index += 1;
        }
        let end = index.min(line.len());
        fields.push(unmark_escapes(&line[start..end]));
        index = end;
        // One separator: white space with at most one non-white delimiter.
        while index < line.len() && is_white(line[index]) {
            index += 1;
        }
        if index < line.len() && in_ifs(line[index]) {
            index += 1;
            while index < line.len() && is_white(line[index]) {
                index += 1;
            }
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_values_for_reuse() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn splits_read_fields_on_ifs() {
        assert_eq!(split_read_fields(b"  a b  c ", " \t\n", 2), ["a", "b  c"]);
        assert_eq!(split_read_fields(b"a,b,,c", ",", 4), ["a", "b", "", "c"]);
        assert_eq!(split_read_fields(b"a b", " ", 5), ["a", "b"]);
        assert_eq!(split_read_fields(b"a\x01 b c", " ", 2), ["a b", "c"]);
    }
}
