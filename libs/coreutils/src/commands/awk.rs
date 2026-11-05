//! `awk`: patterns, actions, fields, associative arrays and functions.
//!
//! Supports the POSIX language apart from pipes to and from commands and
//! `system`, which have no process to run in this design. Numbers are `f64`, and
//! `core` has no maths library, so the few functions awk needs are here.
//!
//! The program comes from a user, so nothing here may panic or run away with the
//! stack: nesting is limited while parsing and while running.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::Ctx;
use crate::io::MAX_STREAM_BYTES;
use crate::regex::{Regex, Syntax};

/// Deepest nesting of parentheses, blocks and unary operators in a program.
const MAX_PARSE_DEPTH: usize = 60;
/// Deepest nesting of expression evaluation and of function calls.
const MAX_EVAL_DEPTH: usize = 160;
const MAX_CALL_DEPTH: usize = 32;
/// Widest field `printf` will pad to.
const MAX_WIDTH: usize = 4096;
/// Most arrays a program can create, to bound what recursion can allocate.
const MAX_ARRAYS: usize = 4096;
/// Bytes kept for one output file before the program ends in an error.
const CHUNK: usize = 4096;

// ---------------------------------------------------------------- numbers ---

fn floor(value: f64) -> f64 {
    if !value.is_finite() || value.abs() >= 4_503_599_627_370_496.0 {
        return value;
    }
    let truncated = value as i64 as f64;
    if truncated > value {
        truncated - 1.0
    } else {
        truncated
    }
}

fn truncate(value: f64) -> f64 {
    if value < 0.0 {
        -floor(-value)
    } else {
        floor(value)
    }
}

fn sqrt(value: f64) -> f64 {
    if value < 0.0 || value.is_nan() {
        return f64::NAN;
    }
    if value == 0.0 || value.is_infinite() {
        return value;
    }
    let mut guess = if value > 1.0 { value / 2.0 } else { 1.0 };
    for _ in 0..1100 {
        let next = (guess + value / guess) / 2.0;
        if (next - guess).abs() <= f64::EPSILON * next {
            return next;
        }
        guess = next;
    }
    guess
}

fn exp(value: f64) -> f64 {
    if value.is_nan() {
        return value;
    }
    if value > 709.8 {
        return f64::INFINITY;
    }
    if value < -745.2 {
        return 0.0;
    }
    // e^x = 2^k * e^r, with r small enough for the series to converge fast.
    let k = floor(value / core::f64::consts::LN_2 + 0.5);
    let r = value - k * core::f64::consts::LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..24 {
        term *= r / n as f64;
        sum += term;
    }
    scale_by_two(sum, k as i32)
}

fn scale_by_two(mut value: f64, mut k: i32) -> f64 {
    while k > 0 {
        let step = k.min(1000);
        value *= f64::from_bits(((step + 1023) as u64) << 52);
        k -= step;
    }
    while k < 0 {
        let step = (-k).min(1000);
        value *= f64::from_bits(((1023 - step) as u64) << 52);
        k += step;
    }
    value
}

fn log(value: f64) -> f64 {
    if value.is_nan() || value < 0.0 {
        return f64::NAN;
    }
    if value == 0.0 {
        return f64::NEG_INFINITY;
    }
    if value.is_infinite() {
        return value;
    }
    // Reduce to m * 2^k with m near one, then use atanh's series.
    let mut m = value;
    let mut k = 0i32;
    while m >= core::f64::consts::SQRT_2 {
        m /= 2.0;
        k += 1;
    }
    while m < core::f64::consts::FRAC_1_SQRT_2 {
        m *= 2.0;
        k -= 1;
    }
    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    let mut term = t;
    let mut sum = 0.0;
    let mut n = 1;
    while n < 40 {
        sum += term / n as f64;
        term *= t2;
        n += 2;
    }
    2.0 * sum + k as f64 * core::f64::consts::LN_2
}

fn pow(base: f64, power: f64) -> f64 {
    if power == 0.0 {
        return 1.0;
    }
    if power == truncate(power) && power.abs() < 1024.0 {
        let mut result = 1.0;
        let mut factor = base;
        let mut left = power.abs() as u32;
        while left > 0 {
            if left & 1 == 1 {
                result *= factor;
            }
            factor *= factor;
            left >>= 1;
        }
        return if power < 0.0 { 1.0 / result } else { result };
    }
    if base == 0.0 {
        return 0.0;
    }
    exp(power * log(base))
}

/// Brings an angle into `[-pi, pi]`.
fn reduce_angle(value: f64) -> f64 {
    let tau = core::f64::consts::TAU;
    value - tau * floor(value / tau + 0.5)
}

fn sin(value: f64) -> f64 {
    if !value.is_finite() {
        return f64::NAN;
    }
    let x = reduce_angle(value);
    let mut term = x;
    let mut sum = x;
    for n in 1..14 {
        let d = (2 * n) as f64;
        term *= -x * x / (d * (d + 1.0));
        sum += term;
    }
    sum
}

fn cos(value: f64) -> f64 {
    if !value.is_finite() {
        return f64::NAN;
    }
    let x = reduce_angle(value);
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..14 {
        let d = (2 * n) as f64;
        term *= -x * x / ((d - 1.0) * d);
        sum += term;
    }
    sum
}

fn atan2(y: f64, x: f64) -> f64 {
    use core::f64::consts::{FRAC_PI_2, PI};
    if y.is_nan() || x.is_nan() {
        return f64::NAN;
    }
    if x == 0.0 && y == 0.0 {
        return 0.0;
    }
    if x == 0.0 {
        return if y > 0.0 { FRAC_PI_2 } else { -FRAC_PI_2 };
    }
    let base = atan(y / x);
    if x > 0.0 {
        base
    } else if y >= 0.0 {
        base + PI
    } else {
        base - PI
    }
}

fn atan(value: f64) -> f64 {
    use core::f64::consts::FRAC_PI_2;
    if value.is_infinite() {
        return if value > 0.0 { FRAC_PI_2 } else { -FRAC_PI_2 };
    }
    if value.abs() > 1.0 {
        return (if value > 0.0 { FRAC_PI_2 } else { -FRAC_PI_2 }) - atan(1.0 / value);
    }
    // Halving the angle twice makes the series converge within a few terms.
    let mut x = value;
    for _ in 0..2 {
        x /= 1.0 + sqrt(1.0 + x * x);
    }
    let mut term = x;
    let mut sum = 0.0;
    for n in 0..16 {
        sum += term / (2 * n + 1) as f64;
        term *= -x * x;
    }
    4.0 * sum
}

/// True when text is a number as a whole, surrounding blanks aside.
fn looks_numeric(text: &str) -> bool {
    let text = text.trim_matches([' ', '\t', '\n']);
    !text.is_empty()
        && text.bytes().any(|byte| byte.is_ascii_digit())
        && text.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E'))
        && text.parse::<f64>().is_ok()
}

/// The longest number at the start of text, as awk converts strings.
fn parse_prefix(text: &str) -> f64 {
    let text = text.trim_start_matches([' ', '\t', '\n']);
    let bytes = text.as_bytes();
    let mut end = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let digits = |from: usize| bytes.get(from..).map_or(0, |rest| {
        rest.iter().take_while(|byte| byte.is_ascii_digit()).count()
    });
    let whole = digits(end);
    end += whole;
    let mut fraction = 0;
    if bytes.get(end) == Some(&b'.') {
        fraction = digits(end + 1);
        if whole > 0 || fraction > 0 {
            end += 1 + fraction;
        }
    }
    if whole == 0 && fraction == 0 {
        return 0.0;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let power = digits(end + 1 + sign);
        if power > 0 {
            end += 1 + sign + power;
        }
    }
    text.get(..end).and_then(|part| part.parse::<f64>().ok()).unwrap_or(0.0)
}

fn format_exponent(value: f64, precision: usize, upper: bool) -> String {
    let text = format!("{value:.precision$e}");
    let (mantissa, power) = text.split_once('e').unwrap_or((&text, "0"));
    let power: i32 = power.parse().unwrap_or(0);
    let sign = if power < 0 { '-' } else { '+' };
    let letter = if upper { 'E' } else { 'e' };
    format!("{mantissa}{letter}{sign}{:02}", power.abs())
}

/// `%g`: the shorter of `%e` and `%f`, without trailing zeros.
fn format_general(value: f64, precision: usize, alternate: bool, upper: bool) -> String {
    let precision = precision.max(1);
    if value == 0.0 {
        return String::from("0");
    }
    let scientific = format!("{:.*e}", precision - 1, value);
    let power: i32 = scientific
        .split_once('e')
        .and_then(|(_, power)| power.parse().ok())
        .unwrap_or(0);
    let mut text = if power < -4 || power >= precision as i32 {
        format_exponent(value, precision - 1, upper)
    } else {
        let decimals = (precision as i32 - 1 - power).max(0) as usize;
        format!("{value:.decimals$}")
    };
    if !alternate {
        let (mantissa, rest) = match text.find(['e', 'E']) {
            Some(index) => text.split_at(index),
            None => (text.as_str(), ""),
        };
        if mantissa.contains('.') {
            let trimmed = mantissa.trim_end_matches('0').trim_end_matches('.');
            text = format!("{trimmed}{rest}");
        }
    }
    text
}

fn number_text(value: f64, format_spec: &str) -> String {
    if value.is_nan() {
        return String::from("nan");
    }
    if value.is_infinite() {
        return String::from(if value > 0.0 { "inf" } else { "-inf" });
    }
    if value == truncate(value) && value.abs() < 1e16 {
        return format!("{}", value as i64);
    }
    sprintf(format_spec, &[Value::Num(value)]).unwrap_or_else(|_| format_general(value, 6, false, false))
}

// ----------------------------------------------------------------- values ---

#[derive(Clone, Debug)]
enum Value {
    Uninit,
    Num(f64),
    Str(String),
    /// Input that looks numeric: compares as a number, but keeps its spelling.
    StrNum(String, f64),
}

impl Value {
    /// A value from outside the program, numeric when it looks it.
    fn input(text: &str) -> Self {
        if looks_numeric(text) {
            Self::StrNum(text.to_string(), parse_prefix(text))
        } else {
            Self::Str(text.to_string())
        }
    }

    fn num(&self) -> f64 {
        match self {
            Self::Uninit => 0.0,
            Self::Num(value) | Self::StrNum(_, value) => *value,
            Self::Str(text) => parse_prefix(text),
        }
    }

    fn truth(&self) -> bool {
        match self {
            Self::Uninit => false,
            Self::Num(value) | Self::StrNum(_, value) => *value != 0.0,
            Self::Str(text) => !text.is_empty(),
        }
    }

    fn is_string(&self) -> bool {
        matches!(self, Self::Str(_))
    }
}

