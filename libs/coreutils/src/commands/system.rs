//! System, environment and expression utilities.

use alloc::format;
use alloc::string::{String, ToString};

use super::opts::{bad_option, Opt, Opts};
use super::{error, format_size, parse_i64, usage, write_line, Ctx};
use crate::fs::Kind;
use crate::regex::{Regex, Syntax};

/// Deepest `( ( ... ) )` and `! ! ...` nesting `test` and `expr` will parse.
const MAX_EXPR_DEPTH: usize = 32;

pub(super) fn run_true(_args: &[String], _ctx: &mut Ctx<'_>) -> i32 {
    0
}

pub(super) fn run_false(_args: &[String], _ctx: &mut Ctx<'_>) -> i32 {
    1
}

pub(super) fn pwd(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    write_line(ctx.stdout, format_args!("{}", ctx.fs.cwd()));
    0
}

pub(super) fn env(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    // `env NAME=value command` runs the command with changed variables and
    // restores them, since a command here shares the shell's environment.
    let mut opts = Opts::new(args, "i").stop_at_operand();
    let mut clear = false;
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('i') => clear = true,
            other => return bad_option(ctx, "env", other),
        }
    }
    let mut rest = opts.rest();
    let saved = ctx.env.clone();
    if clear {
        *ctx.env = crate::env::Env::new();
    }
    while let Some((name, value)) = rest.first().and_then(|arg| arg.split_once('=')) {
        ctx.env.set(name, value);
        ctx.env.export(name);
        rest = &rest[1..];
    }
    let status = match rest.split_first() {
        Some((command, command_args)) => {
            if super::is_command(command) {
                super::run_nested(command, command_args, ctx)
            } else {
                error(ctx.stderr, command, "command not found");
                127
            }
        }
        None => {
            for entry in ctx.env.entries() {
                write_line(ctx.stdout, format_args!("{entry}"));
            }
            0
        }
    };
    *ctx.env = saved;
    status
}

pub(super) fn printenv(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.is_empty() {
        for entry in ctx.env.entries() {
            write_line(ctx.stdout, format_args!("{entry}"));
        }
        return 0;
    }
    let mut status = 0;
    for name in args {
        match ctx.env.get(name).filter(|_| ctx.env.is_exported(name)) {
            Some(value) => write_line(ctx.stdout, format_args!("{value}")),
            None => status = 1,
        }
    }
    status
}

pub(super) fn uname(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut show = [false; 8];
    if args.is_empty() {
        show[0] = true;
    }
    let mut opts = Opts::new(args, "asnrvmpio");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('a') => show = [true; 8],
            Opt::Flag('s') => show[0] = true,
            Opt::Flag('n') => show[1] = true,
            Opt::Flag('r') => show[2] = true,
            Opt::Flag('v') => show[3] = true,
            Opt::Flag('m') => show[4] = true,
            Opt::Flag('p') => show[5] = true,
            Opt::Flag('i') => show[6] = true,
            Opt::Flag('o') => show[7] = true,
            other => return bad_option(ctx, "uname", other),
        }
    }
    if let Some(extra) = opts.operands.first() {
        error(ctx.stderr, "uname", &format!("extra operand '{extra}'"));
        return 2;
    }

    let fields = [
        ctx.system.sysname(),
        ctx.system.nodename(),
        ctx.system.release(),
        ctx.system.version(),
        ctx.system.machine(),
        "unknown",
        "unknown",
        "FEOS",
    ];
    let mut first = true;
    for (index, enabled) in show.iter().enumerate() {
        if !enabled {
            continue;
        }
        if !first {
            ctx.stdout.write(b" ");
        }
        ctx.stdout.write(fields[index].as_bytes());
        first = false;
    }
    ctx.stdout.write(b"\n");
    0
}

pub(super) fn whoami(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    write_line(ctx.stdout, format_args!("{}", ctx.system.username()));
    0
}

pub(super) fn id(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    write_line(
        ctx.stdout,
        format_args!(
            "uid={}({}) gid={}({}) groups={}({})",
            ctx.system.uid(),
            ctx.system.username(),
            ctx.system.gid(),
            ctx.system.username(),
            ctx.system.gid(),
            ctx.system.username()
        ),
    );
    0
}

pub(super) fn hostname(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    write_line(ctx.stdout, format_args!("{}", ctx.system.nodename()));
    0
}

pub(super) fn nproc(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    write_line(ctx.stdout, format_args!("{}", ctx.system.cpu_count()));
    0
}

pub(super) fn arch(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    write_line(ctx.stdout, format_args!("{}", ctx.system.machine()));
    0
}

