//! Integer arithmetic for `$(( ))`, `(( ))`, `let` and `${x:off:len}`.
//!
//! The grammar is C's, on wrapping 64-bit integers. Evaluation happens during
//! parsing; a `no_eval` count switches off side effects and division errors on
//! the untaken side of `&&`, `||` and `?:`. Nesting is bounded, so a hostile
//! expression is an error and not a stack overflow.

use alloc::format;
use alloc::string::{String, ToString};

use feos_coreutils::Env;

/// Deepest nesting of parentheses, unary operators, and variable references.
const MAX_DEPTH: usize = 32;

/// Evaluates `text`, reading and assigning variables in `env`.
pub(crate) fn evaluate(text: &str, env: &mut Env) -> Result<i64, String> {
    evaluate_at(text, env, 0)
}

fn evaluate_at(text: &str, env: &mut Env, depth: usize) -> Result<i64, String> {
    let mut parser = Arithmetic {
        bytes: text.as_bytes(),
        index: 0,
        env,
        depth,
        no_eval: 0,
    };
    parser.skip();
    if parser.at_end() {
        return Ok(0);
    }
    let value = parser.comma()?;
    parser.skip();
    if !parser.at_end() {
        let rest = core::str::from_utf8(&parser.bytes[parser.index..]).unwrap_or("?");
        return Err(format!(
            "syntax error in expression (error token is \"{rest}\")"
        ));
    }
    Ok(value)
}

struct Arithmetic<'a, 'e> {
    bytes: &'a [u8],
    index: usize,
    env: &'e mut Env,
    depth: usize,
    no_eval: usize,
}

/// A binary operator's precedence, with higher binding tighter.
fn binary_precedence(op: &str) -> Option<u8> {
    Some(match op {
        "||" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" => 6,
        "<" | "<=" | ">" | ">=" => 7,
        "<<" | ">>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        "**" => 11,
        _ => return None,
    })
}

/// Raises `base` to `exponent` with wrapping, by repeated squaring.
fn power(mut base: i64, mut exponent: i64) -> i64 {
    let mut result = 1i64;
    while exponent > 0 {
        if exponent & 1 == 1 {
            result = result.wrapping_mul(base);
        }
        base = base.wrapping_mul(base);
        exponent >>= 1;
    }
    result
}

impl Arithmetic<'_, '_> {
    fn at_end(&self) -> bool {
        self.index >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn skip(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.index += 1;
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(String::from("expression too deeply nested"));
        }
        Ok(())
    }

    fn comma(&mut self) -> Result<i64, String> {
        let mut value = self.assignment()?;
        loop {
            self.skip();
            if self.peek() != Some(b',') {
                return Ok(value);
            }
            self.index += 1;
            value = self.assignment()?;
        }
    }

    fn identifier_at(&self, start: usize) -> usize {
        let mut end = start;
        while self
            .bytes
            .get(end)
            .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
        {
            end += 1;
        }
        end
    }

    fn assignment(&mut self) -> Result<i64, String> {
        self.skip();
        self.enter()?;
        let value = self.assignment_inner();
        self.depth -= 1;
        value
    }

    fn assignment_inner(&mut self) -> Result<i64, String> {
        // An assignment is a name followed by an assignment operator.
        if self
            .peek()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        {
            let end = self.identifier_at(self.index);
            let mut after = end;
            while self.bytes.get(after).is_some_and(u8::is_ascii_whitespace) {
                after += 1;
            }
            let rest = self.bytes.get(after..).unwrap_or(&[]);
            let operator = [
                &b"<<="[..],
                b">>=",
                b"+=",
                b"-=",
                b"*=",
                b"/=",
                b"%=",
                b"&=",
                b"^=",
                b"|=",
                b"=",
            ]
            .into_iter()
            .find(|op| rest.starts_with(op) && !(op.len() == 1 && rest.get(1) == Some(&b'=')));
            if let Some(operator) = operator {
                let name = core::str::from_utf8(&self.bytes[self.index..end])
                    .unwrap_or("")
                    .to_string();
                self.index = after + operator.len();
                let right = self.assignment()?;
                let value = if operator == b"=" {
                    right
                } else {
                    let left = self.variable(&name)?;
                    let op = core::str::from_utf8(&operator[..operator.len() - 1]).unwrap_or("");
                    self.apply(op, left, right)?
                };
                self.store(&name, value)?;
                return Ok(value);
            }
        }
        self.ternary()
    }