// ------------------------------------------------------------------ lexer ---

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Str(String),
    Regex(String),
    Name(String),
    /// A name directly followed by `(`: a call to a user function.
    FuncName(String),
    Builtin(String),
    Keyword(&'static str),
    Op(&'static str),
    Newline,
    Eof,
}

const KEYWORDS: &[&str] = &[
    "BEGIN", "END", "function", "func", "if", "else", "while", "for", "do", "break", "continue",
    "next", "nextfile", "exit", "return", "delete", "in", "getline", "print", "printf",
];

const BUILTINS: &[&str] = &[
    "length", "substr", "index", "split", "sub", "gsub", "match", "sprintf", "tolower", "toupper",
    "int", "sqrt", "exp", "log", "sin", "cos", "atan2", "rand", "srand", "close", "fflush",
    "system",
];

const OPS: &[&str] = &[
    "**=", "&&", "||", "++", "--", "+=", "-=", "*=", "/=", "%=", "^=", "==", "<=", ">=", "!=",
    "!~", ">>", "**", "{", "}", "(", ")", "[", "]", ";", ",", "+", "-", "*", "/", "%", "^", "!",
    ">", "<", "|", "?", ":", "~", "$", "=",
];

struct Lexer {
    chars: Vec<char>,
    position: usize,
    previous: Token,
}

impl Lexer {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            position: 0,
            previous: Token::Newline,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    /// A `/` starts a regular expression unless a value has just ended.
    fn regex_allowed(&self) -> bool {
        !matches!(
            &self.previous,
            Token::Number(_) | Token::Str(_) | Token::Name(_) | Token::Builtin(_)
        ) && !matches!(self.previous, Token::Op(")" | "]" | "$" | "++" | "--"))
    }

    fn next(&mut self) -> Result<Token, String> {
        let token = self.scan()?;
        self.previous = token.clone();
        Ok(token)
    }

    fn scan(&mut self) -> Result<Token, String> {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r') => self.position += 1,
                Some('\\') if self.chars.get(self.position + 1) == Some(&'\n') => self.position += 2,
                Some('#') => {
                    while self.peek().is_some_and(|ch| ch != '\n') {
                        self.position += 1;
                    }
                }
                _ => break,
            }
        }
        let Some(ch) = self.peek() else {
            return Ok(Token::Eof);
        };
        if ch == '\n' {
            self.position += 1;
            return Ok(Token::Newline);
        }
        if ch.is_ascii_digit()
            || (ch == '.' && self.chars.get(self.position + 1).is_some_and(char::is_ascii_digit))
        {
            return Ok(self.number());
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = self.position;
            while self.peek().is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
                self.position += 1;
            }
            let word: String = self.chars[start..self.position].iter().collect();
            if let Some(keyword) = KEYWORDS.iter().find(|keyword| **keyword == word) {
                return Ok(Token::Keyword(keyword));
            }
            if BUILTINS.contains(&word.as_str()) {
                return Ok(Token::Builtin(word));
            }
            if self.peek() == Some('(') {
                return Ok(Token::FuncName(word));
            }
            return Ok(Token::Name(word));
        }
        if ch == '"' {
            return self.string();
        }
        if ch == '/' && self.regex_allowed() {
            return self.regex();
        }
        for op in OPS {
            let matches = op.chars().enumerate().all(|(offset, want)| {
                self.chars.get(self.position + offset) == Some(&want)
            });
            if matches {
                self.position += op.chars().count();
                return Ok(Token::Op(op));
            }
        }
        Err(format!("unexpected character '{ch}'"))
    }

    fn number(&mut self) -> Token {
        let start = self.position;
        while self.peek().is_some_and(|ch| ch.is_ascii_digit() || ch == '.') {
            self.position += 1;
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let mut end = self.position + 1;
            if matches!(self.chars.get(end), Some('+' | '-')) {
                end += 1;
            }
            if self.chars.get(end).is_some_and(char::is_ascii_digit) {
                self.position = end;
                while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                    self.position += 1;
                }
            }
        }
        let text: String = self.chars[start..self.position].iter().collect();
        Token::Number(parse_prefix(&text))
    }

    fn string(&mut self) -> Result<Token, String> {
        self.position += 1;
        let mut text = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(String::from("unterminated string"));
            };
            self.position += 1;
            match ch {
                '"' => return Ok(Token::Str(text)),
                '\n' => return Err(String::from("newline in string")),
                '\\' => {
                    let Some(next) = self.peek() else {
                        return Err(String::from("unterminated string"));
                    };
                    self.position += 1;
                    match next {
                        '\n' => {}
                        'n' => text.push('\n'),
                        't' => text.push('\t'),
                        'r' => text.push('\r'),
                        'a' => text.push('\x07'),
                        'b' => text.push('\x08'),
                        'f' => text.push('\x0c'),
                        'v' => text.push('\x0b'),
                        '\\' => text.push('\\'),
                        '"' => text.push('"'),
                        '/' => text.push('/'),
                        digit @ '0'..='7' => {
                            let mut value = digit as u32 - '0' as u32;
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(more @ '0'..='7') => {
                                        value = value * 8 + (more as u32 - '0' as u32);
                                        self.position += 1;
                                    }
                                    _ => break,
                                }
                            }
                            text.push(char::from_u32(value).unwrap_or('?'));
                        }
                        other => {
                            text.push('\\');
                            text.push(other);
                        }
                    }
                }
                other => text.push(other),
            }
        }
    }

    fn regex(&mut self) -> Result<Token, String> {
        self.position += 1;
        let mut text = String::new();
        let mut bracket = false;
        loop {
            let Some(ch) = self.peek() else {
                return Err(String::from("unterminated regular expression"));
            };
            self.position += 1;
            match ch {
                '\n' => return Err(String::from("newline in regular expression")),
                '\\' => {
                    if let Some(next) = self.peek() {
                        self.position += 1;
                        if next != '/' {
                            text.push('\\');
                        }
                        text.push(next);
                    }
                }
                '[' => {
                    bracket = true;
                    text.push(ch);
                }
                ']' => {
                    bracket = false;
                    text.push(ch);
                }
                '/' if !bracket => return Ok(Token::Regex(text)),
                other => text.push(other),
            }
        }
    }
}

// -------------------------------------------------------------------- AST ---