pub(super) fn uptime(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let seconds = ctx.system.uptime_ms() / 1000;
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        write_line(
            ctx.stdout,
            format_args!(
                "up {days} days, {hours:02}:{minutes:02}, 1 user, load average: 0.00 0.00 0.00"
            ),
        );
    } else {
        write_line(
            ctx.stdout,
            format_args!("up {hours:02}:{minutes:02}, 1 user, load average: 0.00 0.00 0.00"),
        );
    }
    0
}

pub(super) fn free(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let human = args.iter().any(|arg| arg.contains('h'));
    let Some(memory) = ctx.system.memory() else {
        error(ctx.stderr, "free", "no memory accounting available");
        return 1;
    };
    let (total, used, free) = if human {
        (
            format_size(memory.total),
            format_size(memory.used),
            format_size(memory.free),
        )
    } else {
        (
            format!("{}", memory.total),
            format!("{}", memory.used),
            format!("{}", memory.free),
        )
    };
    write_line(
        ctx.stdout,
        format_args!(
            "              total        used        free      shared  buff/cache   available"
        ),
    );
    write_line(
        ctx.stdout,
        format_args!(
            "Mem:     {total:>10}  {used:>10}  {free:>10}           0           0  {free:>10}"
        ),
    );
    0
}

pub(super) fn test(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    test_command("test", args, ctx)
}

/// `[` requires its closing bracket; `test` treats `]` as an ordinary word.
pub(super) fn bracket(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    match args.split_last() {
        Some((last, inner)) if last == "]" => test_command("[", inner, ctx),
        _ => {
            error(ctx.stderr, "[", "missing ']'");
            2
        }
    }
}

fn test_command(name: &str, args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.is_empty() {
        return 1;
    }

    // POSIX fixes the meaning of short argument lists, so an operand that looks
    // like an operator (`test -n`, `[ = ]`) cannot derail the expression parser.
    let simple = match args {
        [only] => Some(Ok(!only.is_empty())),
        [not, only] if not == "!" => Some(Ok(only.is_empty())),
        [left, operator, right] if is_binary_test(operator) => {
            Some(compare(left, operator, right))
        }
        _ => None,
    };
    if let Some(result) = simple {
        return match result {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(message) => {
                error(ctx.stderr, name, &message);
                2
            }
        };
    }

    let mut parser = TestParser {
        args,
        index: 0,
        depth: 0,
        ctx,
    };
    match parser.parse_or() {
        Ok(value) => {
            if parser.index != args.len() {
                error(parser.ctx.stderr, name, "too many arguments");
                2
            } else if value {
                0
            } else {
                1
            }
        }
        Err(message) => {
            error(parser.ctx.stderr, name, &message);
            2
        }
    }
}

struct TestParser<'a, 'b, 'c> {
    args: &'a [String],
    index: usize,
    depth: usize,
    ctx: &'b mut Ctx<'c>,
}