    fn ternary(&mut self) -> Result<i64, String> {
        let condition = self.binary(1)?;
        self.skip();
        if self.peek() != Some(b'?') {
            return Ok(condition);
        }
        self.index += 1;
        if condition == 0 {
            self.no_eval += 1;
        }
        let first = self.assignment();
        if condition == 0 {
            self.no_eval -= 1;
        }
        let first = first?;
        self.skip();
        if self.peek() != Some(b':') {
            return Err(String::from("expected `:` in conditional expression"));
        }
        self.index += 1;
        if condition != 0 {
            self.no_eval += 1;
        }
        let second = self.assignment();
        if condition != 0 {
            self.no_eval -= 1;
        }
        let second = second?;
        Ok(if condition != 0 { first } else { second })
    }

    /// Reads a binary operator without consuming it.
    fn peek_operator(&mut self) -> Option<&'static str> {
        self.skip();
        let rest = self.bytes.get(self.index..)?;
        [
            "**", "||", "&&", "==", "!=", "<=", ">=", "<<", ">>", "|", "^", "&", "<", ">", "+",
            "-", "*", "/", "%",
        ]
        .into_iter()
        .find(|op| rest.starts_with(op.as_bytes()))
    }

    fn binary(&mut self, minimum: u8) -> Result<i64, String> {
        let mut left = self.unary()?;
        loop {
            let Some(op) = self.peek_operator() else {
                return Ok(left);
            };
            let Some(precedence) = binary_precedence(op).filter(|level| *level >= minimum) else {
                return Ok(left);
            };
            self.index += op.len();

            if op == "&&" || op == "||" {
                let decided = if op == "&&" { left == 0 } else { left != 0 };
                if decided {
                    self.no_eval += 1;
                }
                let right = self.binary(precedence + 1);
                if decided {
                    self.no_eval -= 1;
                }
                let right = right?;
                left = i64::from(if op == "&&" {
                    left != 0 && right != 0
                } else {
                    left != 0 || right != 0
                });
                continue;
            }

            // `**` is right-associative.
            let next = if op == "**" {
                precedence
            } else {
                precedence + 1
            };
            let right = self.binary(next)?;
            left = self.apply(op, left, right)?;
        }
    }

    fn apply(&self, op: &str, left: i64, right: i64) -> Result<i64, String> {
        Ok(match op {
            "|" => left | right,
            "^" => left ^ right,
            "&" => left & right,
            "==" => i64::from(left == right),
            "!=" => i64::from(left != right),
            "<" => i64::from(left < right),
            "<=" => i64::from(left <= right),
            ">" => i64::from(left > right),
            ">=" => i64::from(left >= right),
            "<<" => left.wrapping_shl(right as u32),
            ">>" => left.wrapping_shr(right as u32),
            "+" => left.wrapping_add(right),
            "-" => left.wrapping_sub(right),
            "*" => left.wrapping_mul(right),
            "/" | "%" => {
                if right == 0 {
                    if self.no_eval > 0 {
                        return Ok(0);
                    }
                    return Err(String::from("division by 0"));
                }
                if op == "/" {
                    left.wrapping_div(right)
                } else {
                    left.wrapping_rem(right)
                }
            }
            "**" => {
                if right < 0 {
                    if self.no_eval > 0 {
                        return Ok(0);
                    }
                    return Err(String::from("exponent less than 0"));
                }
                power(left, right)
            }
            _ => return Err(format!("unknown operator '{op}'")),
        })
    }

    fn unary(&mut self) -> Result<i64, String> {
        self.skip();
        let Some(byte) = self.peek() else {
            return Err(String::from("syntax error: operand expected"));
        };
        let second = self.bytes.get(self.index + 1).copied();
        match (byte, second) {
            (b'+', Some(b'+')) | (b'-', Some(b'-')) => {
                self.index += 2;
                self.skip();
                let name = self.name()?;
                let current = self.variable(&name)?;
                let value = if byte == b'+' {
                    current.wrapping_add(1)
                } else {
                    current.wrapping_sub(1)
                };
                self.store(&name, value)?;
                Ok(value)
            }
            (b'-' | b'+' | b'!' | b'~', _) => {
                self.index += 1;
                self.enter()?;
                let operand = self.unary();
                self.depth -= 1;
                let operand = operand?;
                Ok(match byte {
                    b'-' => operand.wrapping_neg(),
                    b'+' => operand,
                    b'!' => i64::from(operand == 0),
                    _ => !operand,
                })
            }
            _ => self.primary(),
        }
    }

    fn name(&mut self) -> Result<String, String> {
        let start = self.index;
        let end = self.identifier_at(start);
        let first = self.bytes.get(start);
        if end == start || first.is_some_and(u8::is_ascii_digit) {
            return Err(String::from("syntax error: variable expected"));
        }
        self.index = end;
        Ok(core::str::from_utf8(&self.bytes[start..end])
            .unwrap_or("")
            .to_string())
    }

    fn primary(&mut self) -> Result<i64, String> {
        self.skip();
        match self.peek() {
            Some(b'(') => {
                self.index += 1;
                self.enter()?;
                let value = self.comma();
                self.depth -= 1;
                let value = value?;
                self.skip();
                if self.peek() != Some(b')') {
                    return Err(String::from("missing `)`"));
                }
                self.index += 1;
                Ok(value)
            }
            Some(byte) if byte.is_ascii_digit() => self.number(),
            Some(byte) if byte == b'_' || byte.is_ascii_alphabetic() => {
                let name = self.name()?;
                let current = self.variable(&name)?;
                self.skip();
                let rest = self.bytes.get(self.index..).unwrap_or(&[]);
                if rest.starts_with(b"++") || rest.starts_with(b"--") {
                    let up = rest[0] == b'+';
                    self.index += 2;
                    let value = if up {
                        current.wrapping_add(1)
                    } else {
                        current.wrapping_sub(1)
                    };
                    self.store(&name, value)?;
                    return Ok(current);
                }
                Ok(current)
            }
            Some(_) => {
                let rest = core::str::from_utf8(&self.bytes[self.index..]).unwrap_or("?");
                Err(format!(
                    "syntax error: operand expected (error token is \"{rest}\")"
                ))
            }
            None => Err(String::from("syntax error: operand expected")),
        }
    }

    fn number(&mut self) -> Result<i64, String> {
        let start = self.index;
        let end = self.identifier_at(start);
        let mut word_end = end;
        // `base#digits`
        if self.bytes.get(end) == Some(&b'#') {
            word_end = self.identifier_at(end + 1);
        }
        self.index = word_end;
        let word = core::str::from_utf8(&self.bytes[start..word_end]).unwrap_or("");
        parse_literal(word).ok_or_else(|| format!("{word}: value too great for base"))
    }

    fn variable(&mut self, name: &str) -> Result<i64, String> {
        let Some(value) = self.env.get(name).map(str::to_string) else {
            return Ok(0);
        };
        if value.is_empty() {
            return Ok(0);
        }
        if let Some(number) = parse_literal(value.trim()) {
            return Ok(number);
        }
        // A value may itself be an expression, as in `a=b+1; $((a))`.
        if self.depth >= MAX_DEPTH {
            return Err(format!("{name}: expression recursion level exceeded"));
        }
        evaluate_at(&value, self.env, self.depth + 1)
    }

    fn store(&mut self, name: &str, value: i64) -> Result<(), String> {
        if self.no_eval > 0 {
            return Ok(());
        }
        self.env
            .try_set(name, &value.to_string())
            .map_err(|error| format!("{name}: {}", error.text()))
    }
}