#[derive(Debug)]
enum Expr {
    Number(f64),
    Str(String),
    /// A bare `/re/`: matches against `$0`.
    Match(String),
    /// A regular expression as an operand of `~`, `split` and friends.
    Regex(String),
    Field(Box<Expr>),
    Var(String),
    Index(String, Vec<Expr>),
    Assign(&'static str, Box<Expr>, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    In(Vec<Expr>, String),
    MatchOp(bool, Box<Expr>, Box<Expr>),
    Compare(&'static str, Box<Expr>, Box<Expr>),
    Concat(Box<Expr>, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Negate(Box<Expr>),
    Plus(Box<Expr>),
    Not(Box<Expr>),
    /// `++x` or `--x` (`prefix`), and `x++` or `x--`.
    Step {
        target: Box<Expr>,
        delta: f64,
        prefix: bool,
    },
    Call(String, Vec<Expr>),
    Builtin(String, Vec<Expr>),
    Getline(Option<Box<Expr>>, Option<Box<Expr>>),
    /// Parentheses. More than one member is only valid as `print`'s arguments.
    Group(Vec<Expr>),
}

#[derive(Debug)]
enum Redirect {
    None,
    Write(Expr),
    Append(Expr),
}

#[derive(Debug)]
enum Stmt {
    Expr(Expr),
    Print(Vec<Expr>, Redirect),
    Printf(Vec<Expr>, Redirect),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Box<Stmt>>, Box<Stmt>),
    ForIn(String, String, Box<Stmt>),
    Block(Vec<Stmt>),
    Next,
    NextFile,
    Exit(Option<Expr>),
    Return(Option<Expr>),
    Break,
    Continue,
    Delete(String, Option<Vec<Expr>>),
}

#[derive(Debug)]
enum Pattern {
    Begin,
    End,
    Always,
    Expr(Expr),
    Range(Expr, Expr),
}

struct Rule {
    pattern: Pattern,
    action: Option<Vec<Stmt>>,
    in_range: bool,
}

struct Function {
    params: Vec<String>,
    body: Vec<Stmt>,
}

// ----------------------------------------------------------------- parser ---

struct Parser {
    lexer: Lexer,
    token: Token,
    depth: usize,
    /// Parsing `print`'s arguments, where a bare `>` is a redirection.
    no_greater: bool,
    /// Inside `for (`'s first clause, where a bare `in` belongs to the `for`.
    no_in: bool,
}

type ParseResult<T> = Result<T, String>;

impl Parser {
    fn new(text: &str) -> ParseResult<Self> {
        let mut lexer = Lexer::new(text);
        let token = lexer.next()?;
        Ok(Self {
            lexer,
            token,
            depth: 0,
            no_greater: false,
            no_in: false,
        })
    }

    fn advance(&mut self) -> ParseResult<Token> {
        let next = self.lexer.next()?;
        Ok(core::mem::replace(&mut self.token, next))
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(&self.token, Token::Op(found) if *found == op)
    }

    fn is_keyword(&self, word: &str) -> bool {
        matches!(&self.token, Token::Keyword(found) if *found == word)
    }

    fn accept(&mut self, op: &str) -> ParseResult<bool> {
        if self.is_op(op) {
            self.advance()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn expect(&mut self, op: &str) -> ParseResult<()> {
        if self.accept(op)? {
            Ok(())
        } else {
            Err(format!("expected '{op}'"))
        }
    }

    fn skip_newlines(&mut self) -> ParseResult<()> {
        while self.token == Token::Newline {
            self.advance()?;
        }
        Ok(())
    }

    fn skip_terminators(&mut self) -> ParseResult<()> {
        while self.token == Token::Newline || self.is_op(";") {
            self.advance()?;
        }
        Ok(())
    }

    fn enter(&mut self) -> ParseResult<()> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(String::from("program is nested too deeply"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn program(&mut self) -> ParseResult<(Vec<Rule>, BTreeMap<String, Function>)> {
        let mut rules = Vec::new();
        let mut functions = BTreeMap::new();
        loop {
            self.skip_terminators()?;
            if self.token == Token::Eof {
                return Ok((rules, functions));
            }
            if self.is_keyword("function") || self.is_keyword("func") {
                self.advance()?;
                let name = match self.advance()? {
                    Token::Name(name) | Token::FuncName(name) => name,
                    _ => return Err(String::from("expected a function name")),
                };
                self.expect("(")?;
                let mut params = Vec::new();
                while !self.is_op(")") {
                    match self.advance()? {
                        Token::Name(param) => params.push(param),
                        _ => return Err(String::from("expected a parameter name")),
                    }
                    self.skip_newlines()?;
                    if !self.accept(",")? {
                        break;
                    }
                    self.skip_newlines()?;
                }
                self.expect(")")?;
                self.skip_newlines()?;
                let body = self.block()?;
                functions.insert(name, Function { params, body });
                continue;
            }
            rules.push(self.rule()?);
        }
    }

    fn rule(&mut self) -> ParseResult<Rule> {
        let pattern = if self.is_keyword("BEGIN") {
            self.advance()?;
            Pattern::Begin
        } else if self.is_keyword("END") {
            self.advance()?;
            Pattern::End
        } else if self.is_op("{") {
            Pattern::Always
        } else {
            let first = self.expression()?;
            if self.accept(",")? {
                self.skip_newlines()?;
                Pattern::Range(first, self.expression()?)
            } else {
                Pattern::Expr(first)
            }
        };
        let action = if self.is_op("{") {
            Some(self.block()?)
        } else {
            None
        };
        if action.is_none() && matches!(pattern, Pattern::Begin | Pattern::End) {
            return Err(String::from("BEGIN and END need an action"));
        }
        Ok(Rule {
            pattern,
            action,
            in_range: false,
        })
    }

    fn block(&mut self) -> ParseResult<Vec<Stmt>> {
        self.enter()?;
        self.expect("{")?;
        let mut body = Vec::new();
        loop {
            self.skip_terminators()?;
            if self.accept("}")? {
                break;
            }
            if self.token == Token::Eof {
                return Err(String::from("missing '}'"));
            }
            body.push(self.statement()?);
        }
        self.leave();
        Ok(body)
    }

    fn end_simple(&mut self) -> ParseResult<()> {
        if self.is_op(";") || self.token == Token::Newline {
            self.advance()?;
            return Ok(());
        }
        if self.is_op("}") || self.token == Token::Eof {
            return Ok(());
        }
        Err(format!("unexpected {:?}", self.token))
    }

    fn body_statement(&mut self) -> ParseResult<Box<Stmt>> {
        self.skip_newlines()?;
        if self.accept(";")? {
            return Ok(Box::new(Stmt::Block(Vec::new())));
        }
        Ok(Box::new(self.statement()?))
    }

    fn statement(&mut self) -> ParseResult<Stmt> {
        self.enter()?;
        let statement = self.statement_inner();
        self.leave();
        statement
    }

    fn statement_inner(&mut self) -> ParseResult<Stmt> {
        if self.is_op("{") {
            return Ok(Stmt::Block(self.block()?));
        }
        let keyword = match &self.token {
            Token::Keyword(word) => Some(*word),
            _ => None,
        };
        let statement = match keyword {
            Some("if") => {
                self.advance()?;
                self.expect("(")?;
                let condition = self.expression()?;
                self.expect(")")?;
                let body = self.body_statement()?;
                // `else` may follow on a later line or after a `;`.
                loop {
                    if self.token == Token::Newline || self.is_op(";") {
                        let saved_position = self.lexer.position;
                        let saved_previous = self.lexer.previous.clone();
                        let saved_token = self.token.clone();
                        self.skip_terminators()?;
                        if self.is_keyword("else") {
                            break;
                        }
                        self.lexer.position = saved_position;
                        self.lexer.previous = saved_previous;
                        self.token = saved_token;
                    }
                    break;
                }
                let otherwise = if self.is_keyword("else") {
                    self.advance()?;
                    Some(self.body_statement()?)
                } else {
                    None
                };
                return Ok(Stmt::If(condition, body, otherwise));
            }
            Some("while") => {
                self.advance()?;
                self.expect("(")?;
                let condition = self.expression()?;
                self.expect(")")?;
                return Ok(Stmt::While(condition, self.body_statement()?));
            }
            Some("do") => {
                self.advance()?;
                let body = self.body_statement()?;
                self.skip_terminators()?;
                if !self.is_keyword("while") {
                    return Err(String::from("expected 'while' after 'do'"));
                }
                self.advance()?;
                self.expect("(")?;
                let condition = self.expression()?;
                self.expect(")")?;
                self.end_simple()?;
                return Ok(Stmt::DoWhile(body, condition));
            }
            Some("for") => return self.for_statement(),
            Some("break") => {
                self.advance()?;
                Stmt::Break
            }
            Some("continue") => {
                self.advance()?;
                Stmt::Continue
            }
            Some("next") => {
                self.advance()?;
                Stmt::Next
            }
            Some("nextfile") => {
                self.advance()?;
                Stmt::NextFile
            }
            Some("exit") => {
                self.advance()?;
                Stmt::Exit(self.optional_expression()?)
            }
            Some("return") => {
                self.advance()?;
                Stmt::Return(self.optional_expression()?)
            }
            Some("delete") => {
                self.advance()?;
                let name = match self.advance()? {
                    Token::Name(name) => name,
                    _ => return Err(String::from("delete needs an array")),
                };
                let index = if self.accept("[")? {
                    let index = self.expression_list()?;
                    self.expect("]")?;
                    Some(index)
                } else {
                    None
                };
                Stmt::Delete(name, index)
            }
            _ => self.simple_statement()?,
        };
        self.end_simple()?;
        Ok(statement)
    }

    fn optional_expression(&mut self) -> ParseResult<Option<Expr>> {
        if self.is_op(";") || self.is_op("}") || self.token == Token::Newline || self.token == Token::Eof {
            return Ok(None);
        }
        Ok(Some(self.expression()?))
    }

    fn simple_statement(&mut self) -> ParseResult<Stmt> {
        if self.is_keyword("print") || self.is_keyword("printf") {
            let printf = self.is_keyword("printf");
            self.advance()?;
            let saved = core::mem::replace(&mut self.no_greater, true);
            let args = if self.is_op(";")
                || self.is_op("}")
                || self.is_op(">")
                || self.is_op(">>")
                || self.is_op("|")
                || self.token == Token::Newline
                || self.token == Token::Eof
            {
                Vec::new()
            } else {
                self.expression_list()?
            };
            self.no_greater = saved;
            let args = match args.len() {
                1 => {
                    let mut args = args;
                    match args.pop() {
                        Some(Expr::Group(list)) => list,
                        Some(other) => alloc::vec![other],
                        None => Vec::new(),
                    }
                }
                _ => args,
            };
            if self.is_op("|") {
                return Err(String::from("pipes to commands are not supported"));
            }
            let redirect = if self.accept(">")? {
                Redirect::Write(self.concat_no_redirect()?)
            } else if self.accept(">>")? {
                Redirect::Append(self.concat_no_redirect()?)
            } else {
                Redirect::None
            };
            return Ok(if printf {
                if args.is_empty() {
                    return Err(String::from("printf needs a format"));
                }
                Stmt::Printf(args, redirect)
            } else {
                Stmt::Print(args, redirect)
            });
        }
        Ok(Stmt::Expr(self.expression()?))
    }

    fn concat_no_redirect(&mut self) -> ParseResult<Expr> {
        let saved = core::mem::replace(&mut self.no_greater, true);
        let target = self.concatenation();
        self.no_greater = saved;
        target
    }

    fn for_statement(&mut self) -> ParseResult<Stmt> {
        self.advance()?;
        self.expect("(")?;
        // `for (k in a)` and `for (init; test; step)` share a start.
        let init = if self.is_op(";") {
            None
        } else if let Token::Name(name) = &self.token {
            let name = name.clone();
            let position = self.lexer.position;
            let previous = self.lexer.previous.clone();
            let token = self.token.clone();
            self.advance()?;
            if self.is_keyword("in") {
                self.advance()?;
                let array = match self.advance()? {
                    Token::Name(array) => array,
                    _ => return Err(String::from("expected an array after 'in'")),
                };
                if self.accept(")")? {
                    return Ok(Stmt::ForIn(name, array, self.body_statement()?));
                }
                return Err(String::from("expected ')' in for"));
            }
            self.lexer.position = position;
            self.lexer.previous = previous;
            self.token = token;
            Some(Box::new(self.simple_statement()?))
        } else {
            Some(Box::new(self.simple_statement()?))
        };
        self.expect(";")?;
        self.skip_newlines()?;
        let condition = if self.is_op(";") { None } else { Some(self.expression()?) };
        self.expect(";")?;
        self.skip_newlines()?;
        let step = if self.is_op(")") {
            None
        } else {
            Some(Box::new(self.simple_statement()?))
        };
        self.expect(")")?;
        Ok(Stmt::For(init, condition, step, self.body_statement()?))
    }

    fn expression_list(&mut self) -> ParseResult<Vec<Expr>> {
        let mut list = alloc::vec![self.expression()?];
        while self.accept(",")? {
            self.skip_newlines()?;
            list.push(self.expression()?);
        }
        Ok(list)
    }

    fn expression(&mut self) -> ParseResult<Expr> {
        self.enter()?;
        let expression = self.ternary();
        self.leave();
        expression
    }

    fn ternary(&mut self) -> ParseResult<Expr> {
        let condition = self.or()?;
        if self.accept("?")? {
            self.skip_newlines()?;
            let yes = self.expression()?;
            self.skip_newlines()?;
            self.expect(":")?;
            self.skip_newlines()?;
            let no = self.expression()?;
            return Ok(Expr::Conditional(Box::new(condition), Box::new(yes), Box::new(no)));
        }
        const ASSIGN: &[&str] = &["=", "+=", "-=", "*=", "/=", "%=", "^=", "**="];
        if let Token::Op(op) = self.token {
            if ASSIGN.contains(&op) && matches!(condition, Expr::Var(_) | Expr::Index(..) | Expr::Field(_)) {
                self.advance()?;
                self.skip_newlines()?;
                let value = self.expression()?;
                return Ok(Expr::Assign(op, Box::new(condition), Box::new(value)));
            }
        }
        Ok(condition)
    }

    fn or(&mut self) -> ParseResult<Expr> {
        let mut left = self.and()?;
        while self.accept("||")? {
            self.skip_newlines()?;
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> ParseResult<Expr> {
        let mut left = self.in_expr()?;
        while self.accept("&&")? {
            self.skip_newlines()?;
            left = Expr::And(Box::new(left), Box::new(self.in_expr()?));
        }
        Ok(left)
    }

    fn in_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.match_expr()?;
        while self.is_keyword("in") && !self.no_in {
            self.advance()?;
            let name = match self.advance()? {
                Token::Name(name) => name,
                _ => return Err(String::from("expected an array after 'in'")),
            };
            left = Expr::In(alloc::vec![left], name);
        }
        Ok(left)
    }

    fn match_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.comparison()?;
        loop {
            let negate = if self.is_op("~") {
                false
            } else if self.is_op("!~") {
                true
            } else {
                return Ok(left);
            };
            self.advance()?;
            let right = self.comparison()?;
            let right = match right {
                Expr::Match(text) => Expr::Regex(text),
                other => other,
            };
            left = Expr::MatchOp(negate, Box::new(left), Box::new(right));
        }
    }

    fn comparison(&mut self) -> ParseResult<Expr> {
        let left = self.concatenation()?;
        let op = match self.token {
            Token::Op(op @ ("<" | "<=" | "==" | "!=" | ">=")) => op,
            Token::Op(">") if !self.no_greater => ">",
            _ => return Ok(left),
        };
        self.advance()?;
        let right = self.concatenation()?;
        Ok(Expr::Compare(op, Box::new(left), Box::new(right)))
    }

    fn starts_operand(&self) -> bool {
        match &self.token {
            Token::Number(_) | Token::Str(_) | Token::Regex(_) | Token::Name(_)
            | Token::FuncName(_) | Token::Builtin(_) => true,
            Token::Op(op) => matches!(*op, "$" | "!" | "(" | "-" | "+" | "++" | "--"),
            _ => false,
        }
    }

    fn concatenation(&mut self) -> ParseResult<Expr> {
        let mut left = self.additive()?;
        while self.starts_operand() && !self.is_op("+") && !self.is_op("-") {
            let right = self.additive()?;
            left = Expr::Concat(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn additive(&mut self) -> ParseResult<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.token {
                Token::Op(op @ ("+" | "-")) => op,
                _ => return Ok(left),
            };
            self.advance()?;
            left = Expr::Binary(op, Box::new(left), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> ParseResult<Expr> {
        let mut left = self.unary()?;
        loop {
            let op = match self.token {
                Token::Op(op @ ("*" | "/" | "%")) => op,
                _ => return Ok(left),
            };
            self.advance()?;
            left = Expr::Binary(op, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> ParseResult<Expr> {
        self.enter()?;
        let result = if self.accept("!")? {
            self.unary().map(|inner| Expr::Not(Box::new(inner)))
        } else if self.accept("-")? {
            self.unary().map(|inner| Expr::Negate(Box::new(inner)))
        } else if self.accept("+")? {
            self.unary().map(|inner| Expr::Plus(Box::new(inner)))
        } else {
            self.power()
        };
        self.leave();
        result
    }

    fn power(&mut self) -> ParseResult<Expr> {
        let base = self.postfix()?;
        if self.is_op("^") || self.is_op("**") {
            self.advance()?;
            // Right associative, and binds tighter than unary minus on its left.
            self.enter()?;
            let exponent = self.unary();
            self.leave();
            return Ok(Expr::Binary("^", Box::new(base), Box::new(exponent?)));
        }
        Ok(base)
    }

    fn postfix(&mut self) -> ParseResult<Expr> {
        let target = self.primary()?;
        if matches!(target, Expr::Var(_) | Expr::Index(..) | Expr::Field(_)) {
            if self.accept("++")? {
                return Ok(Expr::Step {
                    target: Box::new(target),
                    delta: 1.0,
                    prefix: false,
                });
            }
            if self.accept("--")? {
                return Ok(Expr::Step {
                    target: Box::new(target),
                    delta: -1.0,
                    prefix: false,
                });
            }
        }
        Ok(target)
    }

    fn primary(&mut self) -> ParseResult<Expr> {
        let token = self.token.clone();
        match token {
            Token::Number(value) => {
                self.advance()?;
                Ok(Expr::Number(value))
            }
            Token::Str(text) => {
                self.advance()?;
                Ok(Expr::Str(text))
            }
            Token::Regex(text) => {
                self.advance()?;
                Ok(Expr::Match(text))
            }
            Token::Name(name) => {
                self.advance()?;
                if self.accept("[")? {
                    let saved = core::mem::replace(&mut self.no_greater, false);
                    let index = self.expression_list()?;
                    self.no_greater = saved;
                    self.expect("]")?;
                    return Ok(Expr::Index(name, index));
                }
                Ok(Expr::Var(name))
            }
            Token::FuncName(name) => {
                self.advance()?;
                self.expect("(")?;
                let args = self.call_args()?;
                Ok(Expr::Call(name, args))
            }
            Token::Builtin(name) => {
                self.advance()?;
                if self.accept("(")? {
                    let mut args = self.call_args()?;
                    // A bare /re/ here names the pattern; it doesn't match $0.
                    let pattern = match name.as_str() {
                        "sub" | "gsub" => Some(0),
                        "match" => Some(1),
                        "split" => Some(2),
                        _ => None,
                    };
                    if let Some(arg) = pattern.and_then(|index| args.get_mut(index)) {
                        if let Expr::Match(text) = arg {
                            *arg = Expr::Regex(core::mem::take(text));
                        }
                    }
                    return Ok(Expr::Builtin(name, args));
                }
                if name == "length" {
                    return Ok(Expr::Builtin(name, Vec::new()));
                }
                Err(format!("{name} needs parentheses"))
            }
            Token::Keyword("getline") => {
                self.advance()?;
                let target = if matches!(&self.token, Token::Name(_)) || self.is_op("$") {
                    Some(Box::new(self.postfix()?))
                } else {
                    None
                };
                let file = if self.accept("<")? {
                    Some(Box::new(self.postfix()?))
                } else {
                    None
                };
                Ok(Expr::Getline(target, file))
            }
            Token::Op("$") => {
                self.advance()?;
                self.enter()?;
                let field = if self.accept("++")? || self.accept("--")? {
                    Err(String::from("'$++' is not supported"))
                } else if self.accept("-")? {
                    self.primary().map(|inner| Expr::Negate(Box::new(inner)))
                } else {
                    self.primary()
                };
                self.leave();
                Ok(Expr::Field(Box::new(field?)))
            }
            Token::Op(op @ ("++" | "--")) => {
                self.advance()?;
                self.enter()?;
                let target = self.primary();
                self.leave();
                let target = target?;
                if !matches!(target, Expr::Var(_) | Expr::Index(..) | Expr::Field(_)) {
                    return Err(format!("{op} needs a variable"));
                }
                Ok(Expr::Step {
                    target: Box::new(target),
                    delta: if op == "++" { 1.0 } else { -1.0 },
                    prefix: true,
                })
            }
            Token::Op("(") => {
                self.advance()?;
                self.enter()?;
                let saved = (
                    core::mem::replace(&mut self.no_greater, false),
                    core::mem::replace(&mut self.no_in, false),
                );
                let first = self.expression();
                let result = first.and_then(|first| {
                    if self.is_op(",") {
                        let mut list = alloc::vec![first];
                        while self.accept(",")? {
                            self.skip_newlines()?;
                            list.push(self.expression()?);
                        }
                        self.expect(")")?;
                        if !self.is_keyword("in") {
                            // `print (a, b)`: only print can use this.
                            return Ok(Expr::Group(list));
                        }
                        self.advance()?;
                        let name = match self.advance()? {
                            Token::Name(name) => name,
                            _ => return Err(String::from("expected an array after 'in'")),
                        };
                        Ok(Expr::In(list, name))
                    } else {
                        self.expect(")")?;
                        // A group is not an lvalue, so `(a) = 1` stays an error.
                        Ok(Expr::Group(alloc::vec![first]))
                    }
                });
                (self.no_greater, self.no_in) = saved;
                self.leave();
                result
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }

    fn call_args(&mut self) -> ParseResult<Vec<Expr>> {
        let saved = core::mem::replace(&mut self.no_greater, false);
        self.skip_newlines()?;
        let mut args = Vec::new();
        while !self.is_op(")") {
            args.push(self.expression()?);
            self.skip_newlines()?;
            if !self.accept(",")? {
                break;
            }
            self.skip_newlines()?;
        }
        self.no_greater = saved;
        self.expect(")")?;
        Ok(args)
    }
}

// ------------------------------------------------------------ interpreter ---

/// Something that interrupts ordinary evaluation.
enum Stop {
    Next,
    NextFile,
    Exit,
    Break,
    Continue,
    Return(Value),
    Error(String),
}

type Eval<T> = Result<T, Stop>;

fn fail<T>(message: impl Into<String>) -> Eval<T> {
    Err(Stop::Error(message.into()))
}

/// What a name holds.
#[derive(Clone)]
enum Cell {
    Scalar(Value),
    Array(usize),
    /// Passed uninitialized to a function, which may yet use it as an array.
    Lazy(usize),
}

/// An input file, read as records.
struct Source {
    data: Vec<u8>,
    position: usize,
    /// More can come from standard input.
    stdin: bool,
}

impl Source {
    fn file(data: Vec<u8>) -> Self {
        Self {
            data,
            position: 0,
            stdin: false,
        }
    }

    fn standard_input() -> Self {
        Self {
            data: Vec::new(),
            position: 0,
            stdin: true,
        }
    }

    /// Reads more of standard input. Returns `false` at its end.
    fn fill(&mut self, ctx: &mut Ctx<'_>) -> bool {
        if !self.stdin {
            return false;
        }
        if self.position > 0 {
            self.data.drain(..self.position);
            self.position = 0;
        }
        if self.data.len() >= MAX_STREAM_BYTES {
            self.stdin = false;
            return false;
        }
        let mut chunk = [0u8; CHUNK];
        let count = ctx.stdin.read(&mut chunk);
        if count == 0 || self.data.try_reserve(count).is_err() {
            self.stdin = false;
            return false;
        }
        self.data.extend_from_slice(&chunk[..count]);
        true
    }

    /// The next record, split on `separator`; paragraph mode when it is empty.
    fn next_record(&mut self, separator: &str, ctx: &mut Ctx<'_>) -> Option<String> {
        let mut searched = 0usize;
        loop {
            let rest = self.data.get(self.position..).unwrap_or(&[]);
            if separator.is_empty() {
                // Blank lines separate records, and any number of them count as one.
                let skip = rest.iter().take_while(|byte| **byte == b'\n').count();
                let body = &rest[skip..];
                if let Some(end) = body.windows(2).position(|pair| pair == b"\n\n") {
                    let text = String::from_utf8_lossy(&body[..end]).into_owned();
                    self.position += skip + end + 2;
                    return Some(text);
                }
            } else {
                let byte = separator.as_bytes().first().copied().unwrap_or(b'\n');
                if let Some(end) = rest.iter().skip(searched).position(|found| *found == byte) {
                    let end = end + searched;
                    let text = String::from_utf8_lossy(&rest[..end]).into_owned();
                    self.position += end + 1;
                    return Some(text);
                }
                searched = rest.len();
            }
            let before = self.position;
            if !self.fill(ctx) {
                let rest = self.data.get(self.position..).unwrap_or(&[]);
                let rest = if separator.is_empty() {
                    let skip = rest.iter().take_while(|byte| **byte == b'\n').count();
                    let body = &rest[skip..];
                    body.strip_suffix(b"\n").unwrap_or(body)
                } else {
                    rest
                };
                if rest.is_empty() {
                    return None;
                }
                let text = String::from_utf8_lossy(rest).into_owned();
                self.position = self.data.len();
                return Some(text);
            }
            searched = searched.saturating_sub(before.min(searched));
            if before > 0 {
                searched = 0;
            }
        }
    }
}

struct Interp<'p, 'c, 'x> {
    ctx: &'c mut Ctx<'x>,
    functions: &'p BTreeMap<String, Function>,
    globals: BTreeMap<String, Cell>,
    frames: Vec<BTreeMap<String, Cell>>,
    arrays: Vec<BTreeMap<String, Value>>,
    regexes: BTreeMap<String, Regex>,
    record: String,
    fields: Vec<String>,
    fields_stale: bool,
    record_stale: bool,
    nr: f64,
    fnr: f64,
    files: Vec<String>,
    next_file: usize,
    input: Option<Source>,
    used_a_file: bool,
    readers: BTreeMap<String, Source>,
    outputs: BTreeMap<String, Vec<u8>>,
    exit_code: i32,
    depth: usize,
    calls: usize,
    seed: u64,
    prev_seed: f64,
    ticks: u32,
}

impl Interp<'_, '_, '_> {
    fn var(&self, name: &str) -> Option<&Cell> {
        self.frames
            .last()
            .and_then(|frame| frame.get(name))
            .or_else(|| self.globals.get(name))
    }

    fn special_value(&mut self, name: &str) -> Option<Value> {
        match name {
            "NF" => {
                self.split_fields();
                Some(Value::Num(self.fields.len() as f64))
            }
            "NR" => Some(Value::Num(self.nr)),
            "FNR" => Some(Value::Num(self.fnr)),
            _ => None,
        }
    }

    fn get_var(&mut self, name: &str) -> Eval<Value> {
        if let Some(value) = self.special_value(name) {
            return Ok(value);
        }
        match self.var(name) {
            None | Some(Cell::Lazy(_)) => Ok(Value::Uninit),
            Some(Cell::Scalar(value)) => Ok(value.clone()),
            Some(Cell::Array(_)) => fail(format!("{name} is an array")),
        }
    }

    fn set_var(&mut self, name: &str, value: Value) -> Eval<()> {
        match name {
            "NF" => {
                self.set_nf(value.num());
                return Ok(());
            }
            "NR" => {
                self.nr = value.num();
                return Ok(());
            }
            "FNR" => {
                self.fnr = value.num();
                return Ok(());
            }
            _ => {}
        }
        let scope = match self.frames.last_mut() {
            Some(frame) if frame.contains_key(name) => frame,
            _ => &mut self.globals,
        };
        if let Some(Cell::Array(_)) = scope.get(name) {
            return fail(format!("{name} is an array"));
        }
        scope.insert(name.to_string(), Cell::Scalar(value));
        Ok(())
    }

    fn new_array(&mut self) -> Eval<usize> {
        if self.arrays.len() >= MAX_ARRAYS {
            return fail("too many arrays");
        }
        self.arrays.push(BTreeMap::new());
        Ok(self.arrays.len() - 1)
    }

    /// The array behind a name, created on first use.
    fn array_id(&mut self, name: &str) -> Eval<usize> {
        match self.var(name) {
            Some(Cell::Array(id) | Cell::Lazy(id)) => Ok(*id),
            Some(Cell::Scalar(Value::Uninit)) | None => {
                let id = self.new_array()?;
                let scope = match self.frames.last_mut() {
                    Some(frame) if frame.contains_key(name) => frame,
                    _ => &mut self.globals,
                };
                scope.insert(name.to_string(), Cell::Array(id));
                Ok(id)
            }
            Some(Cell::Scalar(_)) => fail(format!("{name} is not an array")),
        }
    }

    fn string_var(&self, name: &str, default: &str) -> String {
        match self.globals.get(name) {
            Some(Cell::Scalar(value)) => self.to_text_with(value, "%.6g"),
            _ => default.to_string(),
        }
    }

    fn to_text_with(&self, value: &Value, format_spec: &str) -> String {
        match value {
            Value::Uninit => String::new(),
            Value::Num(number) => number_text(*number, format_spec),
            Value::Str(text) | Value::StrNum(text, _) => text.clone(),
        }
    }

    fn to_text(&self, value: &Value) -> String {
        if matches!(value, Value::Num(_)) {
            let convfmt = self.string_var("CONVFMT", "%.6g");
            return self.to_text_with(value, &convfmt);
        }
        self.to_text_with(value, "%.6g")
    }

    fn regex(&mut self, pattern: &str) -> Eval<&Regex> {
        if !self.regexes.contains_key(pattern) {
            // Programs that build patterns in a loop can't grow this forever.
            if self.regexes.len() >= 256 {
                self.regexes.clear();
            }
            match Regex::new(pattern, Syntax::Extended) {
                Ok(regex) => {
                    self.regexes.insert(pattern.to_string(), regex);
                }
                Err(message) => return fail(format!("bad regular expression /{pattern}/: {message}")),
            }
        }
        match self.regexes.get(pattern) {
            Some(regex) => Ok(regex),
            None => fail("regular expression cache"),
        }
    }

    // Records and fields ------------------------------------------------------

    fn set_record(&mut self, text: String) {
        self.record = text;
        self.fields_stale = true;
        self.record_stale = false;
    }

    fn split_text(&mut self, text: &str, separator: &str) -> Eval<Vec<String>> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        if separator == " " {
            return Ok(text.split_ascii_whitespace().map(ToString::to_string).collect());
        }
        if separator.is_empty() {
            return Ok(text.chars().map(|ch| ch.to_string()).collect());
        }
        let mut chars = separator.chars();
        if let (Some(only), None) = (chars.next(), chars.next()) {
            if only != '\\' {
                return Ok(text.split(only).map(ToString::to_string).collect());
            }
        }
        let regex = self.regex(separator)?;
        let bytes = text.as_bytes();
        let mut parts = Vec::new();
        let mut start = 0;
        let mut from = 0;
        while let Some(found) = regex.find_at(bytes, from) {
            let (begin, end) = found.span();
            if end == begin {
                from = end + 1;
                if from > bytes.len() {
                    break;
                }
                continue;
            }
            parts.push(String::from_utf8_lossy(&bytes[start..begin]).into_owned());
            start = end;
            from = end;
        }
        parts.push(String::from_utf8_lossy(&bytes[start..]).into_owned());
        Ok(parts)
    }

    fn split_fields(&mut self) {
        if !self.fields_stale {
            return;
        }
        self.fields_stale = false;
        let separator = self.string_var("FS", " ");
        let record = core::mem::take(&mut self.record);
        // A bad FS was already reported when it could be; here it just won't split.
        self.fields = self.split_text(&record, &separator).unwrap_or_default();
        self.record = record;
    }

    fn rebuild_record(&mut self) {
        if !self.record_stale {
            return;
        }
        self.record_stale = false;
        let separator = self.string_var("OFS", " ");
        self.record = self.fields.join(separator.as_str());
    }

    fn set_nf(&mut self, count: f64) {
        self.split_fields();
        let count = (count.max(0.0) as usize).min(1 << 20);
        self.fields.resize(count, String::new());
        self.record_stale = true;
    }

    fn get_field(&mut self, index: f64) -> Eval<Value> {
        if index < 0.0 {
            return fail("negative field index");
        }
        if index == 0.0 {
            self.rebuild_record();
            return Ok(Value::input(&self.record.clone()));
        }
        self.split_fields();
        Ok(self
            .fields
            .get(index as usize - 1)
            .map_or(Value::Uninit, |field| Value::input(field)))
    }

    fn set_field(&mut self, index: f64, value: &str) -> Eval<()> {
        if index < 0.0 {
            return fail("negative field index");
        }
        if index == 0.0 {
            self.set_record(value.to_string());
            return Ok(());
        }
        let index = index as usize;
        if index > 1 << 20 {
            return fail("field index is too large");
        }
        self.split_fields();
        if self.fields.len() < index {
            self.fields.resize(index, String::new());
        }
        self.fields[index - 1] = value.to_string();
        self.record_stale = true;
        Ok(())
    }

    /// Opens the next operand. Assignments among them apply as they're reached.
    fn advance_input(&mut self) -> Eval<bool> {
        while self.next_file < self.files.len() {
            let operand = self.files[self.next_file].clone();
            self.next_file += 1;
            if let Some((name, value)) = split_assignment(&operand) {
                self.set_var(name, Value::input(&unescape(value)))?;
                continue;
            }
            self.used_a_file = true;
            self.set_var("FILENAME", Value::Str(operand.clone()))?;
            self.fnr = 0.0;
            if operand == "-" {
                self.input = Some(Source::standard_input());
                return Ok(true);
            }
            match self.ctx.fs.read_file(&operand) {
                Ok(data) => {
                    self.input = Some(Source::file(data));
                    return Ok(true);
                }
                Err(error) => {
                    let message = format!("awk: {operand}: {}\n", error.text());
                    self.ctx.stderr.write(message.as_bytes());
                    self.exit_code = 2;
                }
            }
        }
        if !self.used_a_file {
            self.used_a_file = true;
            self.input = Some(Source::standard_input());
            return Ok(true);
        }
        Ok(false)
    }

    /// The next main-input record, moving through files as they end.
    fn next_main_record(&mut self) -> Eval<Option<String>> {
        let separator = self.string_var("RS", "\n");
        loop {
            if self.input.is_none() && !self.advance_input()? {
                return Ok(None);
            }
            let Some(mut source) = self.input.take() else {
                return Ok(None);
            };
            match source.next_record(&separator, self.ctx) {
                Some(record) => {
                    self.input = Some(source);
                    return Ok(Some(record));
                }
                None => self.input = None,
            }
        }
    }

    // Statements --------------------------------------------------------------

    fn poll(&mut self) -> Eval<()> {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks % 1024 == 0 && self.ctx.stdout.is_broken() {
            self.exit_code = 130;
            return Err(Stop::Exit);
        }
        Ok(())
    }

    fn run_block(&mut self, body: &[Stmt]) -> Eval<()> {
        for statement in body {
            self.run(statement)?;
        }
        Ok(())
    }

    fn run(&mut self, statement: &Stmt) -> Eval<()> {
        match statement {
            Stmt::Expr(expression) => {
                self.eval(expression)?;
            }
            Stmt::Print(args, redirect) => {
                let mut line = String::new();
                if args.is_empty() {
                    self.rebuild_record();
                    line.push_str(&self.record);
                } else {
                    let separator = self.string_var("OFS", " ");
                    let ofmt = self.string_var("OFMT", "%.6g");
                    for (index, arg) in args.iter().enumerate() {
                        if index > 0 {
                            line.push_str(&separator);
                        }
                        let value = self.eval(arg)?;
                        line.push_str(&self.to_text_with(&value, &ofmt));
                    }
                }
                line.push_str(&self.string_var("ORS", "\n"));
                self.output(redirect, line.as_bytes())?;
            }
            Stmt::Printf(args, redirect) => {
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.eval(arg)?);
                }
                let format_text = self.to_text(&values[0]);
                let text = sprintf(&format_text, &values[1..]).or_else(fail)?;
                self.output(redirect, text.as_bytes())?;
            }
            Stmt::If(condition, yes, no) => {
                if self.eval(condition)?.truth() {
                    self.run(yes)?;
                } else if let Some(no) = no {
                    self.run(no)?;
                }
            }
            Stmt::While(condition, body) => {
                while self.eval(condition)?.truth() {
                    self.poll()?;
                    match self.run(body) {
                        Err(Stop::Break) => break,
                        Err(Stop::Continue) | Ok(()) => {}
                        Err(other) => return Err(other),
                    }
                }
            }
            Stmt::DoWhile(body, condition) => loop {
                self.poll()?;
                match self.run(body) {
                    Err(Stop::Break) => break,
                    Err(Stop::Continue) | Ok(()) => {}
                    Err(other) => return Err(other),
                }
                if !self.eval(condition)?.truth() {
                    break;
                }
            },
            Stmt::For(init, condition, step, body) => {
                if let Some(init) = init {
                    self.run(init)?;
                }
                loop {
                    self.poll()?;
                    if let Some(condition) = condition {
                        if !self.eval(condition)?.truth() {
                            break;
                        }
                    }
                    match self.run(body) {
                        Err(Stop::Break) => break,
                        Err(Stop::Continue) | Ok(()) => {}
                        Err(other) => return Err(other),
                    }
                    if let Some(step) = step {
                        self.run(step)?;
                    }
                }
            }
            Stmt::ForIn(variable, array, body) => {
                let id = self.array_id(array)?;
                let keys: Vec<String> = self.arrays[id].keys().cloned().collect();
                for key in keys {
                    self.poll()?;
                    // A key deleted during the loop is skipped.
                    if !self.arrays[id].contains_key(&key) {
                        continue;
                    }
                    self.set_var(variable, Value::input(&key))?;
                    match self.run(body) {
                        Err(Stop::Break) => break,
                        Err(Stop::Continue) | Ok(()) => {}
                        Err(other) => return Err(other),
                    }
                }
            }
            Stmt::Block(body) => self.run_block(body)?,
            Stmt::Next => return Err(Stop::Next),
            Stmt::NextFile => return Err(Stop::NextFile),
            Stmt::Exit(code) => {
                if let Some(code) = code {
                    self.exit_code = self.eval(code)?.num() as i32;
                }
                return Err(Stop::Exit);
            }
            Stmt::Return(value) => {
                let value = match value {
                    Some(value) => self.eval(value)?,
                    None => Value::Uninit,
                };
                return Err(Stop::Return(value));
            }
            Stmt::Break => return Err(Stop::Break),
            Stmt::Continue => return Err(Stop::Continue),
            Stmt::Delete(name, index) => {
                let id = self.array_id(name)?;
                match index {
                    Some(index) => {
                        let key = self.subscript(index)?;
                        self.arrays[id].remove(&key);
                    }
                    None => self.arrays[id].clear(),
                }
            }
        }
        Ok(())
    }

    fn output(&mut self, redirect: &Redirect, bytes: &[u8]) -> Eval<()> {
        let (target, append) = match redirect {
            Redirect::None => {
                self.ctx.stdout.write(bytes);
                return Ok(());
            }
            Redirect::Write(target) => (target, false),
            Redirect::Append(target) => (target, true),
        };
        let value = self.eval(target)?;
        let path = self.to_text(&value);
        match path.as_str() {
            "/dev/stdout" | "-" => {
                self.ctx.stdout.write(bytes);
                return Ok(());
            }
            "/dev/stderr" => {
                self.ctx.stderr.write(bytes);
                return Ok(());
            }
            "" => return fail("empty output file name"),
            _ => {}
        }
        if !self.outputs.contains_key(&path) {
            if let Err(error) = self.ctx.fs.write_file(&path, b"", append) {
                return fail(format!("{path}: {}", error.text()));
            }
            self.outputs.insert(path.clone(), Vec::new());
        }
        let full = self.outputs.get(&path).is_some_and(|data| data.len() >= CHUNK);
        if let Some(data) = self.outputs.get_mut(&path) {
            data.extend_from_slice(bytes);
        }
        if full {
            self.flush_output(&path)?;
        }
        Ok(())
    }

    fn flush_output(&mut self, path: &str) -> Eval<()> {
        if let Some(data) = self.outputs.get_mut(path) {
            let data = core::mem::take(data);
            if let Err(error) = self.ctx.fs.write_file(path, &data, true) {
                return fail(format!("{path}: {}", error.text()));
            }
        }
        Ok(())
    }

    fn flush_all(&mut self) -> Eval<()> {
        let paths: Vec<String> = self.outputs.keys().cloned().collect();
        for path in paths {
            self.flush_output(&path)?;
        }
        Ok(())
    }

    // Expressions -------------------------------------------------------------

    fn subscript(&mut self, index: &[Expr]) -> Eval<String> {
        let mut key = String::new();
        for (position, part) in index.iter().enumerate() {
            if position > 0 {
                key.push_str(&self.string_var("SUBSEP", "\x1c"));
            }
            let value = self.eval(part)?;
            key.push_str(&self.to_text(&value));
        }
        Ok(key)
    }

    fn eval(&mut self, expression: &Expr) -> Eval<Value> {
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            return fail("expression is nested too deeply");
        }
        let result = self.eval_inner(expression);
        self.depth -= 1;
        result
    }

    fn assign_to(&mut self, target: &Expr, value: Value) -> Eval<()> {
        match target {
            Expr::Var(name) => self.set_var(name, value),
            Expr::Index(name, index) => {
                let key = self.subscript(index)?;
                let id = self.array_id(name)?;
                self.arrays[id].insert(key, value);
                Ok(())
            }
            Expr::Field(index) => {
                let index = self.eval(index)?.num();
                let text = self.to_text(&value);
                self.set_field(index, &text)
            }
            _ => fail("assignment to something that isn't a variable"),
        }
    }

    fn eval_inner(&mut self, expression: &Expr) -> Eval<Value> {
        Ok(match expression {
            Expr::Number(value) => Value::Num(*value),
            Expr::Str(text) => Value::Str(text.clone()),
            Expr::Regex(text) => Value::Str(text.clone()),
            Expr::Match(pattern) => {
                self.rebuild_record();
                let record = core::mem::take(&mut self.record);
                let found = self.regex(pattern).map(|regex| regex.is_match(record.as_bytes()));
                self.record = record;
                Value::Num(if found? { 1.0 } else { 0.0 })
            }
            Expr::Field(index) => {
                let index = self.eval(index)?.num();
                self.get_field(index)?
            }
            Expr::Var(name) => self.get_var(name)?,
            Expr::Index(name, index) => {
                let key = self.subscript(index)?;
                let id = self.array_id(name)?;
                self.arrays[id].entry(key).or_insert(Value::Uninit).clone()
            }
            Expr::Assign(op, target, value) => {
                let right = self.eval(value)?;
                let result = if *op == "=" {
                    // Plain copies keep a strnum's numeric look.
                    right
                } else {
                    let left = self.eval(target)?.num();
                    let operator = match *op {
                        "+=" => "+",
                        "-=" => "-",
                        "*=" => "*",
                        "/=" => "/",
                        "%=" => "%",
                        _ => "^",
                    };
                    Value::Num(arithmetic(operator, left, right.num())?)
                };
                self.assign_to(target, result.clone())?;
                result
            }
            Expr::Conditional(condition, yes, no) => {
                if self.eval(condition)?.truth() {
                    self.eval(yes)?
                } else {
                    self.eval(no)?
                }
            }
            Expr::And(left, right) => {
                let truth = self.eval(left)?.truth() && self.eval(right)?.truth();
                Value::Num(f64::from(u8::from(truth)))
            }
            Expr::Or(left, right) => {
                let truth = self.eval(left)?.truth() || self.eval(right)?.truth();
                Value::Num(f64::from(u8::from(truth)))
            }
            Expr::In(index, name) => {
                let key = self.subscript(index)?;
                let id = self.array_id(name)?;
                Value::Num(f64::from(u8::from(self.arrays[id].contains_key(&key))))
            }
            Expr::MatchOp(negate, left, right) => {
                let value = self.eval(left)?;
                let text = self.to_text(&value);
                let pattern = self.eval(right)?;
                let pattern = self.to_text(&pattern);
                let found = self.regex(&pattern)?.is_match(text.as_bytes());
                Value::Num(f64::from(u8::from(found != *negate)))
            }
            Expr::Compare(op, left, right) => {
                let left = self.eval(left)?;
                let right = self.eval(right)?;
                let ordering = if left.is_string() || right.is_string() {
                    self.to_text(&left).cmp(&self.to_text(&right))
                } else {
                    left.num()
                        .partial_cmp(&right.num())
                        .unwrap_or(core::cmp::Ordering::Greater)
                };
                use core::cmp::Ordering::{Equal, Greater, Less};
                let truth = match *op {
                    "<" => ordering == Less,
                    "<=" => ordering != Greater,
                    "==" => ordering == Equal,
                    "!=" => ordering != Equal,
                    ">=" => ordering != Less,
                    _ => ordering == Greater,
                };
                Value::Num(f64::from(u8::from(truth)))
            }
            Expr::Concat(left, right) => {
                let left = self.eval(left)?;
                let mut text = self.to_text(&left);
                let right = self.eval(right)?;
                let right = self.to_text(&right);
                if text.len().saturating_add(right.len()) > MAX_STREAM_BYTES {
                    return fail("string is too long");
                }
                text.push_str(&right);
                Value::Str(text)
            }
            Expr::Binary(op, left, right) => {
                let left = self.eval(left)?.num();
                let right = self.eval(right)?.num();
                Value::Num(arithmetic(op, left, right)?)
            }
            Expr::Negate(inner) => Value::Num(-self.eval(inner)?.num()),
            Expr::Plus(inner) => Value::Num(self.eval(inner)?.num()),
            Expr::Not(inner) => {
                let truth = self.eval(inner)?.truth();
                Value::Num(f64::from(u8::from(!truth)))
            }
            Expr::Step {
                target,
                delta,
                prefix,
            } => {
                let old = self.eval(target)?.num();
                self.assign_to(target, Value::Num(old + delta))?;
                Value::Num(if *prefix { old + delta } else { old })
            }
            Expr::Call(name, args) => self.call(name, args)?,
            Expr::Builtin(name, args) => self.builtin(name, args)?,
            Expr::Getline(target, file) => self.getline(target.as_deref(), file.as_deref())?,
            Expr::Group(list) => match list.as_slice() {
                [only] => self.eval(only)?,
                _ => return fail("a parenthesised list only works with print"),
            },
        })
    }

    fn call(&mut self, name: &str, args: &[Expr]) -> Eval<Value> {
        let functions = self.functions;
        let Some(function) = functions.get(name) else {
            return fail(format!("function {name} is not defined"));
        };
        if args.len() > function.params.len() {
            return fail(format!("{name}: too many arguments"));
        }
        if self.calls >= MAX_CALL_DEPTH {
            return fail(format!("{name}: calls are nested too deeply"));
        }

        let mut frame = BTreeMap::new();
        for (param, arg) in function.params.iter().zip(args) {
            let cell = match arg {
                // An array goes by reference, and an unset name might be one.
                Expr::Var(variable) => match self.var(variable).cloned() {
                    Some(cell @ (Cell::Array(_) | Cell::Lazy(_))) => cell,
                    Some(Cell::Scalar(Value::Uninit)) | None
                        if !matches!(variable.as_str(), "NF" | "NR" | "FNR") =>
                    {
                        let id = self.new_array()?;
                        let scope = match self.frames.last_mut() {
                            Some(frame) if frame.contains_key(variable) => frame,
                            _ => &mut self.globals,
                        };
                        scope.insert(variable.clone(), Cell::Lazy(id));
                        Cell::Lazy(id)
                    }
                    _ => Cell::Scalar(self.eval(arg)?),
                },
                _ => Cell::Scalar(self.eval(arg)?),
            };
            frame.insert(param.clone(), cell);
        }
        for param in function.params.iter().skip(args.len()) {
            frame.insert(param.clone(), Cell::Scalar(Value::Uninit));
        }

        self.frames.push(frame);
        self.calls += 1;
        let result = self.run_block(&function.body);
        self.calls -= 1;
        self.frames.pop();
        match result {
            Ok(()) => Ok(Value::Uninit),
            Err(Stop::Return(value)) => Ok(value),
            Err(other) => Err(other),
        }
    }

    fn text_arg(&mut self, args: &[Expr], index: usize) -> Eval<String> {
        match args.get(index) {
            Some(arg) => {
                let value = self.eval(arg)?;
                Ok(self.to_text(&value))
            }
            None => fail("missing argument"),
        }
    }

    fn num_arg(&mut self, args: &[Expr], index: usize) -> Eval<f64> {
        match args.get(index) {
            Some(arg) => Ok(self.eval(arg)?.num()),
            None => fail("missing argument"),
        }
    }

    fn builtin(&mut self, name: &str, args: &[Expr]) -> Eval<Value> {
        Ok(match name {
            "length" => match args.first() {
                None => {
                    self.rebuild_record();
                    Value::Num(self.record.chars().count() as f64)
                }
                Some(Expr::Var(variable))
                    if matches!(self.var(variable), Some(Cell::Array(_))) =>
                {
                    let id = self.array_id(variable)?;
                    Value::Num(self.arrays[id].len() as f64)
                }
                Some(_) => Value::Num(self.text_arg(args, 0)?.chars().count() as f64),
            },
            "substr" => {
                let text = self.text_arg(args, 0)?;
                let start = round(self.num_arg(args, 1)?);
                let length = match args.get(2) {
                    Some(_) => round(self.num_arg(args, 2)?),
                    None => f64::INFINITY,
                };
                let end = start + length;
                let first = start.max(1.0);
                if end <= first || first.is_nan() || end.is_nan() {
                    Value::Str(String::new())
                } else {
                    let taken = end - first;
                    let taken = if taken > usize::MAX as f64 { usize::MAX } else { taken as usize };
                    let part: String = text.chars().skip(first as usize - 1).take(taken).collect();
                    Value::Str(part)
                }
            }
            "index" => {
                let text = self.text_arg(args, 0)?;
                let needle = self.text_arg(args, 1)?;
                match text.find(&needle) {
                    Some(at) => Value::Num((text[..at].chars().count() + 1) as f64),
                    None => Value::Num(0.0),
                }
            }
            "split" => {
                let text = self.text_arg(args, 0)?;
                let Some(Expr::Var(array)) = args.get(1) else {
                    return fail("split needs an array");
                };
                let separator = match args.get(2) {
                    Some(_) => self.text_arg(args, 2)?,
                    None => self.string_var("FS", " "),
                };
                let parts = self.split_text(&text, &separator)?;
                let id = self.array_id(array)?;
                let count = parts.len();
                self.arrays[id].clear();
                for (index, part) in parts.into_iter().enumerate() {
                    let value = Value::input(&part);
                    self.arrays[id].insert((index + 1).to_string(), value);
                }
                Value::Num(count as f64)
            }
            "sub" | "gsub" => return self.substitute(name == "gsub", args),
            "match" => {
                let text = self.text_arg(args, 0)?;
                let pattern = self.text_arg(args, 1)?;
                let found = self.regex(&pattern)?.find_at(text.as_bytes(), 0).map(|m| m.span());
                let (start, length) = match found {
                    Some((begin, end)) => (
                        (String::from_utf8_lossy(&text.as_bytes()[..begin]).chars().count() + 1) as f64,
                        String::from_utf8_lossy(&text.as_bytes()[begin..end]).chars().count() as f64,
                    ),
                    None => (0.0, -1.0),
                };
                self.set_var("RSTART", Value::Num(start))?;
                self.set_var("RLENGTH", Value::Num(length))?;
                Value::Num(start)
            }
            "sprintf" => {
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.eval(arg)?);
                }
                let Some(first) = values.first() else {
                    return fail("sprintf needs a format");
                };
                let format_text = self.to_text(first);
                Value::Str(sprintf(&format_text, &values[1..]).or_else(fail)?)
            }
            "tolower" => Value::Str(self.text_arg(args, 0)?.to_lowercase()),
            "toupper" => Value::Str(self.text_arg(args, 0)?.to_uppercase()),
            "int" => Value::Num(truncate(self.num_arg(args, 0)?)),
            "sqrt" => Value::Num(sqrt(self.num_arg(args, 0)?)),
            "exp" => Value::Num(exp(self.num_arg(args, 0)?)),
            "log" => Value::Num(log(self.num_arg(args, 0)?)),
            "sin" => Value::Num(sin(self.num_arg(args, 0)?)),
            "cos" => Value::Num(cos(self.num_arg(args, 0)?)),
            "atan2" => {
                let y = self.num_arg(args, 0)?;
                let x = self.num_arg(args, 1)?;
                Value::Num(atan2(y, x))
            }
            "rand" => {
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 7;
                self.seed ^= self.seed << 17;
                Value::Num((self.seed >> 11) as f64 / (1u64 << 53) as f64)
            }
            "srand" => {
                let seed = match args.first() {
                    Some(_) => self.num_arg(args, 0)?,
                    None => self.ctx.system.uptime_ms() as f64,
                };
                let previous = self.prev_seed;
                self.prev_seed = seed;
                self.seed = (seed as i64 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
                Value::Num(previous)
            }
            "close" => {
                let path = self.text_arg(args, 0)?;
                let known = self.outputs.contains_key(&path) || self.readers.contains_key(&path);
                self.flush_output(&path)?;
                self.outputs.remove(&path);
                self.readers.remove(&path);
                Value::Num(if known { 0.0 } else { -1.0 })
            }
            "fflush" => {
                self.flush_all()?;
                Value::Num(0.0)
            }
            // There's no way to start a command from here: see the module docs.
            _ => Value::Num(-1.0),
        })
    }

    fn substitute(&mut self, global: bool, args: &[Expr]) -> Eval<Value> {
        let pattern = self.text_arg(args, 0)?;
        let replacement = self.text_arg(args, 1)?;
        let target = args.get(2);
        let original = match target {
            Some(expression) => {
                let value = self.eval(expression)?;
                self.to_text(&value)
            }
            None => {
                self.rebuild_record();
                self.record.clone()
            }
        };

        let bytes = original.as_bytes();
        let regex = self.regex(&pattern)?;
        let mut result: Vec<u8> = Vec::new();
        let mut position = 0;
        let mut count = 0usize;
        while position <= bytes.len() {
            let Some(found) = regex.find_at(bytes, position) else {
                break;
            };
            let (begin, end) = found.span();
            result.extend_from_slice(&bytes[position..begin]);
            let matched = &bytes[begin..end];
            let mut chars = replacement.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '&' => result.extend_from_slice(matched),
                    '\\' => match chars.next() {
                        Some('&') => result.push(b'&'),
                        Some('\\') => result.push(b'\\'),
                        Some(other) => {
                            result.push(b'\\');
                            let mut buffer = [0u8; 4];
                            result.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
                        }
                        None => result.push(b'\\'),
                    },
                    other => {
                        let mut buffer = [0u8; 4];
                        result.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
                    }
                }
            }
            if result.len() > MAX_STREAM_BYTES {
                return fail("string is too long");
            }
            count += 1;
            if end == begin {
                // An empty match copies a character and moves past it.
                let step = original
                    .get(begin..)
                    .and_then(|rest| rest.chars().next())
                    .map_or(1, char::len_utf8);
                let stop = (begin + step).min(bytes.len());
                result.extend_from_slice(&bytes[begin..stop]);
                position = begin + step;
            } else {
                position = end;
            }
            if !global {
                break;
            }
        }
        if count == 0 {
            return Ok(Value::Num(0.0));
        }
        if position < bytes.len() {
            result.extend_from_slice(&bytes[position..]);
        }
        let text = String::from_utf8_lossy(&result).into_owned();
        match target {
            Some(expression) => self.assign_to(expression, Value::input(&text))?,
            None => self.set_record(text),
        }
        Ok(Value::Num(count as f64))
    }

    fn getline(&mut self, target: Option<&Expr>, file: Option<&Expr>) -> Eval<Value> {
        let separator = self.string_var("RS", "\n");
        let record = match file {
            Some(file) => {
                let value = self.eval(file)?;
                let path = self.to_text(&value);
                if !self.readers.contains_key(&path) {
                    if path == "-" || path == "/dev/stdin" {
                        self.readers.insert(path.clone(), Source::standard_input());
                    } else {
                        match self.ctx.fs.read_file(&path) {
                            Ok(data) => {
                                self.readers.insert(path.clone(), Source::file(data));
                            }
                            Err(_) => return Ok(Value::Num(-1.0)),
                        }
                    }
                }
                let Some(mut source) = self.readers.remove(&path) else {
                    return Ok(Value::Num(-1.0));
                };
                let record = source.next_record(&separator, self.ctx);
                self.readers.insert(path, source);
                record
            }
            None => {
                let record = self.next_main_record()?;
                if record.is_some() {
                    self.nr += 1.0;
                    self.fnr += 1.0;
                }
                record
            }
        };
        let Some(record) = record else {
            return Ok(Value::Num(0.0));
        };
        match target {
            Some(target) => self.assign_to(target, Value::input(&record))?,
            None => self.set_record(record),
        }
        Ok(Value::Num(1.0))
    }
}

fn round(value: f64) -> f64 {
    floor(value + 0.5)
}

fn arithmetic(op: &str, left: f64, right: f64) -> Eval<f64> {
    Ok(match op {
        "+" => left + right,
        "-" => left - right,
        "*" => left * right,
        "/" => {
            if right == 0.0 {
                return fail("division by zero");
            }
            left / right
        }
        "%" => {
            if right == 0.0 {
                return fail("division by zero in %");
            }
            left % right
        }
        _ => pow(left, right),
    })
}

/// `printf`'s formatting.
fn sprintf(format_text: &str, values: &[Value]) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = format_text.chars().peekable();
    let mut next = values.iter();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }
        let (mut left, mut plus, mut space, mut alternate, mut zero) = (false, false, false, false, false);
        while let Some(flag) = chars.peek().copied() {
            match flag {
                '-' => left = true,
                '+' => plus = true,
                ' ' => space = true,
                '#' => alternate = true,
                '0' => zero = true,
                _ => break,
            }
            chars.next();
        }
        let mut number = |chars: &mut core::iter::Peekable<core::str::Chars<'_>>| -> Option<usize> {
            if chars.peek() == Some(&'*') {
                chars.next();
                return Some(next.next().map_or(0.0, Value::num).max(0.0) as usize);
            }
            let mut value = None;
            while let Some(digit) = chars.peek().and_then(|ch| ch.to_digit(10)) {
                chars.next();
                value = Some(value.unwrap_or(0usize).saturating_mul(10).saturating_add(digit as usize));
            }
            value
        };
        let width = number(&mut chars).unwrap_or(0);
        let precision = if chars.peek() == Some(&'.') {
            chars.next();
            Some(number(&mut chars).unwrap_or(0))
        } else {
            None
        };
        if width > MAX_WIDTH || precision.is_some_and(|precision| precision > MAX_WIDTH) {
            return Err(String::from("printf field is too wide"));
        }
        // C's length modifiers mean nothing here.
        while matches!(chars.peek(), Some('l' | 'h' | 'L' | 'z' | 'j' | 't')) {
            chars.next();
        }
        let Some(conversion) = chars.next() else {
            out.push('%');
            break;
        };
        let value = match conversion {
            'd' | 'i' | 'o' | 'x' | 'X' | 'u' | 'c' | 's' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G' => {
                next.next().cloned().unwrap_or(Value::Uninit)
            }
            other => {
                out.push('%');
                out.push(other);
                continue;
            }
        };

        let (sign, body, numeric) = match conversion {
            'd' | 'i' => {
                let number = truncate(value.num());
                let text = if number.is_finite() {
                    format!("{}", number.abs() as u64)
                } else {
                    String::from("inf")
                };
                let text = pad_digits(text, precision);
                (sign_text(number < 0.0, plus, space), text, precision.is_none())
            }
            'o' | 'x' | 'X' | 'u' => {
                let number = truncate(value.num()) as i64 as u64;
                let mut text = match conversion {
                    'o' => format!("{number:o}"),
                    'x' => format!("{number:x}"),
                    'X' => format!("{number:X}"),
                    _ => format!("{number}"),
                };
                text = pad_digits(text, precision);
                if alternate && number != 0 {
                    text = match conversion {
                        'o' => format!("0{text}"),
                        'x' => format!("0x{text}"),
                        'X' => format!("0X{text}"),
                        _ => text,
                    };
                }
                ("", text, precision.is_none())
            }
            'c' => {
                let text = match &value {
                    Value::Num(number) => char::from_u32(*number as u32).unwrap_or('?').to_string(),
                    Value::Uninit => String::new(),
                    Value::Str(text) | Value::StrNum(text, _) => {
                        text.chars().next().map_or(String::new(), |ch| ch.to_string())
                    }
                };
                ("", text, false)
            }
            's' => {
                let text = match &value {
                    Value::Uninit => String::new(),
                    Value::Num(number) => number_text(*number, "%.6g"),
                    Value::Str(text) | Value::StrNum(text, _) => text.clone(),
                };
                let text = match precision {
                    Some(limit) => text.chars().take(limit).collect(),
                    None => text,
                };
                ("", text, false)
            }
            _ => {
                let number = value.num();
                let digits = precision.unwrap_or(6);
                let magnitude = number.abs();
                let upper = conversion.is_ascii_uppercase();
                let text = if number.is_nan() {
                    String::from("nan")
                } else if number.is_infinite() {
                    String::from("inf")
                } else {
                    match conversion {
                        'e' | 'E' => format_exponent(magnitude, digits, upper),
                        'f' | 'F' => format!("{magnitude:.digits$}"),
                        _ => format_general(magnitude, digits, alternate, upper),
                    }
                };
                (sign_text(number < 0.0, plus, space), text, true)
            }
        };

        let length = sign.chars().count() + body.chars().count();
        let padding = width.saturating_sub(length);
        if left {
            out.push_str(sign);
            out.push_str(&body);
            out.extend(core::iter::repeat_n(' ', padding));
        } else if zero && numeric {
            out.push_str(sign);
            out.extend(core::iter::repeat_n('0', padding));
            out.push_str(&body);
        } else {
            out.extend(core::iter::repeat_n(' ', padding));
            out.push_str(sign);
            out.push_str(&body);
        }
        if out.len() > MAX_STREAM_BYTES {
            return Err(String::from("printf output is too long"));
        }
    }
    Ok(out)
}

fn sign_text(negative: bool, plus: bool, space: bool) -> &'static str {
    if negative {
        "-"
    } else if plus {
        "+"
    } else if space {
        " "
    } else {
        ""
    }
}

