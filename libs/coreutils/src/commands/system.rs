//! System, environment and expression utilities.

use alloc::format;
use alloc::string::{String, ToString};

use super::{error, format_size, is_option, parse_i64, usage, write_line, Ctx};
use crate::fs::Kind;

pub(super) fn env(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if !args.is_empty() {
        error(
            ctx.stderr,
            "env",
            "running a command with a modified environment is a shell feature",
        );
        return 2;
    }
    for entry in ctx.env.entries() {
        write_line(ctx.stdout, format_args!("{entry}"));
    }
    0
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
        match ctx.env.get(name) {
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
    for arg in args {
        if !is_option(arg) {
            error(ctx.stderr, "uname", &format!("extra operand '{arg}'"));
            return 2;
        }
        for flag in arg[1..].chars() {
            match flag {
                'a' => show = [true; 8],
                's' => show[0] = true,
                'n' => show[1] = true,
                'r' => show[2] = true,
                'v' => show[3] = true,
                'm' => show[4] = true,
                'p' => show[5] = true,
                'i' => show[6] = true,
                'o' => show[7] = true,
                other => {
                    error(ctx.stderr, "uname", &format!("invalid option -- '{other}'"));
                    return 2;
                }
            }
        }
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
    let mut args = args;
    if args.last().map(String::as_str) == Some("]") {
        args = &args[..args.len() - 1];
    }
    if args.is_empty() {
        return 1;
    }

    let mut parser = TestParser {
        args,
        index: 0,
        ctx,
    };
    match parser.parse_or() {
        Ok(value) => {
            if parser.index != args.len() {
                error(parser.ctx.stderr, "test", "extra arguments");
                2
            } else if value {
                0
            } else {
                1
            }
        }
        Err(message) => {
            error(parser.ctx.stderr, "test", message);
            2
        }
    }
}

struct TestParser<'a, 'b, 'c> {
    args: &'a [String],
    index: usize,
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

    fn parse_or(&mut self) -> Result<bool, &'static str> {
        let mut value = self.parse_and()?;
        while self.peek() == Some("-o") {
            self.index += 1;
            let right = self.parse_and()?;
            value = value || right;
        }
        Ok(value)
    }

    fn parse_and(&mut self) -> Result<bool, &'static str> {
        let mut value = self.parse_not()?;
        while self.peek() == Some("-a") {
            self.index += 1;
            let right = self.parse_not()?;
            value = value && right;
        }
        Ok(value)
    }

    fn parse_not(&mut self) -> Result<bool, &'static str> {
        if self.peek() == Some("!") {
            self.index += 1;
            return Ok(!self.parse_not()?);
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<bool, &'static str> {
        if self.peek() == Some("(") {
            self.index += 1;
            let value = self.parse_or()?;
            if self.take().as_deref() != Some(")") {
                return Err("missing ')'");
            }
            return Ok(value);
        }

        let Some(first) = self.take() else {
            return Err("expected an expression");
        };

        match first.as_str() {
            "-e" => self.exists(),
            "-f" => self.kind(Kind::File),
            "-d" => self.kind(Kind::Dir),
            "-s" => {
                let path = self.take().ok_or("missing operand")?;
                let meta = self.ctx.fs.meta(&path).map_err(|_| "missing file")?;
                Ok(meta.size > 0)
            }
            "-r" | "-w" => self.exists(),
            "-z" => Ok(self.take().unwrap_or_default().is_empty()),
            "-n" => Ok(!self.take().unwrap_or_default().is_empty()),
            _ => {
                // Binary operators have a left operand that we just consumed.
                let operator = self.peek().map(str::to_string);
                if let Some(operator) = operator {
                    if matches!(
                        operator.as_str(),
                        "=" | "==" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
                    ) {
                        self.index += 1;
                        let right = self.take().ok_or("missing right operand")?;
                        return Ok(compare(&first, &operator, &right));
                    }
                }
                Ok(!first.is_empty())
            }
        }
    }

    fn exists(&mut self) -> Result<bool, &'static str> {
        let path = self.take().ok_or("missing operand")?;
        Ok(self.ctx.fs.exists(&path))
    }

    fn kind(&mut self, expected: Kind) -> Result<bool, &'static str> {
        let path = self.take().ok_or("missing operand")?;
        Ok(self
            .ctx
            .fs
            .meta(&path)
            .map(|meta| meta.kind == expected)
            .unwrap_or(false))
    }
}

