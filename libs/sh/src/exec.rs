//! Script execution: pipelines, redirection and control flow.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use feos_coreutils::glob::glob_match;
use feos_coreutils::regex::{Regex, Syntax};
use feos_coreutils::{is_command, Ctx, FsError, Kind, Vfs};

use crate::fds::{FdInput, FdOutput, Leftovers, OpenFile, SavedFds, StreamView};
use crate::lexer::{Assignment, RedirOp, Word};
use crate::parser::{
    ArithForCommand, CaseCommand, CaseFlow, Command, CondExpr, ForCommand, IfCommand, List, Logic,
    Parser, Pipeline, Redirection, Script, SimpleCommand, WhileCommand,
};
use crate::{Flow, Host, Io, Shell, Snapshot, MAX_DEPTH};

/// Roughly how much stack one nested compound command needs while parsing.
const PARSE_STACK_PER_LEVEL: usize = if cfg!(debug_assertions) { 4096 } else { 1536 };

/// Words that take assignments as arguments, so `local x=$y` does not split.
fn is_declaration(word: &str) -> bool {
    matches!(
        word,
        "export" | "readonly" | "local" | "declare" | "typeset"
    )
}

/// Reads the address of a local to estimate how much stack is in use.
#[inline(never)]
fn stack_position() -> usize {
    let marker = 0u8;
    core::ptr::addr_of!(marker) as usize
}

