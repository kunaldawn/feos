//! The expression language of MilkDrop presets: a subset of Nullsoft's EEL2.
//!
//! Code is a list of statements separated by `;`. Every statement is an
//! expression, and an assignment is one too. Variables are case-insensitive
//! doubles that spring into being at zero. `reg00`..`reg99` and `gmegabuf` are
//! shared by all the code in a [`Globals`]; everything else lives in one
//! context's [`Env`]. Precedence follows EEL2 (tightest first):
//!
//! | operators            | |
//! |----------------------|-|
//! | `-x` `+x` `!x`       | unary |
//! | `^`                  | power, right associative |
//! | `%`                  | integer remainder |
//! | `*` `/`              | |
//! | `+` `-`              | |
//! | `\|` `&`             | integer bitwise |
//! | `==` `!=` `<` `>` `<=` `>=` | |
//! | `\|\|` `&&`          | short-circuit |
//! | `? :`                | |
//! | `=` `+=` `-=` `*=` `/=` `%=` `^=` | right associative |
//!
//! The arithmetic is MilkDrop's, which is forgiving on purpose so that a typo
//! doesn't turn a preset black: dividing by zero gives zero, `sqrt` takes the
//! magnitude, `pow` that has no real answer gives zero, and comparisons for
//! equality allow 1e-5 of slack.
//!
//! Old presets often leave out a `;` at the end of a line. As MilkDrop did,
//! a line break between two complete expressions separates them.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use feos_math as m;

/// What comparisons and truth tests forgive.
const EPSILON: f64 = 0.00001;
/// `megabuf` and `gmegabuf` entries: 1M doubles, allocated 64K at a time.
pub const MEGABUF_ENTRIES: usize = 1 << 20;
const MEGABUF_BLOCK: usize = 1 << 16;
/// Most iterations `loop` and `while` run, as EEL2 caps them.
const LOOP_LIMIT: usize = 1 << 20;
/// Nesting depth a parser accepts before calling the code malformed.
const MAX_DEPTH: usize = 200;

/// Why code didn't compile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompileError {
    /// Something was found where it can't be.
    Unexpected(usize),
    /// The code ended partway through an expression.
    UnexpectedEnd,
    /// A function that this doesn't know, or with the wrong arguments.
    UnknownFunction(usize),
    /// Assignment to something that isn't a variable or memory.
    NotAssignable(usize),
    /// The code nests too deeply.
    TooDeep,
    /// No memory for the program.
    OutOfMemory,
}

/// Variable names, and the slots they map to.
///
/// A context registers its built-in names first, in a fixed order, so code
/// that drives it can find them by index without looking anything up.
#[derive(Clone, Default, Debug)]
pub struct VarTable {
    names: Vec<String>,
}

impl VarTable {
    /// A table with `builtins` in slots `0..builtins.len()`.
    pub fn with_builtins(builtins: &[&str]) -> Self {
        let mut table = Self { names: Vec::new() };
        for name in builtins {
            table.names.push(String::from(*name));
        }
        table
    }

    /// The slot for `name`, adding it if it's new. Names are case-insensitive.
    pub fn slot(&mut self, name: &str) -> Option<u32> {
        if let Some(index) = self.find(name) {
            return Some(index);
        }
        let mut lower = String::new();
        lower.try_reserve(name.len()).ok()?;
        lower.extend(name.chars().map(|c| c.to_ascii_lowercase()));
        self.names.try_reserve(1).ok()?;
        self.names.push(lower);
        Some(self.names.len() as u32 - 1)
    }

    /// The slot for `name`, if it has one.
    pub fn find(&self, name: &str) -> Option<u32> {
        self.names
            .iter()
            .position(|known| known.eq_ignore_ascii_case(name))
            .map(|index| index as u32)
    }

    /// How many slots there are.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Memory addressed by number, as `megabuf(i)`.
#[derive(Default)]
pub struct MemBuf {
    blocks: Vec<Option<Box<[f64]>>>,
}

impl MemBuf {
    /// The value at `index`: zero if it was never written, or out of range.
    pub fn get(&self, index: f64) -> f64 {
        let Some(at) = mem_index(index) else {
            return 0.0;
        };
        match self.blocks.get(at / MEGABUF_BLOCK) {
            Some(Some(block)) => block[at % MEGABUF_BLOCK],
            _ => 0.0,
        }
    }

