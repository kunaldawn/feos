//! `find`: walks a tree and evaluates an expression on each path.
//!
//! The expression language is the POSIX one: predicates joined by `-a`, `-o`,
//! `!` and parentheses, with `-print` assumed when no action is given.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{error, parse_usize, path_error, write_out, Ctx, MAX_WALK_DEPTH};
use crate::fs::{base_name, join as fs_join, FsError, Kind, Meta};
use crate::glob::glob_match;

/// Deepest parenthesis and `!` nesting in a `find` expression.
const MAX_EXPR_DEPTH: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Compare {
    Less,
    Equal,
    Greater,
}

enum Expr {
    True,
    Name(String, bool),
    Path(String),
    Type(Kind),
    Size(Compare, usize),
    Empty,
    Print(char),
    Delete,
    Exec(Vec<String>),
    Prune,
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

struct ExprParser<'a> {
    args: &'a [String],
    index: usize,
    depth: usize,
    has_action: bool,
    has_delete: bool,
    max_depth: Option<usize>,
    min_depth: usize,
    depth_first: bool,
}

impl ExprParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.index).map(String::as_str)
    }

    fn value(&mut self, predicate: &str) -> Result<String, String> {
        let value = self.args.get(self.index).cloned();
        self.index += 1;
        value.ok_or_else(|| format!("missing argument to '{predicate}'"))
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while matches!(self.peek(), Some("-o" | "-or")) {
            self.index += 1;
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.not()?;
        loop {
            match self.peek() {
                Some("-a" | "-and") => self.index += 1,
                Some(")" | "-o" | "-or") | None => return Ok(left),
                Some(_) => {}
            }
            let right = self.not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
    }

    fn not(&mut self) -> Result<Expr, String> {
        if matches!(self.peek(), Some("!" | "-not")) {
            self.index += 1;
            self.depth += 1;
            if self.depth > MAX_EXPR_DEPTH {
                return Err(String::from("expression too deeply nested"));
            }
            let inner = self.not()?;
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let Some(word) = self.peek().map(str::to_string) else {
            return Err(String::from("expected an expression"));
        };
        self.index += 1;
        match word.as_str() {
            "(" => {
                self.depth += 1;
                if self.depth > MAX_EXPR_DEPTH {
                    return Err(String::from("expression too deeply nested"));
                }
                let inner = self.or()?;
                self.depth -= 1;
                if self.peek() != Some(")") {
                    return Err(String::from("missing ')'"));
                }
                self.index += 1;
                Ok(inner)
            }
            "-name" => Ok(Expr::Name(self.value(&word)?, false)),
            "-iname" => Ok(Expr::Name(self.value(&word)?.to_ascii_lowercase(), true)),
            "-path" | "-wholename" => Ok(Expr::Path(self.value(&word)?)),
            "-type" => match self.value(&word)?.as_str() {
                "f" => Ok(Expr::Type(Kind::File)),
                "d" => Ok(Expr::Type(Kind::Dir)),
                other => Err(format!("unknown argument to -type: {other}")),
            },
            "-size" => {
                let text = self.value(&word)?;
                parse_size(&text).ok_or_else(|| format!("invalid -size '{text}'"))
            }
            "-empty" => Ok(Expr::Empty),
            "-true" => Ok(Expr::True),
            "-false" => Ok(Expr::Not(Box::new(Expr::True))),
            "-print" => {
                self.has_action = true;
                Ok(Expr::Print('\n'))
            }
            "-print0" => {
                self.has_action = true;
                Ok(Expr::Print('\0'))
            }
            "-delete" => {
                self.has_action = true;
                self.has_delete = true;
                Ok(Expr::Delete)
            }
            "-prune" => Ok(Expr::Prune),
            "-exec" => {
                self.has_action = true;
                let start = self.index;
                while let Some(arg) = self.peek() {
                    if arg == ";" || arg == "+" {
                        break;
                    }
                    self.index += 1;
                }
                if self.peek().is_none() || self.index == start {
                    return Err(String::from("missing argument to '-exec'"));
                }
                let command = self.args[start..self.index].to_vec();
                self.index += 1;
                Ok(Expr::Exec(command))
            }
            "-maxdepth" => {
                let text = self.value(&word)?;
                self.max_depth = Some(parse_usize(&text).ok_or("invalid -maxdepth")?);
                Ok(Expr::True)
            }
            "-mindepth" => {
                let text = self.value(&word)?;
                self.min_depth = parse_usize(&text).ok_or("invalid -mindepth")?;
                Ok(Expr::True)
            }
            "-depth" => {
                self.depth_first = true;
                Ok(Expr::True)
            }
            other => Err(format!("unknown predicate '{other}'")),
        }
    }
}

/// Parses `-size [+-]N[ckMG]`; a bare number counts 512-byte blocks.
fn parse_size(text: &str) -> Option<Expr> {
    let (compare, rest) = match text.as_bytes().first()? {
        b'+' => (Compare::Greater, &text[1..]),
        b'-' => (Compare::Less, &text[1..]),
        _ => (Compare::Equal, text),
    };
    let digits = rest.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(rest.len());
    let (number, unit) = rest.split_at(digits);
    let scale = match unit {
        "" | "b" => 512,
        "c" => 1,
        "k" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        _ => return None,
    };
    Some(Expr::Size(compare, number.parse::<usize>().ok()?.checked_mul(scale)?))
}

pub(super) fn find(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    // Paths are everything before the first predicate.
    let starts_expression =
        |arg: &String| arg.starts_with('-') && arg.len() > 1 || arg == "(" || arg == "!";
    let split = args.iter().position(starts_expression).unwrap_or(args.len());
    let (path_args, expression) = args.split_at(split);

    let mut parser = ExprParser {
        args: expression,
        index: 0,
        depth: 0,
        has_action: false,
        has_delete: false,
        max_depth: None,
        min_depth: 0,
        depth_first: false,
    };
    let expr = if expression.is_empty() {
        Ok(Expr::True)
    } else {
        parser.or().and_then(|expr| {
            if parser.index < expression.len() {
                Err(format!("unexpected '{}'", expression[parser.index]))
            } else {
                Ok(expr)
            }
        })
    };
    let expr = match expr {
        Ok(expr) => expr,
        Err(message) => {
            error(ctx.stderr, "find", &message);
            return 2;
        }
    };

    let walk = Walk {
        expr: &expr,
        print_by_default: !parser.has_action,
        max_depth: parser.max_depth,
        min_depth: parser.min_depth,
        depth_first: parser.depth_first || parser.has_delete,
    };

    let default_path = [String::from(".")];
    let paths = if path_args.is_empty() {
        &default_path[..]
    } else {
        path_args
    };
    let mut status = 0;
    for path in paths {
        if let Err(err) = walk.visit(ctx, path, 0, &mut status) {
            path_error(ctx.stderr, "find", path, err);
            status = 1;
        }
    }
    status
}

struct Walk<'a> {
    expr: &'a Expr,
    print_by_default: bool,
    max_depth: Option<usize>,
    min_depth: usize,
    depth_first: bool,
}

impl Walk<'_> {
    fn visit(
        &self,
        ctx: &mut Ctx<'_>,
        path: &str,
        depth: usize,
        status: &mut i32,
    ) -> Result<(), FsError> {
        if depth > MAX_WALK_DEPTH {
            return Err(FsError::TooDeep);
        }
        if ctx.cancelled() {
            return Ok(());
        }
        let meta = ctx.fs.meta(path)?;
        let mut prune = false;

        if !self.depth_first {
            self.check(ctx, path, meta, depth, &mut prune, status)?;
        }
        let descend = meta.kind == Kind::Dir
            && !prune
            && self.max_depth.is_none_or(|max| depth < max);
        if descend {
            let entries = ctx.fs.read_dir(path)?;
            for entry in entries {
                let child = fs_join(path, &entry.name);
                if let Err(err) = self.visit(ctx, &child, depth + 1, status) {
                    path_error(ctx.stderr, "find", &child, err);
                    *status = 1;
                }
            }
        }
        if self.depth_first {
            self.check(ctx, path, meta, depth, &mut prune, status)?;
        }
        Ok(())
    }

    fn check(
        &self,
        ctx: &mut Ctx<'_>,
        path: &str,
        meta: Meta,
        depth: usize,
        prune: &mut bool,
        status: &mut i32,
    ) -> Result<(), FsError> {
        if depth < self.min_depth {
            return Ok(());
        }
        let matched = self.eval(self.expr, ctx, path, meta, prune, status);
        if matched && self.print_by_default {
            let shown = super::files::listed_name(ctx, path);
            write_out(ctx.stdout, format_args!("{shown}\n"));
        }
        Ok(())
    }

    fn eval(
        &self,
        expr: &Expr,
        ctx: &mut Ctx<'_>,
        path: &str,
        meta: Meta,
        prune: &mut bool,
        status: &mut i32,
    ) -> bool {
        match expr {
            Expr::True => true,
            Expr::Name(pattern, fold) => {
                let name = if path == "/" { "/" } else { base_name(path.trim_end_matches('/')) };
                if *fold {
                    glob_match(pattern, &name.to_ascii_lowercase())
                } else {
                    glob_match(pattern, name)
                }
            }
            Expr::Path(pattern) => glob_match(pattern, path),
            Expr::Type(kind) => meta.kind == *kind,
            Expr::Size(compare, bytes) => match compare {
                Compare::Less => meta.size < *bytes,
                Compare::Equal => meta.size == *bytes,
                Compare::Greater => meta.size > *bytes,
            },
            Expr::Empty => match meta.kind {
                Kind::File => meta.size == 0,
                Kind::Dir => ctx.fs.read_dir(path).is_ok_and(|entries| entries.is_empty()),
            },
            Expr::Print(terminator) => {
                write_out(ctx.stdout, format_args!("{path}{terminator}"));
                true
            }
            Expr::Delete => {
                let result = match meta.kind {
                    Kind::File => ctx.fs.remove_file(path),
                    Kind::Dir => ctx.fs.remove_dir(path),
                };
                if let Err(err) = result {
                    path_error(ctx.stderr, "find", path, err);
                    *status = 1;
                    return false;
                }
                true
            }
            Expr::Exec(command) => {
                let argv: Vec<String> = command
                    .iter()
                    .map(|arg| arg.replace("{}", path))
                    .collect();
                let Some((name, rest)) = argv.split_first() else {
                    return false;
                };
                super::run_nested(name, rest, ctx) == 0
            }
            Expr::Prune => {
                *prune = true;
                true
            }
            Expr::Not(inner) => !self.eval(inner, ctx, path, meta, prune, status),
            Expr::And(left, right) => {
                self.eval(left, ctx, path, meta, prune, status)
                    && self.eval(right, ctx, path, meta, prune, status)
            }
            Expr::Or(left, right) => {
                self.eval(left, ctx, path, meta, prune, status)
                    || self.eval(right, ctx, path, meta, prune, status)
            }
        }
    }
}