fn pad_digits(text: String, precision: Option<usize>) -> String {
    match precision {
        Some(wanted) if wanted > text.len() => {
            let mut padded = String::with_capacity(wanted);
            padded.extend(core::iter::repeat_n('0', wanted - text.len()));
            padded.push_str(&text);
            padded
        }
        _ => text,
    }
}

/// `name=value` among the operands.
fn split_assignment(operand: &str) -> Option<(&str, &str)> {
    let (name, value) = operand.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some((name, value))
}

/// Escapes in `-v` values and `-F`, as in a string in the program.
fn unescape(text: &str) -> String {
    let mut lexer = Lexer::new("");
    lexer.chars = core::iter::once('"')
        .chain(text.chars().filter(|ch| *ch != '\n'))
        .chain(core::iter::once('"'))
        .collect();
    if text.contains('"') {
        return text.to_string();
    }
    match lexer.string() {
        Ok(Token::Str(value)) => value,
        _ => text.to_string(),
    }
}

/// `awk [-F fs] [-v name=value] [-f file | program] [file...]`
pub(super) fn awk(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut separator = None;
    let mut assignments = Vec::new();
    let mut program_file = None;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        let (flag, attached) = arg.split_at(2.min(arg.len()));
        let value = if attached.is_empty() {
            index += 1;
            args.get(index).cloned()
        } else {
            Some(attached.to_string())
        };
        let Some(value) = value else {
            ctx.stderr.write(format!("awk: {flag} needs a value\n").as_bytes());
            return 2;
        };
        match flag {
            "-F" => separator = Some(value),
            "-v" => assignments.push(value),
            "-f" => program_file = Some(value),
            _ => {
                ctx.stderr.write(format!("awk: unknown option {flag}\n").as_bytes());
                return 2;
            }
        }
        index += 1;
    }

    let program = match program_file {
        Some(path) => match ctx.fs.read_file(&path) {
            Ok(data) => String::from_utf8_lossy(&data).into_owned(),
            Err(error) => {
                ctx.stderr.write(format!("awk: {path}: {}\n", error.text()).as_bytes());
                return 2;
            }
        },
        None => match args.get(index) {
            Some(program) => {
                index += 1;
                program.clone()
            }
            None => {
                ctx.stderr.write(b"usage: awk [-F fs] [-v name=value] [-f file | program] [file...]\n");
                return 2;
            }
        },
    };

    let parsed = Parser::new(&program).and_then(|mut parser| parser.program());
    let (mut rules, functions) = match parsed {
        Ok(parsed) => parsed,
        Err(message) => {
            ctx.stderr.write(format!("awk: syntax error: {message}\n").as_bytes());
            return 2;
        }
    };

    let files = args.get(index..).unwrap_or(&[]).to_vec();
    let mut interp = Interp {
        ctx,
        functions: &functions,
        globals: BTreeMap::new(),
        frames: Vec::new(),
        arrays: Vec::new(),
        regexes: BTreeMap::new(),
        record: String::new(),
        fields: Vec::new(),
        fields_stale: false,
        record_stale: false,
        nr: 0.0,
        fnr: 0.0,
        files,
        next_file: 0,
        input: None,
        used_a_file: false,
        readers: BTreeMap::new(),
        outputs: BTreeMap::new(),
        exit_code: 0,
        depth: 0,
        calls: 0,
        seed: 0x2545_f491_4f6c_dd1d,
        prev_seed: 0.0,
        ticks: 0,
    };
    let result = interp.main(&mut rules, separator, &assignments);
    let flushed = interp.flush_all();
    let message = match (result, flushed) {
        (Err(Stop::Error(message)), _) | (_, Err(Stop::Error(message))) => Some(message),
        _ => None,
    };
    if let Some(message) = message {
        interp.ctx.stderr.write(format!("awk: {message}\n").as_bytes());
        return 2;
    }
    interp.exit_code
}