    /// Stores `value` at `index`, if there's memory for it.
    pub fn set(&mut self, index: f64, value: f64) {
        let Some(at) = mem_index(index) else {
            return;
        };
        let block = at / MEGABUF_BLOCK;
        if self.blocks.len() <= block {
            if self.blocks.try_reserve(block + 1 - self.blocks.len()).is_err() {
                return;
            }
            self.blocks.resize_with(block + 1, || None);
        }
        if self.blocks[block].is_none() {
            let mut storage = Vec::new();
            if storage.try_reserve_exact(MEGABUF_BLOCK).is_err() {
                return;
            }
            storage.resize(MEGABUF_BLOCK, 0.0);
            self.blocks[block] = Some(storage.into_boxed_slice());
        }
        if let Some(Some(storage)) = self.blocks.get_mut(block) {
            storage[at % MEGABUF_BLOCK] = value;
        }
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.blocks.clear();
    }
}

fn mem_index(index: f64) -> Option<usize> {
    // EEL2 rounds the address to the nearest entry.
    let at = m::floor(index + EPSILON);
    if !(0.0..MEGABUF_ENTRIES as f64).contains(&at) {
        return None;
    }
    Some(at as usize)
}

/// State every context of one visualiser shares.
pub struct Globals {
    /// `reg00` to `reg99`.
    pub regs: [f64; 100],
    /// `gmegabuf`.
    pub gmegabuf: MemBuf,
    rng: u64,
}

impl Default for Globals {
    fn default() -> Self {
        Self::new(0x9e37_79b9_7f4a_7c15)
    }
}

impl Globals {
    /// Fresh globals, with `rand` seeded from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            regs: [0.0; 100],
            gmegabuf: MemBuf::default(),
            rng: seed | 1,
        }
    }

    /// A uniform random number in [0, 1).
    pub fn random(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// One context's variables and memory.
#[derive(Default)]
pub struct Env {
    /// Slot values, indexed as the [`VarTable`] the code compiled against.
    pub vars: Vec<f64>,
    /// `megabuf`.
    pub megabuf: MemBuf,
}

impl Env {
    /// Room for every slot in `table`, all zero.
    pub fn for_table(table: &VarTable) -> Option<Self> {
        let mut vars = Vec::new();
        vars.try_reserve_exact(table.len()).ok()?;
        vars.resize(table.len(), 0.0);
        Some(Self {
            vars,
            megabuf: MemBuf::default(),
        })
    }

    /// The value in slot `index`.
    pub fn get(&self, index: usize) -> f64 {
        self.vars.get(index).copied().unwrap_or(0.0)
    }

    /// Sets slot `index`.
    pub fn set(&mut self, index: usize, value: f64) {
        if let Some(slot) = self.vars.get_mut(index) {
            *slot = value;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unary {
    Neg,
    Not,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Binary {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Func1 {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Sqrt,
    Sqr,
    Abs,
    Sign,
    Exp,
    Log,
    Log10,
    Int,
    Floor,
    Ceil,
    Rand,
    Bnot,
    Invsqrt,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Func2 {
    Atan2,
    Pow,
    Min,
    Max,
    Above,
    Below,
    Equal,
    Band,
    Bor,
    Sigmoid,
    Fmod,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    Var(u32),
    Reg(u8),
    Mem(bool),
}

/// A compiled expression.
#[derive(Debug)]
enum Node {
    Const(f64),
    Var(u32),
    Reg(u8),
    /// `megabuf(i)` (local) or `gmegabuf(i)` (global).
    Mem(bool, Box<Node>),
    Assign(u32, Box<Node>),
    AssignReg(u8, Box<Node>),
    AssignMem(bool, Box<Node>, Box<Node>),
    Unary(Unary, Box<Node>),
    Binary(Binary, Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    If(Box<Node>, Box<Node>, Box<Node>),
    Call1(Func1, Box<Node>),
    Call2(Func2, Box<Node>, Box<Node>),
    Seq(Vec<Node>),
    Loop(Box<Node>, Box<Node>),
    While(Box<Node>),
}

/// Compiled code, ready to run against an [`Env`] made for its table.
#[derive(Debug, Default)]
pub struct Program {
    statements: Vec<Node>,
}

impl Program {
    /// Compiles `source`, adding any new variables to `table`.
    pub fn compile(source: &str, table: &mut VarTable) -> Result<Self, CompileError> {
        let tokens = lex(source)?;
        let mut parser = Parser {
            source,
            tokens: &tokens,
            at: 0,
            table,
            depth: 0,
        };
        let statements = parser.statements(Stop::End)?;
        if parser.at < tokens.len() {
            return Err(CompileError::Unexpected(tokens[parser.at].offset));
        }
        Ok(Self { statements })
    }

    /// Whether there's nothing to run.
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// Runs the code. Returns the value of the last statement.
    pub fn run(&self, env: &mut Env, globals: &mut Globals) -> f64 {
        let mut last = 0.0;
        for statement in &self.statements {
            last = eval(statement, env, globals);
        }
        last
    }
}

fn truthy(value: f64) -> bool {
    value.abs() > EPSILON
}

fn bool_value(value: bool) -> f64 {
    if value {
        1.0
    } else {
        0.0
    }
}

/// Converts to an integer for the bitwise and remainder operators.
fn to_int(value: f64) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    m::floor(value).clamp(-9.0e18, 9.0e18) as i64
}

fn binary(op: Binary, a: f64, b: f64) -> f64 {
    match op {
        Binary::Add => a + b,
        Binary::Sub => a - b,
        Binary::Mul => a * b,
        Binary::Div => {
            if b == 0.0 {
                0.0
            } else {
                a / b
            }
        }
        Binary::Mod => {
            let (a, b) = (to_int(a), to_int(b));
            if b == 0 {
                0.0
            } else {
                a.wrapping_rem(b) as f64
            }
        }
        Binary::Pow => safe_pow(a, b),
        Binary::BitAnd => (to_int(a) & to_int(b)) as f64,
        Binary::BitOr => (to_int(a) | to_int(b)) as f64,
        Binary::Eq => bool_value((a - b).abs() < EPSILON),
        Binary::Ne => bool_value((a - b).abs() >= EPSILON),
        Binary::Lt => bool_value(a < b),
        Binary::Gt => bool_value(a > b),
        Binary::Le => bool_value(a <= b),
        Binary::Ge => bool_value(a >= b),
    }
}

fn safe_pow(a: f64, b: f64) -> f64 {
    let value = m::pow(a, b);
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn call1(func: Func1, x: f64, globals: &mut Globals) -> f64 {
    match func {
        Func1::Sin => m::sin(x),
        Func1::Cos => m::cos(x),
        Func1::Tan => m::tan(x),
        Func1::Asin => m::asin(x),
        Func1::Acos => m::acos(x),
        Func1::Atan => m::atan(x),
        Func1::Sqrt => m::sqrt(x.abs()),
        Func1::Sqr => x * x,
        Func1::Abs => x.abs(),
        Func1::Sign => {
            if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                0.0
            }
        }
        Func1::Exp => m::exp(x),
        Func1::Log => m::ln(x),
        Func1::Log10 => m::log10(x),
        Func1::Int | Func1::Floor => m::floor(x),
        Func1::Ceil => m::ceil(x),
        Func1::Rand => {
            let whole = m::floor(x);
            if whole < 1.0 {
                globals.random()
            } else {
                m::floor(globals.random() * whole)
            }
        }
        Func1::Bnot => bool_value(!truthy(x)),
        Func1::Invsqrt => {
            let root = m::sqrt(x.abs());
            if root == 0.0 {
                0.0
            } else {
                1.0 / root
            }
        }
    }
}

fn call2(func: Func2, a: f64, b: f64) -> f64 {
    match func {
        Func2::Atan2 => m::atan2(a, b),
        Func2::Pow => safe_pow(a, b),
        Func2::Min => {
            if a < b {
                a
            } else {
                b
            }
        }
        Func2::Max => {
            if a > b {
                a
            } else {
                b
            }
        }
        Func2::Above => bool_value(a > b),
        Func2::Below => bool_value(a < b),
        Func2::Equal => bool_value((a - b).abs() < EPSILON),
        Func2::Band => bool_value(truthy(a) && truthy(b)),
        Func2::Bor => bool_value(truthy(a) || truthy(b)),
        Func2::Sigmoid => {
            let t = 1.0 + m::exp(-a * b);
            if t.abs() > EPSILON {
                1.0 / t
            } else {
                0.0
            }
        }
        Func2::Fmod => {
            if b == 0.0 {
                0.0
            } else {
                m::fmod(a, b)
            }
        }
    }
}

fn eval(node: &Node, env: &mut Env, globals: &mut Globals) -> f64 {
    match node {
        Node::Const(value) => *value,
        Node::Var(slot) => env.vars[*slot as usize],
        Node::Reg(index) => globals.regs[*index as usize],
        Node::Mem(global, index) => {
            let at = eval(index, env, globals);
            if *global {
                globals.gmegabuf.get(at)
            } else {
                env.megabuf.get(at)
            }
        }
        Node::Assign(slot, value) => {
            let value = eval(value, env, globals);
            env.vars[*slot as usize] = value;
            value
        }
        Node::AssignReg(index, value) => {
            let value = eval(value, env, globals);
            globals.regs[*index as usize] = value;
            value
        }
        Node::AssignMem(global, index, value) => {
            let at = eval(index, env, globals);
            let value = eval(value, env, globals);
            if *global {
                globals.gmegabuf.set(at, value);
            } else {
                env.megabuf.set(at, value);
            }
            value
        }
        Node::Unary(Unary::Neg, value) => -eval(value, env, globals),
        Node::Unary(Unary::Not, value) => bool_value(!truthy(eval(value, env, globals))),
        Node::Binary(op, a, b) => {
            let a = eval(a, env, globals);
            let b = eval(b, env, globals);
            binary(*op, a, b)
        }
        Node::And(a, b) => {
            bool_value(truthy(eval(a, env, globals)) && truthy(eval(b, env, globals)))
        }
        Node::Or(a, b) => bool_value(truthy(eval(a, env, globals)) || truthy(eval(b, env, globals))),
        Node::If(condition, then, otherwise) => {
            if truthy(eval(condition, env, globals)) {
                eval(then, env, globals)
            } else {
                eval(otherwise, env, globals)
            }
        }
        Node::Call1(func, x) => {
            let x = eval(x, env, globals);
            call1(*func, x, globals)
        }
        Node::Call2(func, a, b) => {
            let a = eval(a, env, globals);
            let b = eval(b, env, globals);
            call2(*func, a, b)
        }
        Node::Seq(statements) => {
            let mut last = 0.0;
            for statement in statements {
                last = eval(statement, env, globals);
            }
            last
        }
        Node::Loop(count, body) => {
            let count = eval(count, env, globals);
            let times = if count.is_finite() && count > 0.0 {
                (count as usize).min(LOOP_LIMIT)
            } else {
                0
            };
            let mut last = 0.0;
            for _ in 0..times {
                last = eval(body, env, globals);
            }
            last
        }
        Node::While(body) => {
            let mut last = 0.0;
            for _ in 0..LOOP_LIMIT {
                last = eval(body, env, globals);
                if !truthy(last) {
                    break;
                }
            }
            last
        }
    }
}

// The lexer ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Number(f64),
    /// An identifier: its byte range in the source.
    Name(usize, usize),
    /// Punctuation and operators, by their spelling.
    Op(&'static str),
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: Kind,
    offset: usize,
    /// A line break comes between this token and the one before.
    line_start: bool,
}

const OPERATORS: [&str; 30] = [
    "+=", "-=", "*=", "/=", "%=", "^=", "|=", "&=", "==", "!=", "<=", ">=", "&&", "||", "<<",
    ">>", "+", "-", "*", "/", "%", "^", "|", "&", "=", "<", ">", "!", "?", ":",
];

fn lex(source: &str) -> Result<Vec<Token>, CompileError> {
    let bytes = source.as_bytes();
    let mut tokens: Vec<Token> = Vec::new();
    let mut at = 0;
    let mut line_start = false;
    let push = |tokens: &mut Vec<Token>, kind, offset, line_start: &mut bool| {
        tokens.try_reserve(1).map_err(|_| CompileError::OutOfMemory)?;
        tokens.push(Token {
            kind,
            offset,
            line_start: *line_start,
        });
        *line_start = false;
        Ok::<(), CompileError>(())
    };
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            b'\n' | b'\r' => {
                line_start = true;
                at += 1;
            }
            b' ' | b'\t' | 0x0b | 0x0c => at += 1,
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                    if bytes[at] == b'\n' {
                        line_start = true;
                    }
                    at += 1;
                }
                at = (at + 2).min(bytes.len());
            }
            b'0'..=b'9' | b'.' => {
                let start = at;
                let (value, end) = number(bytes, at).ok_or(CompileError::Unexpected(start))?;
                at = end;
                push(&mut tokens, Kind::Number(value), start, &mut line_start)?;
            }
            b'$' => {
                let start = at;
                let (value, end) = constant(bytes, at).ok_or(CompileError::Unexpected(start))?;
                at = end;
                push(&mut tokens, Kind::Number(value), start, &mut line_start)?;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = at;
                while at < bytes.len()
                    && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_' || bytes[at] == b'.')
                {
                    at += 1;
                }
                push(&mut tokens, Kind::Name(start, at), start, &mut line_start)?;
            }
            b'(' | b')' | b',' | b';' | b'[' | b']' => {
                let spelling = match byte {
                    b'(' => "(",
                    b')' => ")",
                    b',' => ",",
                    b';' => ";",
                    b'[' => "[",
                    _ => "]",
                };
                push(&mut tokens, Kind::Op(spelling), at, &mut line_start)?;
                at += 1;
            }
            _ => {
                let rest = &source[at..];
                let Some(op) = OPERATORS.iter().find(|op| rest.starts_with(**op)) else {
                    // Anything else (stray quotes, high bytes) is skipped, as
                    // MilkDrop's own loader drops what it can't read.
                    at += source[at..].chars().next().map_or(1, char::len_utf8);
                    continue;
                };
                push(&mut tokens, Kind::Op(op), at, &mut line_start)?;
                at += op.len();
            }
        }
    }
    Ok(tokens)
}

fn number(bytes: &[u8], start: usize) -> Option<(f64, usize)> {
    let mut at = start;
    let mut mantissa = 0.0f64;
    let mut digits = 0;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        mantissa = mantissa * 10.0 + (bytes[at] - b'0') as f64;
        at += 1;
        digits += 1;
    }
    let mut scale = 0i32;
    if at < bytes.len() && bytes[at] == b'.' {
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            mantissa = mantissa * 10.0 + (bytes[at] - b'0') as f64;
            scale -= 1;
            at += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        // A lone '.' reads as zero, as some presets have `.;` in them.
        return Some((0.0, at));
    }
    if at < bytes.len() && (bytes[at] == b'e' || bytes[at] == b'E') {
        let mut probe = at + 1;
        let negative = match bytes.get(probe) {
            Some(b'-') => {
                probe += 1;
                true
            }
            Some(b'+') => {
                probe += 1;
                false
            }
            _ => false,
        };
        if bytes.get(probe).is_some_and(u8::is_ascii_digit) {
            let mut exponent = 0i32;
            while probe < bytes.len() && bytes[probe].is_ascii_digit() {
                exponent = (exponent * 10 + (bytes[probe] - b'0') as i32).min(4000);
                probe += 1;
            }
            scale += if negative { -exponent } else { exponent };
            at = probe;
        }
    }
    Some((mantissa * m::pow(10.0, scale as f64), at))
}

fn constant(bytes: &[u8], start: usize) -> Option<(f64, usize)> {
    let rest = &bytes[start + 1..];
    let word_end = rest
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        .unwrap_or(rest.len());
    let word = &rest[..word_end];
    let end = start + 1 + word_end;
    if word.eq_ignore_ascii_case(b"pi") {
        return Some((core::f64::consts::PI, end));
    }
    if word.eq_ignore_ascii_case(b"e") {
        return Some((core::f64::consts::E, end));
    }
    if word.eq_ignore_ascii_case(b"phi") {
        return Some((core::f64::consts::GOLDEN_RATIO, end));
    }
    if word.len() > 1 && (word[0] == b'x' || word[0] == b'X') {
        let mut value = 0u64;
        for digit in &word[1..] {
            value = value.checked_mul(16)? + (*digit as char).to_digit(16)? as u64;
        }
        return Some((value as f64, end));
    }
    if rest.first() == Some(&b'\'') && rest.get(2) == Some(&b'\'') {
        return Some((rest[1] as f64, start + 4));
    }
    None
}

// The parser -----------------------------------------------------------------

/// What ends a statement list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    End,
    /// `)` or `,`, for a function argument or parenthesis.
    Argument,
}

struct Parser<'a, 't> {
    source: &'a str,
    tokens: &'a [Token],
    at: usize,
    table: &'t mut VarTable,
    depth: usize,
}

fn boxed(node: Node) -> Box<Node> {
    Box::new(node)
}

impl<'a> Parser<'a, '_> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.at).copied()
    }

    fn peek_op(&self) -> Option<&'static str> {
        match self.peek()?.kind {
            Kind::Op(op) => Some(op),
            _ => None,
        }
    }

    fn eat(&mut self, op: &str) -> bool {
        if self.peek_op() == Some(op) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn offset(&self) -> usize {
        self.peek().map_or(usize::MAX, |token| token.offset)
    }

    fn unexpected(&self) -> CompileError {
        match self.peek() {
            Some(token) => CompileError::Unexpected(token.offset),
            None => CompileError::UnexpectedEnd,
        }
    }

    fn enter(&mut self) -> Result<(), CompileError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(CompileError::TooDeep)
        } else {
            Ok(())
        }
    }

    /// Statements up to the end, or up to an argument's `,` or `)` (not eaten).
    fn statements(&mut self, stop: Stop) -> Result<Vec<Node>, CompileError> {
        let mut list = Vec::new();
        loop {
            while self.eat(";") {}
            match (self.peek_op(), stop) {
                (None, _) if self.peek().is_none() => break,
                (Some(")") | Some(","), Stop::Argument) => break,
                _ => {}
            }
            let statement = self.expression()?;
            list.try_reserve(1).map_err(|_| CompileError::OutOfMemory)?;
            list.push(statement);
            match self.peek() {
                None => break,
                Some(token) => match token.kind {
                    Kind::Op(";") => {}
                    Kind::Op(")") | Kind::Op(",") if stop == Stop::Argument => break,
                    // A missing ';' at the end of a line.
                    _ if token.line_start => {}
                    _ => return Err(CompileError::Unexpected(token.offset)),
                },
            }
        }
        Ok(list)
    }

    fn expression(&mut self) -> Result<Node, CompileError> {
        self.enter()?;
        let result = self.assignment();
        self.depth -= 1;
        result
    }

    fn assignment(&mut self) -> Result<Node, CompileError> {
        let start = self.offset();
        let target = self.ternary()?;
        let Some(op) = self.peek_op() else {
            return Ok(target);
        };
        let compound = match op {
            "=" => None,
            "+=" => Some(Binary::Add),
            "-=" => Some(Binary::Sub),
            "*=" => Some(Binary::Mul),
            "/=" => Some(Binary::Div),
            "%=" => Some(Binary::Mod),
            "^=" => Some(Binary::Pow),
            "|=" => Some(Binary::BitOr),
            "&=" => Some(Binary::BitAnd),
            _ => return Ok(target),
        };
        self.at += 1;
        let place = match &target {
            Node::Var(slot) => Place::Var(*slot),
            Node::Reg(index) => Place::Reg(*index),
            Node::Mem(global, _) => Place::Mem(*global),
            _ => return Err(CompileError::NotAssignable(start)),
        };
        let value = self.expression()?;
        Ok(match place {
            Place::Var(slot) => {
                let value = match compound {
                    Some(op) => Node::Binary(op, boxed(Node::Var(slot)), boxed(value)),
                    None => value,
                };
                Node::Assign(slot, boxed(value))
            }
            Place::Reg(index) => {
                let value = match compound {
                    Some(op) => Node::Binary(op, boxed(Node::Reg(index)), boxed(value)),
                    None => value,
                };
                Node::AssignReg(index, boxed(value))
            }
            Place::Mem(global) => {
                let Node::Mem(_, index) = target else {
                    return Err(CompileError::NotAssignable(start));
                };
                match compound {
                    // The address is computed twice, which only matters if
                    // computing it has side effects. No preset does that.
                    Some(op) => {
                        let again = self.clone_node(&index)?;
                        Node::AssignMem(
                            global,
                            index,
                            boxed(Node::Binary(op, boxed(Node::Mem(global, again)), boxed(value))),
                        )
                    }
                    None => Node::AssignMem(global, index, boxed(value)),
                }
            }
        })
    }

    fn clone_node(&self, node: &Node) -> Result<Box<Node>, CompileError> {
        fn copy(node: &Node) -> Node {
            match node {
                Node::Const(v) => Node::Const(*v),
                Node::Var(s) => Node::Var(*s),
                Node::Reg(r) => Node::Reg(*r),
                Node::Mem(g, i) => Node::Mem(*g, boxed(copy(i))),
                Node::Assign(s, v) => Node::Assign(*s, boxed(copy(v))),
                Node::AssignReg(r, v) => Node::AssignReg(*r, boxed(copy(v))),
                Node::AssignMem(g, i, v) => Node::AssignMem(*g, boxed(copy(i)), boxed(copy(v))),
                Node::Unary(op, v) => Node::Unary(*op, boxed(copy(v))),
                Node::Binary(op, a, b) => Node::Binary(*op, boxed(copy(a)), boxed(copy(b))),
                Node::And(a, b) => Node::And(boxed(copy(a)), boxed(copy(b))),
                Node::Or(a, b) => Node::Or(boxed(copy(a)), boxed(copy(b))),
                Node::If(c, a, b) => Node::If(boxed(copy(c)), boxed(copy(a)), boxed(copy(b))),
                Node::Call1(f, x) => Node::Call1(*f, boxed(copy(x))),
                Node::Call2(f, a, b) => Node::Call2(*f, boxed(copy(a)), boxed(copy(b))),
                Node::Seq(list) => Node::Seq(list.iter().map(copy).collect()),
                Node::Loop(c, b) => Node::Loop(boxed(copy(c)), boxed(copy(b))),
                Node::While(b) => Node::While(boxed(copy(b))),
            }
        }
        Ok(boxed(copy(node)))
    }

    fn ternary(&mut self) -> Result<Node, CompileError> {
        let condition = self.logical()?;
        if !self.eat("?") {
            return Ok(condition);
        }
        self.enter()?;
        let then = self.assignment()?;
        let otherwise = if self.eat(":") {
            self.assignment()?
        } else {
            Node::Const(0.0)
        };
        self.depth -= 1;
        Ok(Node::If(boxed(condition), boxed(then), boxed(otherwise)))
    }

    fn logical(&mut self) -> Result<Node, CompileError> {
        let mut left = self.comparison()?;
        loop {
            if self.eat("&&") {
                let right = self.comparison()?;
                left = Node::And(boxed(left), boxed(right));
            } else if self.eat("||") {
                let right = self.comparison()?;
                left = Node::Or(boxed(left), boxed(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn comparison(&mut self) -> Result<Node, CompileError> {
        let mut left = self.bitwise()?;
        loop {
            let op = match self.peek_op() {
                Some("==") => Binary::Eq,
                Some("!=") => Binary::Ne,
                Some("<") => Binary::Lt,
                Some(">") => Binary::Gt,
                Some("<=") => Binary::Le,
                Some(">=") => Binary::Ge,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.bitwise()?;
            left = Node::Binary(op, boxed(left), boxed(right));
        }
    }

    fn bitwise(&mut self) -> Result<Node, CompileError> {
        let mut left = self.additive()?;
        loop {
            let op = match self.peek_op() {
                Some("|") => Binary::BitOr,
                Some("&") => Binary::BitAnd,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.additive()?;
            left = Node::Binary(op, boxed(left), boxed(right));
        }
    }

    fn additive(&mut self) -> Result<Node, CompileError> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                // A sign at the start of a new line begins a new statement in
                // code that left out its ';'. In the middle of a line it's
                // subtraction, as ever.
                Some(token) if token.line_start && token.kind == Kind::Op("-") => {
                    if self.line_continues() {
                        Binary::Sub
                    } else {
                        return Ok(left);
                    }
                }
                Some(Token { kind: Kind::Op("+"), .. }) => Binary::Add,
                Some(Token { kind: Kind::Op("-"), .. }) => Binary::Sub,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.multiplicative()?;
            left = Node::Binary(op, boxed(left), boxed(right));
        }
    }

    /// For a line starting with `-`: whether it reads as the rest of the last
    /// line. It does unless it looks like `-name = ...`, which can't be.
    fn line_continues(&self) -> bool {
        !matches!(
            (self.tokens.get(self.at + 1), self.tokens.get(self.at + 2)),
            (Some(Token { kind: Kind::Name(..), .. }), Some(Token { kind: Kind::Op("="), .. }))
        )
    }

    fn multiplicative(&mut self) -> Result<Node, CompileError> {
        let mut left = self.remainder()?;
        loop {
            let op = match self.peek_op() {
                Some("*") => Binary::Mul,
                Some("/") => Binary::Div,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.remainder()?;
            left = Node::Binary(op, boxed(left), boxed(right));
        }
    }

    fn remainder(&mut self) -> Result<Node, CompileError> {
        let mut left = self.power()?;
        while self.eat("%") {
            let right = self.power()?;
            left = Node::Binary(Binary::Mod, boxed(left), boxed(right));
        }
        Ok(left)
    }

    fn power(&mut self) -> Result<Node, CompileError> {
        let base = self.unary()?;
        if self.eat("^") {
            self.enter()?;
            let exponent = self.power()?;
            self.depth -= 1;
            return Ok(Node::Binary(Binary::Pow, boxed(base), boxed(exponent)));
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<Node, CompileError> {
        self.enter()?;
        let result = if self.eat("-") {
            self.unary().map(|value| match value {
                Node::Const(v) => Node::Const(-v),
                other => Node::Unary(Unary::Neg, boxed(other)),
            })
        } else if self.eat("+") {
            self.unary()
        } else if self.eat("!") {
            self.unary().map(|value| Node::Unary(Unary::Not, boxed(value)))
        } else {
            self.primary()
        };
        self.depth -= 1;
        result
    }

    fn primary(&mut self) -> Result<Node, CompileError> {
        let Some(token) = self.peek() else {
            return Err(CompileError::UnexpectedEnd);
        };
        match token.kind {
            Kind::Number(value) => {
                self.at += 1;
                Ok(Node::Const(value))
            }
            Kind::Op("(") => {
                self.at += 1;
                let mut list = self.statements(Stop::Argument)?;
                if !self.eat(")") {
                    return Err(self.unexpected());
                }
                Ok(match list.len() {
                    0 => Node::Const(0.0),
                    1 => list.pop().unwrap_or(Node::Const(0.0)),
                    _ => Node::Seq(list),
                })
            }
            Kind::Name(start, end) => {
                self.at += 1;
                let name = self.name(start, end);
                if self.eat("(") {
                    return self.call(name, token.offset);
                }
                if self.eat("[") {
                    // `name[index]`: EEL2's offset into memory. Presets use it
                    // with megabuf-style addressing, so read it as megabuf.
                    let index = self.expression()?;
                    if !self.eat("]") {
                        return Err(self.unexpected());
                    }
                    let base = self.variable(name)?;
                    return Ok(Node::Mem(
                        false,
                        boxed(Node::Binary(Binary::Add, boxed(base), boxed(index))),
                    ));
                }
                self.variable(name)
            }
            _ => Err(CompileError::Unexpected(token.offset)),
        }
    }

    fn name(&self, start: usize, end: usize) -> &'a str {
        &self.source[start..end]
    }

    fn variable(&mut self, name: &str) -> Result<Node, CompileError> {
        if let Some(index) = register(name) {
            return Ok(Node::Reg(index));
        }
        let slot = self.table.slot(name).ok_or(CompileError::OutOfMemory)?;
        Ok(Node::Var(slot))
    }

    /// Arguments up to the `)`, which is eaten.
    fn arguments(&mut self) -> Result<Vec<Node>, CompileError> {
        let mut arguments = Vec::new();
        if self.eat(")") {
            return Ok(arguments);
        }
        loop {
            let mut list = self.statements(Stop::Argument)?;
            let argument = match list.len() {
                0 => Node::Const(0.0),
                1 => list.pop().unwrap_or(Node::Const(0.0)),
                _ => Node::Seq(list),
            };
            arguments.try_reserve(1).map_err(|_| CompileError::OutOfMemory)?;
            arguments.push(argument);
            if self.eat(")") {
                return Ok(arguments);
            }
            if !self.eat(",") {
                return Err(self.unexpected());
            }
        }
    }

    fn call(&mut self, name: &str, offset: usize) -> Result<Node, CompileError> {
        self.enter()?;
        let arguments = self.arguments()?;
        self.depth -= 1;
        let lower = |text: &str| name.eq_ignore_ascii_case(text);
        let count = arguments.len();
        let mut arguments = arguments.into_iter();
        let mut next = || boxed(arguments.next().unwrap_or(Node::Const(0.0)));
        let one = |func| (count == 1).then_some(func);
        let two = |func| (count == 2).then_some(func);
        let unary = [
            ("sin", Func1::Sin),
            ("cos", Func1::Cos),
            ("tan", Func1::Tan),
            ("asin", Func1::Asin),
            ("acos", Func1::Acos),
            ("atan", Func1::Atan),
            ("sqrt", Func1::Sqrt),
            ("sqr", Func1::Sqr),
            ("abs", Func1::Abs),
            ("sign", Func1::Sign),
            ("exp", Func1::Exp),
            ("log", Func1::Log),
            ("log10", Func1::Log10),
            ("int", Func1::Int),
            ("floor", Func1::Floor),
            ("ceil", Func1::Ceil),
            ("rand", Func1::Rand),
            ("bnot", Func1::Bnot),
            ("invsqrt", Func1::Invsqrt),
        ];
        if let Some(&(_, func)) = unary.iter().find(|(known, _)| lower(known)) {
            let func = one(func).ok_or(CompileError::UnknownFunction(offset))?;
            return Ok(Node::Call1(func, next()));
        }
        let binary = [
            ("atan2", Func2::Atan2),
            ("pow", Func2::Pow),
            ("min", Func2::Min),
            ("max", Func2::Max),
            ("above", Func2::Above),
            ("below", Func2::Below),
            ("equal", Func2::Equal),
            ("band", Func2::Band),
            ("bor", Func2::Bor),
            ("sigmoid", Func2::Sigmoid),
            ("fmod", Func2::Fmod),
        ];
        if let Some(&(_, func)) = binary.iter().find(|(known, _)| lower(known)) {
            let func = two(func).ok_or(CompileError::UnknownFunction(offset))?;
            let a = next();
            let b = next();
            return Ok(Node::Call2(func, a, b));
        }
        if lower("if") && count == 3 {
            let condition = next();
            let then = next();
            let otherwise = next();
            return Ok(Node::If(condition, then, otherwise));
        }
        if (lower("megabuf") || lower("gmegabuf")) && count == 1 {
            return Ok(Node::Mem(lower("gmegabuf"), next()));
        }
        if lower("loop") && count == 2 {
            let times = next();
            let body = next();
            return Ok(Node::Loop(times, body));
        }
        if lower("while") && count == 1 {
            return Ok(Node::While(next()));
        }
        if (lower("exec2") && count == 2) || (lower("exec3") && count == 3) {
            let mut list = Vec::new();
            list.try_reserve(count).map_err(|_| CompileError::OutOfMemory)?;
            for _ in 0..count {
                list.push(*next());
            }
            return Ok(Node::Seq(list));
        }
        if lower("freembuf") || lower("memset") || lower("memcpy") {
            // Memory management that a growable buffer doesn't need. The
            // arguments still run, for any side effects in them.
            let mut list = Vec::new();
            list.try_reserve(count + 1).map_err(|_| CompileError::OutOfMemory)?;
            for _ in 0..count {
                list.push(*next());
            }
            list.push(Node::Const(0.0));
            return Ok(Node::Seq(list));
        }
        Err(CompileError::UnknownFunction(offset))
    }
}

/// `reg00`..`reg99`, which are global.
fn register(name: &str) -> Option<u8> {
    let bytes = name.as_bytes();
    if bytes.len() != 5 || !bytes[..3].eq_ignore_ascii_case(b"reg") {
        return None;
    }
    let (tens, ones) = (bytes[3], bytes[4]);
    if !tens.is_ascii_digit() || !ones.is_ascii_digit() {
        return None;
    }
    Some((tens - b'0') * 10 + (ones - b'0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `code` in a fresh context and returns the variable `out`, or the
    /// last value when there's no `out`.
    fn run(code: &str) -> f64 {
        let mut table = VarTable::with_builtins(&["out"]);
        let program = Program::compile(code, &mut table).unwrap_or_else(|e| panic!("{code}: {e:?}"));
        let mut env = Env::for_table(&table).unwrap();
        let mut globals = Globals::default();
        let last = program.run(&mut env, &mut globals);
        if code.contains("out") {
            env.get(0)
        } else {
            last
        }
    }

    #[test]
    fn arithmetic_follows_eel_precedence() {
        assert_eq!(run("out = 1 + 2 * 3"), 7.0);
        assert_eq!(run("out = 2 ^ 3 ^ 2"), 512.0);
        assert_eq!(run("out = -2 ^ 2"), 4.0);
        assert_eq!(run("out = 7 % 4 * 2"), 6.0);
        assert_eq!(run("out = (1 + 2) * 3"), 9.0);
        assert_eq!(run("out = 1 < 2 && 3 > 2"), 1.0);
        assert_eq!(run("out = 5 | 2"), 7.0);
        assert_eq!(run("out = 6 & 3"), 2.0);
        assert_eq!(run("out = 1 ? 10 : 20"), 10.0);
        assert_eq!(run("out = 0 ? 10 : 20"), 20.0);
        assert_eq!(run("out = !0 + !5"), 1.0);
    }

    #[test]
    fn milkdrop_forgives_bad_arithmetic() {
        assert_eq!(run("out = 1 / 0"), 0.0);
        assert_eq!(run("out = 5 % 0"), 0.0);
        assert_eq!(run("out = sqrt(-4)"), 2.0);
        assert_eq!(run("out = pow(-8, 0.5)"), 0.0);
        assert_eq!(run("out = equal(1, 1.000001)"), 1.0);
    }

    #[test]
    fn variables_are_case_insensitive_and_persist() {
        let mut table = VarTable::with_builtins(&["zoom"]);
        let program = Program::compile("Zoom = ZOOM + 1; counter = counter + 1;", &mut table).unwrap();
        let mut env = Env::for_table(&table).unwrap();
        let mut globals = Globals::default();
        program.run(&mut env, &mut globals);
        program.run(&mut env, &mut globals);
        assert_eq!(env.get(0), 2.0);
        assert_eq!(env.get(table.find("COUNTER").unwrap() as usize), 2.0);
    }

    #[test]
    fn functions_and_control_flow() {
        assert_eq!(run("out = if(above(3, 2), 1, 2)"), 1.0);
        assert_eq!(run("out = min(3, max(1, 2))"), 2.0);
        assert_eq!(run("out = 0; loop(5, out = out + 2)"), 10.0);
        assert_eq!(run("i = 0; out = 0; while(i = i + 1; out = out + i; i < 4)"), 10.0);
        assert_eq!(run("out = exec2(a = 3, a * 2)"), 6.0);
        assert!((run("out = sin($pi / 2)") - 1.0).abs() < 1e-12);
        assert_eq!(run("out = sign(-3) + abs(-2) + sqr(3) + int(2.7)"), 12.0);
        assert_eq!(run("out = band(1, 0) + bor(1, 0) + bnot(0)"), 2.0);
        let r = run("out = rand(10)");
        assert!((0.0..10.0).contains(&r) && r == r.trunc());
    }

    #[test]
    fn assignment_forms() {
        assert_eq!(run("out = 2; out += 3; out *= 4; out -= 1; out /= 2"), 9.5);
        assert_eq!(run("a = b = 4; out = a + b"), 8.0);
        assert_eq!(run("megabuf(10) = 7; megabuf(10) += 1; out = megabuf(10)"), 8.0);
        assert_eq!(run("gmegabuf(3) = 2; out = gmegabuf(3) + megabuf(3)"), 2.0);
        assert_eq!(run("reg05 = 4; out = REG05 * 2"), 8.0);
        assert_eq!(run("out = 0; if(1, out = 3, out = 4)"), 3.0);
    }

    #[test]
    fn registers_are_shared_between_programs() {
        let mut table = VarTable::default();
        let write = Program::compile("reg42 = 6;", &mut table).unwrap();
        let mut other = VarTable::default();
        let read = Program::compile("reg42 * 7", &mut other).unwrap();
        let mut globals = Globals::default();
        write.run(&mut Env::for_table(&table).unwrap(), &mut globals);
        assert_eq!(read.run(&mut Env::for_table(&other).unwrap(), &mut globals), 42.0);
    }

    #[test]
    fn lines_without_semicolons_still_separate() {
        assert_eq!(run("a = 1\nb = 2\nout = a + b"), 3.0);
        assert_eq!(run("out = 5\n- 2"), 3.0);
        assert_eq!(run("out = 1;;; ;"), 1.0);
    }

    #[test]
    fn numbers_and_comments() {
        assert_eq!(run("out = .5 + 1.5e1 + 2E-1"), 15.7);
        assert_eq!(run("out = $x10 + $'A'"), 81.0);
        assert_eq!(run("out = 1 // the rest is a comment\n + 1"), 2.0);
        assert_eq!(run("out = /* inline */ 3"), 3.0);
    }

    #[test]
    fn malformed_code_is_an_error_not_a_panic() {
        let mut table = VarTable::default();
        for code in ["out = ", "(1 + 2", "sin(1, 2)", "nosuch(1)", "3 = 4", "1 + * 2", ")"] {
            assert!(Program::compile(code, &mut table).is_err(), "{code} compiled");
        }
        let deep = "(".repeat(5000);
        assert!(Program::compile(&deep, &mut table).is_err());
    }
}