impl TestParser<'_, '_, '_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.index).map(String::as_str)
    }

    fn take(&mut self) -> Option<String> {
        let value = self.args.get(self.index).cloned();
        if value.is_some() {
            self.index += 1;
        }
        value
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            return Err(String::from("expression too deeply nested"));
        }
        Ok(())
    }

    fn parse_or(&mut self) -> Result<bool, String> {
        let mut value = self.parse_and()?;
        while self.peek() == Some("-o") {
            self.index += 1;
            let right = self.parse_and()?;
            value = value || right;
        }
        Ok(value)
    }

    fn parse_and(&mut self) -> Result<bool, String> {
        let mut value = self.parse_not()?;
        while self.peek() == Some("-a") {
            self.index += 1;
            let right = self.parse_not()?;
            value = value && right;
        }
        Ok(value)
    }

    fn parse_not(&mut self) -> Result<bool, String> {
        if self.peek() == Some("!") {
            self.index += 1;
            self.enter()?;
            let value = !self.parse_not()?;
            self.depth -= 1;
            return Ok(value);
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<bool, String> {
        if self.peek() == Some("(") {
            self.index += 1;
            self.enter()?;
            let value = self.parse_or()?;
            self.depth -= 1;
            if self.take().as_deref() != Some(")") {
                return Err(String::from("missing ')'"));
            }
            return Ok(value);
        }

        let Some(first) = self.take() else {
            return Err(String::from("argument expected"));
        };

        // A binary operator wins over reading the first word as a unary one,
        // so `[ -f = -f ]` compares strings.
        if let Some(operator) = self.peek().filter(|word| is_binary_test(word)) {
            let operator = operator.to_string();
            self.index += 1;
            let right = self.take().ok_or("argument expected")?;
            return compare(&first, &operator, &right);
        }

        match first.as_str() {
            "-e" | "-r" | "-w" | "-x" | "-f" | "-d" | "-s" | "-L" | "-h" => {
                let path = self.take().ok_or("argument expected")?;
                Ok(file_test(self.ctx, &first, &path))
            }
            "-z" => Ok(self.take().ok_or("argument expected")?.is_empty()),
            "-n" => Ok(!self.take().ok_or("argument expected")?.is_empty()),
            _ => Ok(!first.is_empty()),
        }
    }
}

fn is_binary_test(word: &str) -> bool {
    matches!(
        word,
        "=" | "==" | "!=" | "<" | ">" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
    )
}

fn file_test(ctx: &mut Ctx<'_>, operator: &str, path: &str) -> bool {
    let Ok(meta) = ctx.fs.meta(path) else {
        return false;
    };
    match operator {
        "-f" => meta.kind == Kind::File,
        "-d" => meta.kind == Kind::Dir,
        "-s" => meta.size > 0 || meta.kind == Kind::Dir,
        // There are no symbolic links.
        "-L" | "-h" => false,
        // There are no permissions, so anything that exists is accessible, and
        // directories and scripts count as executable.
        _ => true,
    }
}

fn compare(left: &str, operator: &str, right: &str) -> Result<bool, String> {
    match operator {
        "=" | "==" => Ok(left == right),
        "!=" => Ok(left != right),
        "<" => Ok(left < right),
        ">" => Ok(left > right),
        _ => {
            let number = |text: &str| -> Result<i64, String> {
                parse_i64(text.trim()).ok_or_else(|| format!("{text}: integer expression expected"))
            };
            let left = number(left)?;
            let right = number(right)?;
            Ok(match operator {
                "-eq" => left == right,
                "-ne" => left != right,
                "-lt" => left < right,
                "-le" => left <= right,
                "-gt" => left > right,
                "-ge" => left >= right,
                _ => false,
            })
        }
    }
}

pub(super) fn expr(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.is_empty() {
        return usage(ctx.stderr, "expr", "expr ARG...");
    }

    if args[0] == "length" && args.len() == 2 {
        let length = args[1].chars().count();
        write_line(ctx.stdout, format_args!("{length}"));
        return i32::from(length == 0);
    }
    if args[0] == "index" && args.len() == 3 {
        let found = args[1]
            .chars()
            .position(|ch| args[2].contains(ch))
            .map_or(0, |index| index + 1);
        write_line(ctx.stdout, format_args!("{found}"));
        return i32::from(found == 0);
    }
    if args[0] == "substr" && args.len() == 4 {
        let position = parse_i64(&args[2]).unwrap_or(0);
        let length = parse_i64(&args[3]).unwrap_or(0).max(0) as usize;
        let result: String = if position < 1 {
            String::new()
        } else {
            args[1]
                .chars()
                .skip(position as usize - 1)
                .take(length)
                .collect()
        };
        write_line(ctx.stdout, format_args!("{result}"));
        return i32::from(result.is_empty());
    }
    if args[0] == "match" && args.len() == 3 {
        let value = regex_match(&args[1], &args[2]);
        return finish_expr(ctx, value);
    }

    let mut parser = ExprParser {
        args,
        index: 0,
        depth: 0,
    };
    let result = parser.or().and_then(|value| {
        if parser.index == args.len() {
            Ok(value)
        } else {
            Err(String::from("syntax error"))
        }
    });
    finish_expr(ctx, result)
}

fn finish_expr(ctx: &mut Ctx<'_>, result: Result<ExprValue, String>) -> i32 {
    match result {
        Ok(value) => {
            let text = value.text();
            write_line(ctx.stdout, format_args!("{text}"));
            i32::from(value.is_null())
        }
        Err(message) => {
            error(ctx.stderr, "expr", &message);
            2
        }
    }
}

#[derive(Clone, Debug)]
enum ExprValue {
    Int(i64),
    Text(String),
}

impl ExprValue {
    fn from_word(word: &str) -> Self {
        match parse_i64(word) {
            Some(number) if !word.starts_with('+') => Self::Int(number),
            _ => Self::Text(word.to_string()),
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Int(number) => number.to_string(),
            Self::Text(text) => text.clone(),
        }
    }

    fn is_null(&self) -> bool {
        match self {
            Self::Int(number) => *number == 0,
            Self::Text(text) => text.is_empty(),
        }
    }

    fn int(&self) -> Result<i64, String> {
        match self {
            Self::Int(number) => Ok(*number),
            Self::Text(_) => Err(String::from("non-integer argument")),
        }
    }
}

/// The POSIX `expr` grammar, loosest operators first: `|`, `&`, comparisons,
/// `+ -`, `* / %`, then `:`.
struct ExprParser<'a> {
    args: &'a [String],
    index: usize,
    depth: usize,
}