/// Parses a decimal, `0x` hex, `0` octal or `base#digits` literal, wrapping on
/// overflow as C-based shells do.
fn parse_literal(word: &str) -> Option<i64> {
    let (negative, word) = match word.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, word),
    };
    let (radix, digits) = if let Some((base, digits)) = word.split_once('#') {
        let base: u32 = base.parse().ok()?;
        if !(2..=64).contains(&base) {
            return None;
        }
        (base, digits)
    } else if let Some(hex) = word.strip_prefix("0x").or(word.strip_prefix("0X")) {
        (16, hex)
    } else if word.len() > 1 && word.starts_with('0') {
        (8, &word[1..])
    } else {
        (10, word)
    };
    if digits.is_empty() {
        return None;
    }
    let mut value = 0i64;
    for byte in digits.bytes() {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'z' => u32::from(byte - b'a') + 10,
            b'A'..=b'Z' => {
                if radix <= 36 {
                    u32::from(byte - b'A') + 10
                } else {
                    u32::from(byte - b'A') + 36
                }
            }
            b'@' => 62,
            b'_' => 63,
            _ => return None,
        };
        if digit >= radix {
            return None;
        }
        value = value
            .wrapping_mul(i64::from(radix))
            .wrapping_add(i64::from(digit));
    }
    Some(if negative { value.wrapping_neg() } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(text: &str) -> Result<i64, String> {
        evaluate(text, &mut Env::new())
    }

    fn value(text: &str) -> i64 {
        eval(text).unwrap()
    }

    #[test]
    fn follows_c_precedence() {
        assert_eq!(value("1 + 2 * 3"), 7);
        assert_eq!(value("(1 + 2) * 3"), 9);
        assert_eq!(value("10 / 3"), 3);
        assert_eq!(value("-4 + 10"), 6);
        assert_eq!(value("2 ** 3 ** 2"), 512);
        assert_eq!(value("1 << 2 + 1"), 8);
        assert_eq!(value("6 & 3 | 8"), 10);
        assert_eq!(value("1 == 1 && 2 > 1"), 1);
        assert_eq!(value("3 > 5"), 0);
        assert_eq!(value("!0 + ~0"), 0);
        assert_eq!(value("1 ? 2 : 3"), 2);
        assert_eq!(value("0 ? 2 : 0 ? 3 : 4"), 4);
        assert_eq!(value("1, 2, 3"), 3);
        assert_eq!(value("0x10 + 010 + 2#101"), 29);
    }

    #[test]
    fn assignments_update_variables() {
        let mut env = Env::new();
        assert_eq!(evaluate("a = 3", &mut env), Ok(3));
        assert_eq!(evaluate("a += 2", &mut env), Ok(5));
        assert_eq!(evaluate("a <<= 1", &mut env), Ok(10));
        assert_eq!(evaluate("a++", &mut env), Ok(10));
        assert_eq!(evaluate("++a", &mut env), Ok(12));
        assert_eq!(evaluate("b = a--", &mut env), Ok(12));
        assert_eq!(env.get("a"), Some("11"));
        assert_eq!(env.get("b"), Some("12"));
        assert_eq!(evaluate("a == 11", &mut env), Ok(1));
    }

    #[test]
    fn short_circuits_skip_side_effects() {
        let mut env = Env::new();
        assert_eq!(evaluate("0 && (a = 1)", &mut env), Ok(0));
        assert_eq!(evaluate("1 || (a = 1)", &mut env), Ok(1));
        assert_eq!(evaluate("1 ? 2 : (a = 1)", &mut env), Ok(2));
        assert_eq!(env.get("a"), None);
        assert_eq!(evaluate("0 && 1 / 0", &mut env), Ok(0));
    }

    #[test]
    fn variables_can_hold_expressions() {
        let mut env = Env::new();
        env.set("a", "b + 1");
        env.set("b", "2");
        assert_eq!(evaluate("a * 2", &mut env), Ok(6));
        env.set("loop", "loop");
        assert!(evaluate("loop", &mut env).is_err());
    }

    #[test]
    fn division_overflow_wraps_instead_of_panicking() {
        assert_eq!(value("(-9223372036854775807 - 1) / -1"), i64::MIN);
        assert_eq!(value("(-9223372036854775807 - 1) % -1"), 0);
        assert_eq!(value("-(-9223372036854775807 - 1)"), i64::MIN);
        assert_eq!(value("9223372036854775807 + 1"), i64::MIN);
        assert_eq!(value("1 << 64"), 1);
    }

    #[test]
    fn errors_are_reported() {
        assert!(eval("1 / 0").unwrap_err().contains("division by 0"));
        assert!(eval("1 % 0").is_err());
        assert!(eval("2 ** -1").is_err());
        assert!(eval("1 +").is_err());
        assert!(eval("1 2").is_err());
        assert!(eval("(1").is_err());
        assert!(eval("09").is_err());
        assert!(eval("$").is_err());
        assert_eq!(eval(""), Ok(0));
    }

    #[test]
    fn nesting_is_bounded() {
        assert!(eval(&"(".repeat(100_000)).is_err());
        assert!(eval(&"-".repeat(100_001)).is_err());
        assert!(eval(&"! ".repeat(100_000)).is_err());
        assert!(eval(&"a=".repeat(100_000)).is_err());
        assert!(eval(&"1?".repeat(100_000)).is_err());
    }
}