fn compare(left: &str, operator: &str, right: &str) -> bool {
    match operator {
        "=" | "==" => left == right,
        "!=" => left != right,
        _ => {
            let left = parse_i64(left).unwrap_or(0);
            let right = parse_i64(right).unwrap_or(0);
            match operator {
                "-eq" => left == right,
                "-ne" => left != right,
                "-lt" => left < right,
                "-le" => left <= right,
                "-gt" => left > right,
                "-ge" => left >= right,
                _ => false,
            }
        }
    }
}

pub(super) fn expr(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.is_empty() {
        return usage(ctx.stderr, "expr", "expr ARG...");
    }

    if args[0] == "length" {
        let value = args.get(1).map(String::as_str).unwrap_or("");
        let length = value.chars().count();
        write_line(ctx.stdout, format_args!("{length}"));
        return if length == 0 { 1 } else { 0 };
    }
    if args[0] == "index" {
        let value = args.get(1).map(String::as_str).unwrap_or("");
        let chars = args.get(2).map(String::as_str).unwrap_or("");
        let found = value
            .chars()
            .position(|ch| chars.contains(ch))
            .map(|index| index + 1)
            .unwrap_or(0);
        write_line(ctx.stdout, format_args!("{found}"));
        return if found == 0 { 1 } else { 0 };
    }
    if args[0] == "substr" {
        let value = args.get(1).map(String::as_str).unwrap_or("");
        let position = args
            .get(2)
            .and_then(|value| parse_i64(value))
            .unwrap_or(1)
            .max(1) as usize;
        let length = args
            .get(3)
            .and_then(|value| parse_i64(value))
            .unwrap_or(0)
            .max(0) as usize;
        let result: String = value.chars().skip(position - 1).take(length).collect();
        write_line(ctx.stdout, format_args!("{result}"));
        return if result.is_empty() { 1 } else { 0 };
    }

    let mut value = match parse_i64(&args[0]) {
        Some(value) => value,
        None => {
            // A bare string is echoed, as POSIX expr does.
            if args.len() == 1 {
                write_line(ctx.stdout, format_args!("{}", args[0]));
                return if args[0].is_empty() { 1 } else { 0 };
            }
            error(
                ctx.stderr,
                "expr",
                &format!("non-numeric argument '{}'", args[0]),
            );
            return 2;
        }
    };

    let mut index = 1usize;
    while index < args.len() {
        let operator = args[index].as_str();
        let Some(right) = args.get(index + 1).and_then(|value| parse_i64(value)) else {
            error(ctx.stderr, "expr", "syntax error");
            return 2;
        };
        value = match operator {
            "+" => value.wrapping_add(right),
            "-" => value.wrapping_sub(right),
            "*" => value.wrapping_mul(right),
            "/" => {
                if right == 0 {
                    error(ctx.stderr, "expr", "division by zero");
                    return 2;
                }
                value / right
            }
            "%" => {
                if right == 0 {
                    error(ctx.stderr, "expr", "division by zero");
                    return 2;
                }
                value % right
            }
            "<" => i64::from(value < right),
            "<=" => i64::from(value <= right),
            "=" | "==" => i64::from(value == right),
            "!=" => i64::from(value != right),
            ">=" => i64::from(value >= right),
            ">" => i64::from(value > right),
            "&" => value & right,
            "|" => value | right,
            _ => {
                error(
                    ctx.stderr,
                    "expr",
                    &format!("unknown operator '{operator}'"),
                );
                return 2;
            }
        };
        index += 2;
    }

    write_line(ctx.stdout, format_args!("{value}"));
    if value == 0 {
        1
    } else {
        0
    }
}