impl Shell<'_> {
    /// Marks where the stack starts at the outermost entry into the shell.
    pub(crate) fn note_stack_base(&mut self) -> bool {
        if self.stack_base.is_none() {
            self.stack_base = Some(stack_position());
            return true;
        }
        false
    }

    fn stack_used(&self) -> usize {
        self.stack_base
            .map_or(0, |base| base.abs_diff(stack_position()))
    }

    /// Enters a nested run of the interpreter, counting depth and checking the
    /// stack. On failure the whole line unwinds, so a function that calls itself
    /// twice cannot spend exponential time failing.
    pub(crate) fn enter_nested(&mut self, io: &mut Io<'_>) -> bool {
        if self.depth >= MAX_DEPTH || self.stack_used() > self.stack_limit {
            self.write_err(io, "sh: maximum nesting depth exceeded\n");
            self.last_status = 2;
            self.flow = Flow::Abort(2);
            return false;
        }
        self.depth += 1;
        true
    }

    pub(crate) fn leave_nested(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Polls for cancellation. Long-running paths call this so Ctrl-C and
    /// `timeout` can stop them.
    pub(crate) fn checkpoint(&mut self, host: &mut dyn Host) -> bool {
        if host.poll_cancel() {
            self.cancel.cancel();
        }
        if let Some(deadline) = self.deadline {
            if self.system.uptime_ms() >= deadline {
                self.timed_out = true;
                self.cancel.cancel();
            }
        }
        if self.cancel.is_cancelled() {
            self.flow = Flow::Interrupted;
            return true;
        }
        false
    }

    /// Writes bytes to a descriptor on behalf of the shell itself.
    pub(crate) fn write_fd(&mut self, io: &mut Io<'_>, fd: usize, bytes: &[u8]) {
        let mut view = StreamView {
            fds: &mut self.fds,
            io,
            cancel: &self.cancel,
            host: None,
            system: self.system,
            deadline: None,
            timed_out: &mut self.timed_out,
            calls: 1,
        };
        view.write(fd, bytes);
    }

    pub(crate) fn write_out(&mut self, io: &mut Io<'_>, text: &str) {
        self.write_fd(io, 1, text.as_bytes());
    }

    pub(crate) fn write_err(&mut self, io: &mut Io<'_>, text: &str) {
        self.write_fd(io, 2, text.as_bytes());
    }

    pub(crate) fn error(&mut self, io: &mut Io<'_>, message: &str) {
        self.write_err(io, &format!("sh: {message}\n"));
    }

    pub(crate) fn fd_is_terminal(&mut self, io: &mut Io<'_>, fd: usize) -> bool {
        let mut view = StreamView {
            fds: &mut self.fds,
            io,
            cancel: &self.cancel,
            host: None,
            system: self.system,
            deadline: None,
            timed_out: &mut self.timed_out,
            calls: 1,
        };
        view.is_terminal(fd)
    }

    /// Reads from a descriptor on behalf of the shell itself.
    pub(crate) fn read_fd(&mut self, io: &mut Io<'_>, fd: usize, out: &mut [u8]) -> usize {
        let mut view = StreamView {
            fds: &mut self.fds,
            io,
            cancel: &self.cancel,
            host: None,
            system: self.system,
            deadline: None,
            timed_out: &mut self.timed_out,
            calls: 1,
        };
        view.read(fd, out)
    }

    pub(crate) fn read_all_fd(
        &mut self,
        io: &mut Io<'_>,
        fd: usize,
        limit: usize,
    ) -> Result<Vec<u8>, FsError> {
        let mut view = StreamView {
            fds: &mut self.fds,
            io,
            cancel: &self.cancel,
            host: None,
            system: self.system,
            deadline: None,
            timed_out: &mut self.timed_out,
            calls: 1,
        };
        view.read_all(fd, limit)
    }

    /// Parses and runs `text` one command at a time, keeping any control flow
    /// for the caller to deal with.
    pub(crate) fn run_text_inner(
        &mut self,
        text: &str,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let mut parser = Parser::new(text);
        let room = self.stack_limit.saturating_sub(self.stack_used());
        parser.set_max_depth(room / PARSE_STACK_PER_LEVEL);
        let mut status = 0;
        loop {
            match parser.next_list(Some(&self.aliases)) {
                Ok(Some(list)) => {
                    status = self.run_list(&list, io, host);
                    if self.flow.is_active() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    self.error(io, &format!("syntax error: {}", error.message));
                    status = 2;
                    self.last_status = 2;
                    break;
                }
            }
        }
        status
    }

    pub(crate) fn run_script(
        &mut self,
        script: &Script,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let mut status = 0;
        for list in &script.lists {
            status = self.run_list(list, io, host);
            if self.flow.is_active() {
                break;
            }
        }
        status
    }

    fn run_list(&mut self, list: &List, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        // Only the last command of an `&&`/`||` chain can trigger `set -e`.
        let count = list.rest.len();
        if count > 0 {
            self.errexit_off += 1;
        }
        let mut status = self.run_pipeline(&list.first, io, host, list.background);
        if count > 0 {
            self.errexit_off -= 1;
        }
        self.last_status = status;
        let mut last = (count == 0).then_some(&list.first);

        for (index, (logic, pipeline)) in list.rest.iter().enumerate() {
            if self.flow.is_active() {
                return status;
            }
            let should_run = match logic {
                Logic::And => status == 0,
                Logic::Or => status != 0,
            };
            if should_run {
                let is_last = index + 1 == count;
                if !is_last {
                    self.errexit_off += 1;
                }
                status = self.run_pipeline(pipeline, io, host, list.background);
                if !is_last {
                    self.errexit_off -= 1;
                }
                self.last_status = status;
                if is_last {
                    last = Some(pipeline);
                }
            }
        }

        if status != 0
            && self.opts.errexit
            && self.errexit_off == 0
            && !self.flow.is_active()
            && last.is_some_and(triggers_errexit)
        {
            self.flow = Flow::Exit(status);
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
        if pipeline.negate {
            self.errexit_off += 1;
        }
        let mut status = match pipeline.commands.as_slice() {
            [command] => self.run_command(command, io, host, background),
            commands => self.run_stages(commands, io, host),
        };
        if pipeline.negate {
            self.errexit_off -= 1;
            if !self.flow.is_active() {
                status = i32::from(status == 0);
            }
        }
        status
    }

    /// Runs a pipeline: each stage's output is buffered and becomes the next
    /// stage's input.
    fn run_stages(&mut self, commands: &[Command], io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let count = commands.len();
        let mut previous: Option<Vec<u8>> = None;
        let mut status = 0;
        let mut failed = 0;
        let mut leftovers = Vec::new();

        for (index, command) in commands.iter().enumerate() {
            let saved = self.fds.save();
            if let Some(data) = previous.take() {
                let reader = self.fds.open(OpenFile::Reader { data, position: 0 });
                self.fds.bind(0, reader, &mut leftovers);
            }
            let capture = (index + 1 < count).then(|| {
                let capture = self.fds.open(OpenFile::Capture {
                    data: Vec::new(),
                    overflow: false,
                });
                self.fds.hold(capture);
                self.fds.bind(1, capture, &mut leftovers);
                capture
            });

            // Each stage is its own subshell, so `cd` or `x=1` in it stays there.
            // A plain coreutil cannot change shell state, so it skips the copy.
            status = if self.stage_needs_isolation(command) {
                self.in_subshell(io, host, |shell, io, host| {
                    shell.run_command(command, io, host, false)
                })
            } else {
                self.run_command(command, io, host, false)
            };

            self.finish_redirects(saved, io);
            self.flush_leftovers(core::mem::take(&mut leftovers), io);
            if let Some(capture) = capture {
                let (data, overflow) = self.fds.take_capture(capture);
                if overflow {
                    self.error(io, "pipeline output is too large");
                    return 1;
                }
                previous = Some(data);
            }
            if status != 0 {
                failed = status;
            }
            if matches!(self.flow, Flow::Interrupted | Flow::Abort(_) | Flow::Exit(_)) {
                return status;
            }
        }

        if self.opts.pipefail {
            failed
        } else {
            status
        }
    }

    fn stage_needs_isolation(&self, command: &Command) -> bool {
        let Command::Simple(simple) = command else {
            return true;
        };
        if !simple.assignments.is_empty() {
            return true;
        }
        let Some(name) = simple.words.first().and_then(Word::plain) else {
            return true;
        };
        !is_command(name)
            || self.functions.contains_key(name)
            || crate::builtin_names().contains(&name)
    }

    /// Runs `body` in a copy of the shell's state and restores it afterward.
    pub(crate) fn in_subshell<F>(&mut self, io: &mut Io<'_>, host: &mut dyn Host, body: F) -> i32
    where
        F: FnOnce(&mut Self, &mut Io<'_>, &mut dyn Host) -> i32,
    {
        if !self.enter_nested(io) {
            return 2;
        }
        let snapshot = self.snapshot();
        let saved = self.fds.save();
        let status = body(self, io, host);
        self.finish_redirects(saved, io);
        let flow = core::mem::replace(&mut self.flow, Flow::None);
        self.restore_snapshot(snapshot);
        self.leave_nested();
        match flow {
            Flow::Exit(code) | Flow::Return(code) => code,
            Flow::None | Flow::Break(_) | Flow::Continue(_) => status,
            // These unwind the whole line, not just the subshell.
            Flow::Interrupted => {
                self.flow = Flow::Interrupted;
                130
            }
            Flow::Abort(code) => {
                self.flow = Flow::Abort(code);
                code
            }
        }
    }

    /// Copies everything a subshell may change. The filesystem is deliberately
    /// shared, as it is between real processes.
    pub(crate) fn snapshot(&mut self) -> Snapshot {
        let snapshot = Snapshot {
            env: self.env.clone(),
            cwd: self.fs.cwd().to_string(),
            aliases: self.aliases.clone(),
            functions: self.functions.clone(),
            params: self.params.clone(),
            arg0: self.arg0.clone(),
            opts: self.opts,
            locals: core::mem::take(&mut self.locals),
            loop_depth: self.loop_depth,
            function_depth: self.function_depth,
            source_depth: self.source_depth,
            errexit_off: self.errexit_off,
            interactive: self.interactive,
            getopts_offset: self.getopts_offset,
        };
        self.loop_depth = 0;
        snapshot
    }

    pub(crate) fn restore_snapshot(&mut self, snapshot: Snapshot) {
        self.env = snapshot.env;
        self.aliases = snapshot.aliases;
        self.functions = snapshot.functions;
        self.params = snapshot.params;
        self.arg0 = snapshot.arg0;
        self.opts = snapshot.opts;
        self.locals = snapshot.locals;
        self.loop_depth = snapshot.loop_depth;
        self.function_depth = snapshot.function_depth;
        self.source_depth = snapshot.source_depth;
        self.errexit_off = snapshot.errexit_off;
        self.interactive = snapshot.interactive;
        self.getopts_offset = snapshot.getopts_offset;
        // The subshell may have removed the directory it started in.
        let mut target = snapshot.cwd;
        while self.fs.chdir(&target).is_err() {
            if target == "/" {
                break;
            }
            target = feos_coreutils::parent(&target).to_string();
        }
    }

    pub(crate) fn run_command(
        &mut self,
        command: &Command,
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        if self.stack_used() > self.stack_limit {
            self.enter_nested(io);
            return 2;
        }
        match command {
            Command::Simple(simple) => self.run_simple(simple, io, host, background),
            Command::If(command) => self.run_if(command, io, host),
            Command::For(command) => self.run_for(command, io, host),
            Command::ArithFor(command) => self.run_arith_for(command, io, host),
            Command::While(command) => self.run_while(command, io, host),
            Command::Case(command) => self.run_case(command, io, host),
            Command::Group(script) => self.run_script(script, io, host),
            Command::Subshell(script) => {
                self.in_subshell(io, host, |shell, io, host| shell.run_script(script, io, host))
            }
            Command::Arith(expression) => match self.eval_arith_text(expression, io, host) {
                Ok(value) => i32::from(value == 0),
                Err(_) => 1,
            },
            Command::Cond(expression) => match self.eval_cond(expression, io, host) {
                Ok(true) => 0,
                Ok(false) => 1,
                Err(status) => status,
            },
            Command::Function(def) => {
                self.functions.insert(def.name.clone(), def.body.clone());
                0
            }
            Command::Redirected(inner, redirs) => {
                let saved = self.fds.save();
                let status = match self.apply_redirections(redirs, io, host) {
                    Ok(()) => self.run_command(inner, io, host, background),
                    Err(status) => status,
                };
                let flushed = self.finish_redirects(saved, io);
                if flushed || status != 0 {
                    status
                } else {
                    1
                }
            }
        }
    }

    /// Runs a condition, during which `set -e` is off.
    fn run_condition(&mut self, script: &Script, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        self.errexit_off += 1;
        let status = self.run_script(script, io, host);
        self.errexit_off -= 1;
        status
    }

    fn run_if(&mut self, command: &IfCommand, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        for (condition, body) in &command.branches {
            let status = self.run_condition(condition, io, host);
            if self.flow.is_active() {
                return status;
            }
            if status == 0 {
                return self.run_script(body, io, host);
            }
        }
        match &command.else_body {
            Some(body) => self.run_script(body, io, host),
            None => 0,
        }
    }

    /// Handles `break` and `continue` after a loop body. Returns true when the
    /// loop must end.
    fn loop_should_end(&mut self) -> bool {
        match self.flow {
            Flow::None => false,
            Flow::Break(levels) => {
                self.flow = if levels > 1 {
                    Flow::Break(levels - 1)
                } else {
                    Flow::None
                };
                true
            }
            Flow::Continue(levels) => {
                if levels > 1 {
                    self.flow = Flow::Continue(levels - 1);
                    true
                } else {
                    self.flow = Flow::None;
                    false
                }
            }
            _ => true,
        }
    }

    fn run_for(&mut self, command: &ForCommand, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let values = match &command.words {
            Some(words) => {
                let mut values = Vec::new();
                for word in words {
                    if self.expand_word_into(word, &mut values, io, host).is_err() {
                        return self.last_status.max(1);
                    }
                }
                values
            }
            None => self.params.clone(),
        };
        let mut status = 0;
        self.loop_depth += 1;
        for value in values {
            if self.checkpoint(host) {
                status = 130;
                break;
            }
            if let Err(error) = self.env.try_set(&command.variable, &value) {
                self.error(io, &format!("{}: {}", command.variable, error.text()));
                status = 1;
                break;
            }
            status = self.run_script(&command.body, io, host);
            if self.loop_should_end() {
                break;
            }
        }
        self.loop_depth -= 1;
        status
    }

    fn run_arith_for(
        &mut self,
        command: &ArithForCommand,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        if self.eval_arith_text(&command.init, io, host).is_err() {
            return 1;
        }
        let mut status = 0;
        self.loop_depth += 1;
        loop {
            if self.checkpoint(host) {
                status = 130;
                break;
            }
            if !command.test.trim().is_empty() {
                match self.eval_arith_text(&command.test, io, host) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => {
                        status = 1;
                        break;
                    }
                }
            }
            status = self.run_script(&command.body, io, host);
            if self.loop_should_end() {
                break;
            }
            if self.eval_arith_text(&command.step, io, host).is_err() {
                status = 1;
                break;
            }
        }
        self.loop_depth -= 1;
        status
    }

    fn run_while(&mut self, command: &WhileCommand, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let mut status = 0;
        self.loop_depth += 1;

        loop {
            if self.checkpoint(host) {
                status = 130;
                break;
            }
            let condition = self.run_condition(&command.condition, io, host);
            if self.flow.is_active() {
                if self.loop_should_end() {
                    break;
                }
                continue;
            }
            let keep_going = if command.until {
                condition != 0
            } else {
                condition == 0
            };
            if !keep_going {
                break;
            }

            status = self.run_script(&command.body, io, host);
            if self.loop_should_end() {
                break;
            }
        }

        self.loop_depth -= 1;
        status
    }

    fn run_case(&mut self, command: &CaseCommand, io: &mut Io<'_>, host: &mut dyn Host) -> i32 {
        let Ok(subject) = self.expand_assign(&command.word, io, host) else {
            return self.last_status.max(1);
        };
        let mut status = 0;
        let mut falling = false;
        for item in &command.items {
            let mut matched = falling;
            if !matched {
                for pattern in &item.patterns {
                    let Ok(pattern) = self.expand_pattern(pattern, io, host) else {
                        return self.last_status.max(1);
                    };
                    if glob_match(&pattern, &subject) {
                        matched = true;
                        break;
                    }
                }
            }
            if !matched {
                continue;
            }
            status = self.run_script(&item.body, io, host);
            if self.flow.is_active() {
                return status;
            }
            match item.flow {
                CaseFlow::End => return status,
                CaseFlow::FallThrough => falling = true,
                CaseFlow::Continue => falling = false,
            }
        }
        status
    }

    fn eval_cond(
        &mut self,
        expression: &CondExpr,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Result<bool, i32> {
        let fail = |shell: &Self| shell.last_status.max(1);
        match expression {
            CondExpr::Word(word) => {
                let text = self.expand_assign(word, io, host).map_err(|_| fail(self))?;
                Ok(!text.is_empty())
            }
            CondExpr::Not(inner) => Ok(!self.eval_cond(inner, io, host)?),
            CondExpr::And(left, right) => {
                Ok(self.eval_cond(left, io, host)? && self.eval_cond(right, io, host)?)
            }
            CondExpr::Or(left, right) => {
                Ok(self.eval_cond(left, io, host)? || self.eval_cond(right, io, host)?)
            }
            CondExpr::Unary(operator, word) => {
                let text = self.expand_assign(word, io, host).map_err(|_| fail(self))?;
                Ok(match operator.as_str() {
                    "-z" => text.is_empty(),
                    "-n" => !text.is_empty(),
                    "-v" => self.env.get(&text).is_some(),
                    "-L" | "-h" => false,
                    "-f" => self.fs.meta(&text).is_ok_and(|meta| meta.kind == Kind::File),
                    "-d" => self.fs.meta(&text).is_ok_and(|meta| meta.kind == Kind::Dir),
                    "-s" => self
                        .fs
                        .meta(&text)
                        .is_ok_and(|meta| meta.size > 0 || meta.kind == Kind::Dir),
                    _ => self.fs.meta(&text).is_ok(),
                })
            }
            CondExpr::Binary(left, operator, right) => {
                let left = self.expand_assign(left, io, host).map_err(|_| fail(self))?;
                match operator.as_str() {
                    "=" | "==" | "!=" => {
                        let pattern = self.expand_pattern(right, io, host).map_err(|_| fail(self))?;
                        let matched = glob_match(&pattern, &left);
                        Ok(matched == (operator != "!="))
                    }
                    "=~" => {
                        let pattern = self.expand_assign(right, io, host).map_err(|_| fail(self))?;
                        match Regex::new(&pattern, Syntax::Extended) {
                            Ok(regex) => Ok(regex.is_match(left.as_bytes())),
                            Err(message) => {
                                self.error(io, &format!("=~: {message}"));
                                Err(2)
                            }
                        }
                    }
                    "<" | ">" => {
                        let right = self.expand_assign(right, io, host).map_err(|_| fail(self))?;
                        Ok(if operator == "<" { left < right } else { left > right })
                    }
                    _ => {
                        let right = self.expand_assign(right, io, host).map_err(|_| fail(self))?;
                        let left = self.eval_arith_text(&left, io, host).map_err(|_| 1)?;
                        let right = self.eval_arith_text(&right, io, host).map_err(|_| 1)?;
                        Ok(match operator.as_str() {
                            "-eq" => left == right,
                            "-ne" => left != right,
                            "-lt" => left < right,
                            "-le" => left <= right,
                            "-gt" => left > right,
                            _ => left >= right,
                        })
                    }
                }
            }
        }
    }

    /// Applies one `NAME=value` to the shell.
    fn assign(
        &mut self,
        assignment: &Assignment,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Result<(), i32> {
        let value = self
            .expand_assign(&assignment.value, io, host)
            .map_err(|_| self.last_status.max(1))?;
        let result = if assignment.append {
            let mut combined = self.env.get(&assignment.name).unwrap_or("").to_string();
            combined.push_str(&value);
            self.env.try_set(&assignment.name, &combined)
        } else {
            self.env.try_set(&assignment.name, &value)
        };
        result.map_err(|error| {
            self.error(io, &format!("{}: {}", assignment.name, error.text()));
            1
        })
    }

    fn run_simple(
        &mut self,
        command: &SimpleCommand,
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        if self.checkpoint(host) {
            return 130;
        }
        self.subst_status = None;

        // Words expand first, so `FOO=1 echo $FOO` sees the old `FOO`.
        let declaration = command
            .words
            .first()
            .and_then(Word::plain)
            .is_some_and(is_declaration);
        let mut argv: Vec<String> = Vec::new();
        for (index, word) in command.words.iter().enumerate() {
            let split = if declaration && index > 0 {
                word.split_assignment()
            } else {
                None
            };
            let expanded = match split {
                Some(assignment) => self.expand_assign(&assignment.value, io, host).map(|value| {
                    let operator = if assignment.append { "+=" } else { "=" };
                    argv.push(format!("{}{operator}{value}", assignment.name));
                }),
                None => self.expand_word_into(word, &mut argv, io, host),
            };
            if expanded.is_err() {
                return self.last_status.max(1);
            }
        }

        let saved_fds = self.fds.save();
        if let Err(status) = self.apply_redirections(&command.redirs, io, host) {
            self.finish_redirects(saved_fds, io);
            return status;
        }

        let status = if argv.is_empty() {
            // A bare assignment persists, and its status is that of the last
            // command substitution in it.
            let mut status = 0;
            for assignment in &command.assignments {
                if let Err(code) = self.assign(assignment, io, host) {
                    status = code;
                    break;
                }
            }
            if status == 0 {
                self.subst_status.unwrap_or(0)
            } else {
                status
            }
        } else {
            self.run_with_temp_vars(command, &argv, io, host, background)
        };

        let flushed = if core::mem::take(&mut self.keep_redirects) {
            self.discard_saved(saved_fds, io)
        } else {
            self.finish_redirects(saved_fds, io)
        };
        if flushed || status != 0 {
            status
        } else {
            1
        }
    }

    fn run_with_temp_vars(
        &mut self,
        command: &SimpleCommand,
        argv: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        let mut saved = Vec::new();
        let mut failed = None;
        for assignment in &command.assignments {
            saved.push((assignment.name.clone(), self.env.var(&assignment.name)));
            if let Err(status) = self.assign(assignment, io, host) {
                failed = Some(status);
                break;
            }
            self.env.export(&assignment.name);
        }

        if self.opts.xtrace {
            let prompt = self.env.get("PS4").unwrap_or("+ ").to_string();
            let mut line = prompt;
            for assignment in &command.assignments {
                let value = self.env.get(&assignment.name).unwrap_or("").to_string();
                line.push_str(&format!("{}={value} ", assignment.name));
            }
            line.push_str(&argv.join(" "));
            line.push('\n');
            self.write_err(io, &line);
        }

        let status = match failed {
            Some(status) => status,
            None => self.dispatch(argv, io, host, background, true),
        };

        for (name, var) in saved.into_iter().rev() {
            self.env.restore(&name, var);
        }
        status
    }

    /// Finds a redirection target, which must expand to exactly one word.
    fn redirect_target(
        &mut self,
        redirection: &Redirection,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Result<String, i32> {
        let Some(word) = redirection.target.as_ref() else {
            return Err(2);
        };
        let mut fields = self
            .expand_word(word, io, host)
            .map_err(|_| self.last_status.max(1))?;
        if fields.len() != 1 {
            self.error(io, &format!("{}: ambiguous redirect", word.raw()));
            return Err(1);
        }
        fields.pop().ok_or(1)
    }

    /// Opens `path` for writing, returning the open-file index.
    fn open_output(
        &mut self,
        path: &str,
        append: bool,
        clobber: bool,
        io: &mut Io<'_>,
    ) -> Result<u16, i32> {
        let absolute = self.fs.resolve(path);
        if let Some(index) = self.open_device(&absolute, true) {
            return Ok(index);
        }
        if self.opts.noclobber && !append && !clobber && self.fs.exists(path) {
            self.error(io, &format!("{path}: cannot overwrite existing file"));
            return Err(1);
        }
        if let Err(error) = self.fs.write_file(path, b"", append) {
            self.error(io, &format!("{path}: {}", error.text()));
            return Err(1);
        }
        // The resolved path keeps `cd /docs > log` writing `log` where the
        // redirection was opened, whatever the command does to the cwd.
        Ok(self.fds.open(OpenFile::Writer {
            path: absolute,
            pending: Vec::new(),
            overflow: false,
        }))
    }

    /// `/dev/null` and friends need no file behind them.
    fn open_device(&mut self, absolute: &str, writing: bool) -> Option<u16> {
        let name = absolute.strip_prefix("/dev/")?;
        let file = match name {
            "null" => OpenFile::Null,
            "zero" => {
                if writing {
                    OpenFile::Null
                } else {
                    OpenFile::Zero
                }
            }
            "tty" => OpenFile::Base(u8::from(writing)),
            _ => return None,
        };
        Some(self.fds.open(file))
    }

    /// Maps `/dev/stdout` and `/dev/fd/N` to the descriptor they name.
    fn device_descriptor(absolute: &str) -> Option<usize> {
        let name = absolute.strip_prefix("/dev/")?;
        match name {
            "stdin" => Some(0),
            "stdout" => Some(1),
            "stderr" => Some(2),
            _ => name
                .strip_prefix("fd/")
                .and_then(|number| number.parse::<usize>().ok())
                .filter(|fd| *fd < crate::fds::MAX_FDS),
        }
    }

    fn dup_fd(&mut self, fd: usize, target: usize, io: &mut Io<'_>) -> Result<(), i32> {
        let mut leftovers = Vec::new();
        let done = self.fds.dup(fd, target, &mut leftovers);
        self.flush_leftovers(leftovers, io);
        if done {
            Ok(())
        } else {
            self.error(io, &format!("{target}: Bad file descriptor"));
            Err(1)
        }
    }

    fn bind_fd(&mut self, fd: usize, index: u16, io: &mut Io<'_>) {
        let mut leftovers = Vec::new();
        self.fds.bind(fd, index, &mut leftovers);
        self.flush_leftovers(leftovers, io);
    }

    /// Applies redirections in source order, so `2>&1 > f` differs from
    /// `> f 2>&1`. A failure leaves the caller to undo what was done.
    pub(crate) fn apply_redirections(
        &mut self,
        redirs: &[Redirection],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Result<(), i32> {
        for redirection in redirs {
            match redirection.op {
                RedirOp::Read(fd) | RedirOp::ReadWrite(fd) => {
                    let path = self.redirect_target(redirection, io, host)?;
                    let absolute = self.fs.resolve(&path);
                    if let Some(target) = Self::device_descriptor(&absolute) {
                        self.dup_fd(usize::from(fd), target, io)?;
                        continue;
                    }
                    if let Some(index) = self.open_device(&absolute, false) {
                        self.bind_fd(usize::from(fd), index, io);
                        continue;
                    }
                    if matches!(redirection.op, RedirOp::ReadWrite(_)) && !self.fs.exists(&path) {
                        if let Err(error) = self.fs.write_file(&path, b"", false) {
                            self.error(io, &format!("{path}: {}", error.text()));
                            return Err(1);
                        }
                    }
                    match self.fs.read_file(&path) {
                        Ok(data) => {
                            let index = self.fds.open(OpenFile::Reader { data, position: 0 });
                            self.bind_fd(usize::from(fd), index, io);
                        }
                        Err(error) => {
                            self.error(io, &format!("{path}: {}", error.text()));
                            return Err(1);
                        }
                    }
                }
                RedirOp::Write(fd) | RedirOp::Clobber(fd) | RedirOp::Append(fd) => {
                    let path = self.redirect_target(redirection, io, host)?;
                    let absolute = self.fs.resolve(&path);
                    if let Some(target) = Self::device_descriptor(&absolute) {
                        self.dup_fd(usize::from(fd), target, io)?;
                        continue;
                    }
                    let append = matches!(redirection.op, RedirOp::Append(_));
                    let clobber = matches!(redirection.op, RedirOp::Clobber(_));
                    let index = self.open_output(&path, append, clobber, io)?;
                    self.bind_fd(usize::from(fd), index, io);
                }
                RedirOp::WriteBoth | RedirOp::AppendBoth => {
                    let path = self.redirect_target(redirection, io, host)?;
                    let append = matches!(redirection.op, RedirOp::AppendBoth);
                    let index = self.open_output(&path, append, false, io)?;
                    self.bind_fd(1, index, io);
                    self.dup_fd(2, 1, io)?;
                }
                RedirOp::Dup(fd, target) => {
                    self.dup_fd(usize::from(fd), usize::from(target), io)?;
                }
                RedirOp::DupWord(fd) => {
                    let word = self.redirect_target(redirection, io, host)?;
                    if word == "-" {
                        self.close_fd(usize::from(fd), io);
                    } else if let Some(target) = word
                        .parse::<usize>()
                        .ok()
                        .filter(|target| *target < crate::fds::MAX_FDS)
                    {
                        self.dup_fd(usize::from(fd), target, io)?;
                    } else if fd == 1 {
                        // `>& file` sends both streams to a file.
                        let index = self.open_output(&word, false, false, io)?;
                        self.bind_fd(1, index, io);
                        self.dup_fd(2, 1, io)?;
                    } else {
                        self.error(io, &format!("{word}: ambiguous redirect"));
                        return Err(1);
                    }
                }
                RedirOp::Close(fd) => self.close_fd(usize::from(fd), io),
                RedirOp::HereDoc(fd) => {
                    let Some(word) = redirection.target.as_ref() else {
                        return Err(2);
                    };
                    let body = self
                        .expand_assign(word, io, host)
                        .map_err(|_| self.last_status.max(1))?;
                    let index = self.fds.open(OpenFile::Reader {
                        data: body.into_bytes(),
                        position: 0,
                    });
                    self.bind_fd(usize::from(fd), index, io);
                }
                RedirOp::HereString(fd) => {
                    let Some(word) = redirection.target.as_ref() else {
                        return Err(2);
                    };
                    let mut body = self
                        .expand_assign(word, io, host)
                        .map_err(|_| self.last_status.max(1))?;
                    body.push('\n');
                    let index = self.fds.open(OpenFile::Reader {
                        data: body.into_bytes(),
                        position: 0,
                    });
                    self.bind_fd(usize::from(fd), index, io);
                }
            }
        }
        Ok(())
    }

    fn close_fd(&mut self, fd: usize, io: &mut Io<'_>) {
        let mut leftovers = Vec::new();
        self.fds.close(fd, &mut leftovers);
        self.flush_leftovers(leftovers, io);
    }

    /// Writes out pending file data. Returns false when a write failed.
    pub(crate) fn flush_writers(&mut self, io: &mut Io<'_>) -> bool {
        let mut pending = Vec::new();
        self.fds.take_pending(&mut pending);
        let mut ok = self.flush_leftovers(pending, io);
        while let Some(path) = self.fds.take_overflowed_writer() {
            self.error(io, &format!("{path}: output is too large"));
            ok = false;
        }
        ok
    }

    pub(crate) fn flush_leftovers(&mut self, leftovers: Leftovers, io: &mut Io<'_>) -> bool {
        let mut ok = true;
        for (path, data) in leftovers {
            if let Err(error) = self.fs.write_file(&path, &data, true) {
                self.error(io, &format!("{path}: {}", error.text()));
                ok = false;
            }
        }
        ok
    }

    /// Flushes files and puts the saved descriptors back.
    pub(crate) fn finish_redirects(&mut self, saved: SavedFds, io: &mut Io<'_>) -> bool {
        let mut ok = self.flush_writers(io);
        let mut leftovers = Vec::new();
        self.fds.restore(saved, &mut leftovers);
        ok &= self.flush_leftovers(leftovers, io);
        ok
    }

    /// Flushes files and keeps the current descriptors, for `exec > f`.
    fn discard_saved(&mut self, saved: SavedFds, io: &mut Io<'_>) -> bool {
        let mut ok = self.flush_writers(io);
        let mut leftovers = Vec::new();
        self.fds.discard(saved, &mut leftovers);
        ok &= self.flush_leftovers(leftovers, io);
        ok
    }

    /// Runs a command by name: function, builtin, then external.
    pub(crate) fn dispatch(
        &mut self,
        argv: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
        use_functions: bool,
    ) -> i32 {
        let Some((name, args)) = argv.split_first() else {
            return 0;
        };

        if use_functions {
            if let Some(body) = self.functions.get(name).cloned() {
                return self.run_function(body, args, io, host);
            }
        }
        if let Some(status) = self.run_builtin(name, args, io, host) {
            return status;
        }
        self.run_external(name, args, io, host, background)
    }

    /// Runs a command outside the shell: a coreutil, an app or a script.
    pub(crate) fn run_external(
        &mut self,
        name: &str,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
        background: bool,
    ) -> i32 {
        if name.contains('/') {
            return self.run_script_path(name, args, io, host);
        }
        if is_command(name) {
            return self.run_coreutil(name, args, io, host);
        }

        let attached = self.fd_is_terminal(io, 0) && self.fd_is_terminal(io, 1);
        if !attached && host.has_command(name) {
            self.error(
                io,
                "external commands cannot be used in pipelines or with redirection",
            );
            return 1;
        }
        if attached {
            let result = {
                let view = RefCell::new(StreamView {
                    fds: &mut self.fds,
                    io,
                    cancel: &self.cancel,
                    host: None,
                    system: self.system,
                    deadline: self.deadline,
                    timed_out: &mut self.timed_out,
                    calls: 0,
                });
                let mut stdin = FdInput {
                    view: &view,
                    fd: 0,
                };
                let mut stdout = FdOutput {
                    view: &view,
                    fd: 1,
                };
                let mut stderr = FdOutput {
                    view: &view,
                    fd: 2,
                };
                host.run_external(name, args, &mut stdin, &mut stdout, &mut stderr, background)
            };
            if let Some(status) = result {
                return status;
            }
        }

        if let Some(path) = self.find_in_path(name) {
            return self.run_script_path(&path, args, io, host);
        }
        self.write_err(io, &format!("sh: {name}: command not found\n"));
        127
    }

    fn run_coreutil(
        &mut self,
        name: &str,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let status = {
            let view = RefCell::new(StreamView {
                fds: &mut self.fds,
                io,
                cancel: &self.cancel,
                host: Some(host),
                system: self.system,
                deadline: self.deadline,
                timed_out: &mut self.timed_out,
                calls: 0,
            });
            let mut stdin = FdInput {
                view: &view,
                fd: 0,
            };
            let mut stdout = FdOutput {
                view: &view,
                fd: 1,
            };
            let mut stderr = FdOutput {
                view: &view,
                fd: 2,
            };
            let mut context = Ctx {
                fs: &mut self.fs,
                env: &mut self.env,
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
                system: self.system,
                depth: 0,
            };
            feos_coreutils::run(name, args, &mut context)
        };

        // `rm -r` of the working directory moves it.
        if self.env.get("PWD") != Some(self.fs.cwd()) {
            let cwd = self.fs.cwd().to_string();
            self.env.set("PWD", &cwd);
        }
        if self.cancel.is_cancelled() {
            self.flow = Flow::Interrupted;
            return 130;
        }
        status
    }

    /// Looks a name up in `PATH`, as a script file.
    pub(crate) fn find_in_path(&mut self, name: &str) -> Option<String> {
        let path = self.env.get("PATH")?.to_string();
        for directory in path.split(':') {
            let directory = if directory.is_empty() { "." } else { directory };
            let candidate = feos_coreutils::join(directory, name);
            if self
                .fs
                .meta(&candidate)
                .is_ok_and(|meta| meta.kind == Kind::File)
            {
                return Some(candidate);
            }
        }
        None
    }

    /// Runs a script file in a child-like copy of the shell.
    pub(crate) fn run_script_path(
        &mut self,
        path: &str,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let data = match self.fs.read_file(path) {
            Ok(data) => data,
            Err(FsError::NotFound) => {
                self.error(io, &format!("{path}: No such file or directory"));
                return 127;
            }
            Err(error) => {
                self.error(io, &format!("{path}: {}", error.text()));
                return 126;
            }
        };
        if data.iter().take(512).any(|byte| *byte == 0) {
            self.error(io, &format!("{path}: cannot execute binary file"));
            return 126;
        }
        if let Some(line) = data.strip_prefix(b"#!") {
            let end = line.iter().position(|byte| *byte == b'\n').unwrap_or(line.len());
            let interpreter = String::from_utf8_lossy(&line[..end]);
            let program = interpreter
                .split_whitespace()
                .next()
                .map_or("", |word| word.rsplit('/').next().unwrap_or(word));
            // `#!/usr/bin/env sh` is fine, and only the shell can run a script.
            let second = interpreter.split_whitespace().nth(1).unwrap_or("");
            let wanted = if program == "env" { second } else { program };
            if !matches!(wanted, "sh" | "bash" | "dash" | "ash" | "") {
                self.error(io, &format!("{path}: {wanted}: bad interpreter"));
                return 126;
            }
        }
        let text = String::from_utf8_lossy(&data);
        self.run_child(&text, path, args, io, host)
    }

    /// Runs `text` as `sh` would: only exported variables carry over, functions
    /// do not, and `exit` ends just this script.
    pub(crate) fn run_child(
        &mut self,
        text: &str,
        arg0: &str,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        self.in_subshell(io, host, |shell, io, host| {
            shell.env.retain_exported();
            shell.functions.clear();
            shell.opts = crate::Options::default();
            shell.function_depth = 0;
            shell.source_depth = 0;
            shell.errexit_off = 0;
            shell.interactive = false;
            shell.arg0 = arg0.to_string();
            shell.params = args.to_vec();
            shell.run_child_body(text, io, host)
        })
    }

    pub(crate) fn run_child_body(
        &mut self,
        text: &str,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        let status = self.run_text_inner(text, io, host);
        // A failure in a child stays in the child.
        if let Flow::Abort(code) = self.flow {
            self.flow = Flow::None;
            return code;
        }
        status
    }

    fn run_function(
        &mut self,
        body: Arc<Command>,
        args: &[String],
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> i32 {
        if !self.enter_nested(io) {
            return 2;
        }
        let params = core::mem::replace(&mut self.params, args.to_vec());
        let loop_depth = core::mem::take(&mut self.loop_depth);
        self.function_depth += 1;
        self.locals.push(Vec::new());

        let mut status = self.run_command(&body, io, host, false);
        if let Flow::Return(code) = self.flow {
            self.flow = Flow::None;
            status = code;
        }

        if let Some(frame) = self.locals.pop() {
            for (name, var) in frame.into_iter().rev() {
                self.env.restore(&name, var);
            }
        }
        self.function_depth -= 1;
        self.loop_depth = loop_depth;
        self.params = params;
        self.leave_nested();
        status
    }
}

/// `set -e` looks at simple commands and subshells; a compound command's failure
/// was already judged inside it.
fn triggers_errexit(pipeline: &Pipeline) -> bool {
    if pipeline.negate {
        return false;
    }
    if pipeline.commands.len() > 1 {
        return true;
    }
    let mut command = pipeline.commands.first();
    while let Some(Command::Redirected(inner, _)) = command {
        command = Some(inner);
    }
    matches!(
        command,
        Some(Command::Simple(_) | Command::Subshell(_) | Command::Arith(_) | Command::Cond(_))
    )
}
