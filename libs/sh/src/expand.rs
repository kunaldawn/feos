//! Word expansion: parameters, command substitution, arithmetic, splitting
//! and pathname generation.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_coreutils::glob::{glob_match, has_glob};
use feos_coreutils::{BytesOutput, Vfs};

use crate::lexer::{Segment, Word};
use crate::{Host, Io, Shell};

impl Shell<'_> {
    /// Expands one word into zero or more command arguments.
    pub(crate) fn expand_word(
        &mut self,
        word: &Word,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Vec<String> {
        let mut parts: Vec<(String, bool)> = Vec::new();

        for segment in &word.segments {
            match segment {
                Segment::Literal(text, quoted) => parts.push((text.clone(), *quoted)),
                Segment::Tilde => {
                    parts.push((self.env.get("HOME").unwrap_or("/").to_string(), true))
                }
                Segment::Var(name, quoted) => {
                    parts.push((self.env.get(name).unwrap_or("").to_string(), *quoted))
                }
                Segment::Special(ch, quoted) => parts.push((self.special_value(*ch), *quoted)),
                Segment::Param(text, quoted) => parts.push((self.expand_param(text), *quoted)),
                Segment::CommandSub(script, quoted) => {
                    let output = self.capture(script, io, host);
                    parts.push((output, *quoted));
                }
                Segment::Arith(expression, quoted) => {
                    parts.push((self.evaluate_arithmetic(expression).to_string(), *quoted));
                }
            }
        }

        let mut fields: Vec<(String, bool)> = Vec::new();
        let mut current = String::new();
        let mut has_field = false;
        let mut glob_ok = false;

        for (text, quoted) in parts {
            if quoted {
                if text.is_empty() {
                    has_field = true;
                } else {
                    current.push_str(&text);
                    has_field = true;
                }
            } else {
                for ch in text.chars() {
                    if ch.is_whitespace() {
                        if has_field {
                            fields.push((core::mem::take(&mut current), glob_ok));
                            has_field = false;
                            glob_ok = false;
                        }
                    } else {
                        current.push(ch);
                        has_field = true;
                        glob_ok = true;
                    }
                }
            }
        }
        if has_field {
            fields.push((current, glob_ok));
        }

        let mut result = Vec::new();
        for (field, glob_ok) in fields {
            if glob_ok && has_glob(&field) {
                let matches = self.glob_path(&field);
                if !matches.is_empty() {
                    result.extend(matches);
                    continue;
                }
            }
            result.push(field);
        }
        result
    }

    /// Expands a word and returns its first field.
    pub(crate) fn expand_one(
        &mut self,
        word: &Word,
        io: &mut Io<'_>,
        host: &mut dyn Host,
    ) -> Option<String> {
        self.expand_word(word, io, host).into_iter().next()
    }

    fn special_value(&self, ch: char) -> String {
        match ch {
            '?' => self.last_status.to_string(),
            '$' => self.system.pid().to_string(),
            '#' => String::from("0"),
            '0' => String::from("sh"),
            _ => String::new(),
        }
    }

    fn expand_param(&mut self, text: &str) -> String {
        if let Some(name) = text.strip_prefix('#') {
            return self
                .env
                .get(name)
                .map(|value| value.chars().count().to_string())
                .unwrap_or_else(|| String::from("0"));
        }

        if let Some((name, default)) = text.split_once(":-") {
            return match self.env.get(name) {
                Some(value) if !value.is_empty() => value.to_string(),
                _ => default.to_string(),
            };
        }
        if let Some((name, default)) = text.split_once(":=") {
            let value = match self.env.get(name) {
                Some(value) if !value.is_empty() => value.to_string(),
                _ => default.to_string(),
            };
            self.env.set(name, &value);
            return value;
        }
        if let Some((name, alternate)) = text.split_once(":+") {
            return match self.env.get(name) {
                Some(value) if !value.is_empty() => alternate.to_string(),
                _ => String::new(),
            };
        }
        if let Some((name, message)) = text.split_once(":?") {
            if self
                .env
                .get(name)
                .map(|value| !value.is_empty())
                .unwrap_or(false)
            {
                return self.env.get(name).unwrap_or("").to_string();
            }
            return message.to_string();
        }

        self.env.get(text).unwrap_or("").to_string()
    }

    fn capture(&mut self, script: &str, io: &mut Io<'_>, host: &mut dyn Host) -> String {
        let saved_status = self.last_status;
        let saved_exit = self.exit_requested;
        let saved_return = self.return_requested;
        let saved_break = self.break_level;
        let saved_continue = self.continue_level;

        self.exit_requested = None;
        self.return_requested = None;
        self.break_level = 0;
        self.continue_level = 0;

        let mut output = BytesOutput::new();
        let mut sub = Io {
            stdin: &mut *io.stdin,
            stdout: &mut output,
            stderr: &mut *io.stderr,
        };
        self.run_text(script, &mut sub, host);
        self.last_status = saved_status;
        self.exit_requested = saved_exit;
        self.return_requested = saved_return;
        self.break_level = saved_break;
        self.continue_level = saved_continue;
        let data = output.into_bytes();
        let mut end = data.len();
        while end > 0 && data[end - 1] == b'\n' {
            end -= 1;
        }
        String::from_utf8_lossy(&data[..end]).into_owned()
    }

    fn glob_path(&mut self, pattern: &str) -> Vec<String> {
        let absolute = pattern.starts_with('/');
        let mut paths: Vec<String> = alloc::vec![if absolute {
            String::from("/")
        } else {
            String::new()
        }];

        let components: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
        let last = components.len().saturating_sub(1);

        for (index, component) in components.iter().enumerate() {
            let is_last = index == last;
            if *component == "." {
                continue;
            }
            if *component == ".." {
                for path in paths.iter_mut() {
                    *path = if path.is_empty() || path == "/" {
                        String::from(if path.is_empty() { ".." } else { "/" })
                    } else {
                        feos_coreutils::parent(path).to_string()
                    };
                }
                continue;
            }

            if has_glob(component) {
                let mut next = Vec::new();
                for path in &paths {
                    let lookup = if path.is_empty() { "." } else { path.as_str() };
                    let Ok(entries) = self.fs.read_dir(lookup) else {
                        continue;
                    };
                    for entry in entries {
                        if entry.name.starts_with('.') && !component.starts_with('.') {
                            continue;
                        }
                        if glob_match(component, &entry.name) {
                            next.push(join_display(path, &entry.name));
                        }
                    }
                }
                paths = next;
            } else {
                let mut next = Vec::new();
                for path in &paths {
                    next.push(join_display(path, component));
                }
                paths = next;
            }

            paths.retain(|path| {
                let lookup = if path.is_empty() { "." } else { path.as_str() };
                match self.fs.meta(lookup) {
                    Ok(meta) => is_last || meta.kind == feos_coreutils::Kind::Dir,
                    Err(_) => false,
                }
            });
        }

        paths.sort();
        paths.dedup();
        paths
    }

    /// Evaluates the integer arithmetic supported in `$((...))`.
    pub(crate) fn evaluate_arithmetic(&self, expression: &str) -> i64 {
        let mut parser = Arithmetic {
            bytes: expression.as_bytes(),
            index: 0,
            env: &self.env,
        };
        parser.expression()
    }
}