impl Interp<'_, '_, '_> {
    fn main(
        &mut self,
        rules: &mut [Rule],
        separator: Option<String>,
        assignments: &[String],
    ) -> Eval<()> {
        for (name, value) in [("FS", " "), ("OFS", " "), ("ORS", "\n"), ("RS", "\n"), ("SUBSEP", "\x1c")] {
            self.set_var(name, Value::Str(value.to_string()))?;
        }
        self.set_var("CONVFMT", Value::Str(String::from("%.6g")))?;
        self.set_var("OFMT", Value::Str(String::from("%.6g")))?;
        if let Some(separator) = separator {
            let separator = if separator == "t" { String::from("\t") } else { unescape(&separator) };
            self.set_var("FS", Value::Str(separator))?;
        }
        for assignment in assignments {
            let Some((name, value)) = split_assignment(assignment) else {
                return fail(format!("bad assignment '{assignment}'"));
            };
            self.set_var(name, Value::input(&unescape(value)))?;
        }

        let mut exiting = false;
        for rule in rules.iter() {
            if let (Pattern::Begin, Some(action)) = (&rule.pattern, &rule.action) {
                match self.run_action(action) {
                    Err(Stop::Exit) => {
                        exiting = true;
                        break;
                    }
                    Err(Stop::Next | Stop::NextFile) => return fail("next used in BEGIN"),
                    other => other?,
                }
            }
        }

        let wants_input = rules
            .iter()
            .any(|rule| !matches!(rule.pattern, Pattern::Begin));
        if !exiting && wants_input {
            exiting = self.main_loop(rules)?;
        }

        // `exit` in BEGIN or a rule still runs END, unless END itself exits.
        let _ = exiting;
        for rule in rules.iter() {
            if let (Pattern::End, Some(action)) = (&rule.pattern, &rule.action) {
                match self.run_action(action) {
                    Err(Stop::Exit) => break,
                    other => other?,
                }
            }
        }
        Ok(())
    }

    fn run_action(&mut self, action: &[Stmt]) -> Eval<()> {
        match self.run_block(action) {
            Err(Stop::Break | Stop::Continue) => fail("break or continue outside a loop"),
            Err(Stop::Return(_)) => fail("return outside a function"),
            other => other,
        }
    }

    /// Returns `true` when the program called `exit`.
    fn main_loop(&mut self, rules: &mut [Rule]) -> Eval<bool> {
        while let Some(record) = self.next_main_record()? {
            self.poll()?;
            if self.ctx.stdout.is_broken() {
                return Ok(true);
            }
            self.nr += 1.0;
            self.fnr += 1.0;
            self.set_record(record);
            'rules: for rule in rules.iter_mut() {
                let selected = match &rule.pattern {
                    Pattern::Begin | Pattern::End => false,
                    Pattern::Always => true,
                    Pattern::Expr(pattern) => self.eval(pattern)?.truth(),
                    Pattern::Range(start, end) => {
                        if !rule.in_range && self.eval(start)?.truth() {
                            rule.in_range = true;
                        }
                        if rule.in_range {
                            if self.eval(end)?.truth() {
                                rule.in_range = false;
                            }
                            true
                        } else {
                            false
                        }
                    }
                };
                if !selected {
                    continue;
                }
                let outcome = match &rule.action {
                    Some(action) => self.run_action(action),
                    None => {
                        self.rebuild_record();
                        let mut line = self.record.clone();
                        line.push_str(&self.string_var("ORS", "\n"));
                        self.ctx.stdout.write(line.as_bytes());
                        Ok(())
                    }
                };
                match outcome {
                    Ok(()) => {}
                    Err(Stop::Next) => break 'rules,
                    Err(Stop::NextFile) => {
                        self.input = None;
                        break 'rules;
                    }
                    Err(Stop::Exit) => return Ok(true),
                    Err(other) => return Err(other),
                }
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::OverlayFs;
    use crate::io::{BytesInput, BytesOutput};
    use crate::{Env, NoSystem};
    use std::vec;

    fn run_with(args: &[&str], stdin: &str) -> (i32, String, String) {
        let mut fs = OverlayFs::new(None);
        let mut env = Env::feos();
        let mut input = BytesInput::new(stdin.as_bytes().to_vec());
        let mut out = BytesOutput::new();
        let mut err = BytesOutput::new();
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let status = {
            let mut ctx = Ctx {
                fs: &mut fs,
                env: &mut env,
                stdin: &mut input,
                stdout: &mut out,
                stderr: &mut err,
                system: &NoSystem,
                depth: 0,
            };
            awk(&args, &mut ctx)
        };
        (
            status,
            String::from_utf8_lossy(out.bytes()).into_owned(),
            String::from_utf8_lossy(err.bytes()).into_owned(),
        )
    }

    fn out(program: &str, stdin: &str) -> String {
        let (_, stdout, stderr) = run_with(&[program], stdin);
        assert_eq!(stderr, "", "program: {program}");
        stdout
    }

    #[test]
    fn fields_and_patterns() {
        assert_eq!(out("{ print $2 }", "a b c\nd e f\n"), "b\ne\n");
        assert_eq!(out("/e/", "a b\nd e\n"), "d e\n");
        assert_eq!(out("$1 > 2", "1\n2\n3\n"), "3\n");
        assert_eq!(out("NR==2,NR==3 { print NR }", "a\nb\nc\nd\n"), "2\n3\n");
        assert_eq!(out("END { print NR }", "a\nb\n"), "2\n");
        assert_eq!(out("{ print NF, $NF }", "a b c\n"), "3 c\n");
        assert_eq!(out("BEGIN { print 1 + 2 * 3, 2 ^ 10, 7 % 3 }", ""), "7 1024 1\n");
    }

    #[test]
    fn separators() {
        assert_eq!(run_with(&["-F:", "{ print $2 }"], "a:b\n").1, "b\n");
        assert_eq!(run_with(&["-F", "\\t", "{ print $2 }"], "a b\tc\n").1, "c\n");
        assert_eq!(run_with(&["-F", "[,;]+", "{ print $3 }"], "a,b;;c\n").1, "c\n");
        assert_eq!(out("BEGIN { OFS=\"-\" } { $1=$1; print }", "a b\n"), "a-b\n");
        assert_eq!(out("BEGIN { RS=\";\" } { print }", "a;b"), "a\nb\n");
        assert_eq!(out("BEGIN { RS=\"\" } { print NF }", "a b\nc\n\n\nd\n"), "3\n1\n");
    }

    #[test]
    fn variables_arrays_and_control_flow() {
        let program = "{ n[$1]++ } END { for (k in n) print k, n[k] }";
        assert_eq!(out(program, "x\ny\nx\n"), "x 2\ny 1\n");
        assert_eq!(out("BEGIN { for (i=0;i<3;i++) s = s i; print s }", ""), "012\n");
        assert_eq!(out("BEGIN { while (i<5) { if (++i==2) continue; if (i==4) break; print i } }", ""), "1\n3\n");
        assert_eq!(out("BEGIN { do { print \"once\" } while (0) }", ""), "once\n");
        assert_eq!(out("BEGIN { a[1,2]=3; if ((1,2) in a) print \"yes\"; delete a[1,2]; print length(a) }", ""), "yes\n0\n");
        assert_eq!(out("BEGIN { x = 1 > 2 ? \"a\" : \"b\"; print x }", ""), "b\n");
        assert_eq!(out("{ if (NR==1) next; print }", "a\nb\n"), "b\n");
        assert_eq!(out("BEGIN { if (1) print \"y\"; else print \"n\" }", ""), "y\n");
    }

    #[test]
    fn exit_runs_end_and_sets_the_status() {
        let (status, stdout, _) = run_with(&["BEGIN { exit 3 } END { print \"end\" }"], "");
        assert_eq!((status, stdout.as_str()), (3, "end\n"));
        let (status, stdout, _) = run_with(&["NR==1 { print; exit }"], "a\nb\n");
        assert_eq!((status, stdout.as_str()), (0, "a\n"));
    }

    #[test]
    fn strings() {
        assert_eq!(out("BEGIN { print length(\"héllo\"), substr(\"hello\", 2, 3), index(\"hello\", \"ll\") }", ""), "5 ell 3\n");
        assert_eq!(out("BEGIN { print substr(\"hello\", 0), substr(\"hello\", -1, 3), substr(\"hello\", 4, 100) }", ""), "hello h lo\n");
        assert_eq!(out("BEGIN { print toupper(\"ab\") tolower(\"CD\") }", ""), "ABcd\n");
        assert_eq!(out("BEGIN { n = split(\"a:b:c\", p, \":\"); print n, p[3] }", ""), "3 c\n");
        assert_eq!(out("{ gsub(/o/, \"0\"); print }", "foo boo\n"), "f00 b00\n");
        assert_eq!(out("{ sub(/o+/, \"[&]\"); print }", "foo boo\n"), "f[oo] boo\n");
        assert_eq!(out("{ n = gsub(/x*/, \"-\"); print n, $0 }", "abc\n"), "4 -a-b-c-\n");
        assert_eq!(out("BEGIN { s=\"aXb\"; sub(/X/, \"\\\\&\", s); print s }", ""), "a&b\n");
        assert_eq!(out("BEGIN { print match(\"foobar\", /o+/), RSTART, RLENGTH }", ""), "2 2 2\n");
        assert_eq!(out("$0 ~ \"^a\" { print \"m\" } $0 !~ /b/ { print \"n\" }", "ab\n"), "m\n");
    }

    #[test]
    fn comparisons_follow_the_type_of_the_values() {
        assert_eq!(out("$1 < $2 { print \"num\" }", "10 9\n"), "");
        assert_eq!(out("BEGIN { if (\"10\" < \"9\") print \"str\" }", ""), "str\n");
        assert_eq!(out("$1 == 1 { print \"same\" }", "1.0\n"), "same\n");
        assert_eq!(out("BEGIN { if (x == 0 && x == \"\") print \"uninit\" }", ""), "uninit\n");
    }

    #[test]
    fn printf_formats() {
        assert_eq!(out("BEGIN { printf \"%d|%5d|%-5d|%05d|%x\\n\", 3.9, 42, 42, 42, 255 }", ""), "3|   42|42   |00042|ff\n");
        assert_eq!(out("BEGIN { printf \"%s|%5s|%-5s|%.2s\\n\", \"a\", \"b\", \"c\", \"defg\" }", ""), "a|    b|c    |de\n");
        assert_eq!(out("BEGIN { printf \"%.2f %e %g %g %c%c%%\\n\", 3.14159, 1500, 0.0001, 1000000, \"xy\", 65 }", ""), "3.14 1.500000e+03 0.0001 1e+06 xA%\n");
        assert_eq!(out("BEGIN { x = sprintf(\"%*d\", 4, 7); print x }", ""), "   7\n");
        assert_eq!(out("BEGIN { print 0.1 + 0.2, 1e6, 100000 * 100000, -3 }", ""), "0.3 1000000 10000000000 -3\n");
        assert_eq!(run_with(&["BEGIN { printf \"%99999d\", 1 }"], "").0, 2);
    }

    #[test]
    fn functions_recursion_and_arrays_by_reference() {
        assert_eq!(out("function f(n) { return n < 2 ? 1 : n * f(n - 1) } BEGIN { print f(5) }", ""), "120\n");
        assert_eq!(out("function g(a) { a[1] = 9 } BEGIN { g(x); print x[1] }", ""), "9\n");
        assert_eq!(out("function h(v, local) { local = v; v = 2; return local } BEGIN { y = 1; print h(y), y }", ""), "1 1\n");
    }

    #[test]
    fn maths_is_close_enough() {
        let program = "BEGIN { printf \"%.4f %.4f %.4f %.4f %.4f %.4f\\n\", sqrt(2), exp(1), log(10), sin(1), cos(1), atan2(1, 1) }";
        assert_eq!(out(program, ""), "1.4142 2.7183 2.3026 0.8415 0.5403 0.7854\n");
        assert_eq!(out("BEGIN { print int(3.9), int(-3.9), (2 ^ 0.5 > 1.41) }", ""), "3 -3 1\n");
    }

    #[test]
    fn getline_and_variables_from_the_command_line() {
        assert_eq!(out("NR==1 { getline; print }", "a\nb\nc\n"), "b\n");
        assert_eq!(out("{ while (getline line) n++ } END { print n }", "a\nb\nc\n"), "2\n");
        assert_eq!(run_with(&["-v", "x=5", "BEGIN { print x + 1 }"], "").1, "6\n");
        assert_eq!(run_with(&["{ print y }", "y=3", "-"], "a\n").1, "3\n");
    }

    #[test]
    fn output_can_go_to_a_file() {
        let mut fs = OverlayFs::new(None);
        let mut env = Env::feos();
        let mut input = BytesInput::empty();
        let mut output = BytesOutput::new();
        let mut errors = BytesOutput::new();
        let mut ctx = Ctx {
            fs: &mut fs,
            env: &mut env,
            stdin: &mut input,
            stdout: &mut output,
            stderr: &mut errors,
            system: &NoSystem,
            depth: 0,
        };
        let program = vec![String::from("BEGIN { print \"x\" > \"/o\"; print \"y\" > \"/o\"; print \"z\" >> \"/p\" }")];
        assert_eq!(awk(&program, &mut ctx), 0);
        assert_eq!(ctx.fs.read_file("/o").unwrap(), b"x\ny\n");
        assert_eq!(ctx.fs.read_file("/p").unwrap(), b"z\n");
    }

    #[test]
    fn bad_programs_are_errors_not_panics() {
        for program in [
            "{", "}", "BEGIN", "(", "$", "print >", "1 +", "/x", "\"x", "function", "function f(",
            "BEGIN { getline < }", "BEGIN { (1,2) }", "BEGIN { f(1) }", "BEGIN { 1/0 }",
            "BEGIN { x[1]=1; x=2 }", "BEGIN { x=1; x[1]=2 }", "BEGIN { $(-1)=1 }",
            "BEGIN { print length(1,2,3) | \"cat\" }", "@", "BEGIN { split() }", "BEGIN { substr() }",
            "BEGIN { \"a\" ~ \"(\" }", "function f() { return f() } BEGIN { f() }",
        ] {
            let (status, _, stderr) = run_with(&[program], "");
            assert_eq!(status, 2, "{program}: {stderr}");
            assert!(!stderr.is_empty(), "{program}");
        }
        let deep = "(".repeat(5000);
        assert_eq!(run_with(&[&deep], "").0, 2);
        let deep = std::format!("BEGIN {{ x = {}1 }}", "-".repeat(5000));
        assert_eq!(run_with(&[&deep], "").0, 2);
    }

    #[test]
    fn it_works_as_a_filter_with_no_input_needed() {
        assert_eq!(out("BEGIN { print \"x\" }", "ignored\n"), "x\n");
        assert_eq!(run_with(&["{ print }", "/missing"], "").0, 2);
        assert_eq!(run_with(&[], "").0, 2);
    }
}