impl ExprParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.index).map(String::as_str)
    }

    fn or(&mut self) -> Result<ExprValue, String> {
        let mut left = self.and()?;
        while self.peek() == Some("|") {
            self.index += 1;
            let right = self.and()?;
            if left.is_null() {
                left = if right.is_null() {
                    ExprValue::Int(0)
                } else {
                    right
                };
            }
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<ExprValue, String> {
        let mut left = self.comparison()?;
        while self.peek() == Some("&") {
            self.index += 1;
            let right = self.comparison()?;
            if left.is_null() || right.is_null() {
                left = ExprValue::Int(0);
            }
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<ExprValue, String> {
        let mut left = self.sum()?;
        while let Some(operator) = self
            .peek()
            .filter(|word| matches!(*word, "<" | "<=" | "=" | "==" | "!=" | ">=" | ">"))
            .map(str::to_string)
        {
            self.index += 1;
            let right = self.sum()?;
            let ordering = match (&left, &right) {
                (ExprValue::Int(a), ExprValue::Int(b)) => a.cmp(b),
                _ => left.text().cmp(&right.text()),
            };
            let result = match operator.as_str() {
                "<" => ordering.is_lt(),
                "<=" => ordering.is_le(),
                "=" | "==" => ordering.is_eq(),
                "!=" => ordering.is_ne(),
                ">=" => ordering.is_ge(),
                _ => ordering.is_gt(),
            };
            left = ExprValue::Int(i64::from(result));
        }
        Ok(left)
    }

    fn sum(&mut self) -> Result<ExprValue, String> {
        let mut left = self.product()?;
        while let Some(operator) = self.peek().filter(|word| matches!(*word, "+" | "-")) {
            let add = operator == "+";
            self.index += 1;
            let right = self.product()?.int()?;
            let value = left.int()?;
            left = ExprValue::Int(if add {
                value.wrapping_add(right)
            } else {
                value.wrapping_sub(right)
            });
        }
        Ok(left)
    }

    fn product(&mut self) -> Result<ExprValue, String> {
        let mut left = self.matching()?;
        while let Some(operator) = self
            .peek()
            .filter(|word| matches!(*word, "*" | "/" | "%"))
            .map(str::to_string)
        {
            self.index += 1;
            let right = self.matching()?.int()?;
            let value = left.int()?;
            left = ExprValue::Int(match operator.as_str() {
                "*" => value.wrapping_mul(right),
                _ if right == 0 => return Err(String::from("division by zero")),
                "/" => value.wrapping_div(right),
                _ => value.wrapping_rem(right),
            });
        }
        Ok(left)
    }

    fn matching(&mut self) -> Result<ExprValue, String> {
        let mut left = self.primary()?;
        while self.peek() == Some(":") {
            self.index += 1;
            let pattern = self.primary()?;
            left = regex_match(&left.text(), &pattern.text())?;
        }
        Ok(left)
    }

    fn primary(&mut self) -> Result<ExprValue, String> {
        let Some(word) = self.peek().map(str::to_string) else {
            return Err(String::from("syntax error"));
        };
        self.index += 1;
        if word == "(" {
            self.depth += 1;
            if self.depth > MAX_EXPR_DEPTH {
                return Err(String::from("expression too deeply nested"));
            }
            let value = self.or()?;
            self.depth -= 1;
            if self.peek() != Some(")") {
                return Err(String::from("syntax error: expecting ')'"));
            }
            self.index += 1;
            return Ok(value);
        }
        Ok(ExprValue::from_word(&word))
    }
}

/// `STRING : REGEX`: an anchored basic regular expression. The result is the
/// first group when there is one, otherwise the number of bytes matched.
fn regex_match(text: &str, pattern: &str) -> Result<ExprValue, String> {
    let mut anchored = String::from("^");
    anchored.push_str(pattern.strip_prefix('^').unwrap_or(pattern));
    let regex = Regex::new(&anchored, Syntax::Basic).map_err(String::from)?;
    let found = regex.find_at(text.as_bytes(), 0);
    if regex.groups() > 0 {
        let group = found
            .and_then(|caps| caps.group(1))
            .and_then(|(start, end)| text.get(start..end))
            .unwrap_or("");
        return Ok(ExprValue::Text(group.to_string()));
    }
    Ok(ExprValue::Int(found.map_or(0, |caps| {
        let (start, end) = caps.span();
        text.get(start..end).map_or(0, |part| part.chars().count()) as i64
    })))
}