fn join_display(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_string()
    } else if directory == "/" {
        format!("/{name}")
    } else {
        format!("{directory}/{name}")
    }
}

struct Arithmetic<'a> {
    bytes: &'a [u8],
    index: usize,
    env: &'a feos_coreutils::Env,
}

impl Arithmetic<'_> {
    fn expression(&mut self) -> i64 {
        let mut value = self.term();
        loop {
            self.skip();
            match self.bytes.get(self.index) {
                Some(b'+') => {
                    self.index += 1;
                    value = value.wrapping_add(self.term());
                }
                Some(b'-') => {
                    self.index += 1;
                    value = value.wrapping_sub(self.term());
                }
                _ => return value,
            }
        }
    }

    fn term(&mut self) -> i64 {
        let mut value = self.unary();
        loop {
            self.skip();
            match self.bytes.get(self.index) {
                Some(b'*') => {
                    self.index += 1;
                    value = value.wrapping_mul(self.unary());
                }
                Some(b'/') => {
                    self.index += 1;
                    let right = self.unary();
                    value = if right == 0 { 0 } else { value / right };
                }
                Some(b'%') => {
                    self.index += 1;
                    let right = self.unary();
                    value = if right == 0 { 0 } else { value % right };
                }
                _ => return value,
            }
        }
    }

    fn unary(&mut self) -> i64 {
        self.skip();
        match self.bytes.get(self.index) {
            Some(b'-') => {
                self.index += 1;
                -self.unary()
            }
            Some(b'+') => {
                self.index += 1;
                self.unary()
            }
            Some(b'!') => {
                self.index += 1;
                i64::from(self.unary() == 0)
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> i64 {
        self.skip();
        match self.bytes.get(self.index) {
            Some(b'(') => {
                self.index += 1;
                let value = self.expression();
                self.skip();
                if self.bytes.get(self.index) == Some(&b')') {
                    self.index += 1;
                }
                value
            }
            Some(byte) if byte.is_ascii_digit() => {
                let start = self.index;
                while self
                    .bytes
                    .get(self.index)
                    .map(|byte| byte.is_ascii_digit())
                    .unwrap_or(false)
                {
                    self.index += 1;
                }
                core::str::from_utf8(&self.bytes[start..self.index])
                    .ok()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(0)
            }
            Some(byte) if *byte == b'_' || byte.is_ascii_alphabetic() => {
                let start = self.index;
                while self
                    .bytes
                    .get(self.index)
                    .map(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
                    .unwrap_or(false)
                {
                    self.index += 1;
                }
                let name = core::str::from_utf8(&self.bytes[start..self.index]).unwrap_or("");
                self.env
                    .get(name)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn skip(&mut self) {
        while self
            .bytes
            .get(self.index)
            .map(|byte| byte.is_ascii_whitespace())
            .unwrap_or(false)
        {
            self.index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{run, NoHost};
    use feos_coreutils::NoSystem;

    #[test]
    fn arithmetic_precedence() {
        let shell = Shell::new(None, &NoSystem);
        assert_eq!(shell.evaluate_arithmetic("1 + 2 * 3"), 7);
        assert_eq!(shell.evaluate_arithmetic("(1 + 2) * 3"), 9);
        assert_eq!(shell.evaluate_arithmetic("10 / 3"), 3);
        assert_eq!(shell.evaluate_arithmetic("-4 + 10"), 6);
    }

    #[test]
    fn quoted_and_unquoted_expansion() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut host = NoHost;
        run(&mut shell, &mut host, "name='a b'");
        let (_, out, _) = run(&mut shell, &mut host, "printf '<%s>\\n' $name");
        assert_eq!(out, "<a>\n<b>\n");
        let (_, out, _) = run(&mut shell, &mut host, "printf '<%s>\\n' \"$name\"");
        assert_eq!(out, "<a b>\n");
    }
}
