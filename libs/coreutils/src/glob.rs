//! Shell-style glob matching (`*`, `?`, `[...]`).
//!
//! Only path components are matched; callers walk the filesystem themselves.
//! Matching works on `char`s, so `?` never splits a UTF-8 sequence, and it uses
//! the classic one-star backtrack instead of recursion, so a hostile pattern
//! cannot overflow a small kernel stack. No allocation happens here.

/// Returns `true` when `name` matches the glob `pattern`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut pi = 0usize;
    let mut ni = 0usize;
    // Where to resume after the most recent `*`: pattern and name offsets.
    let mut star: Option<(usize, usize)> = None;

    loop {
        if pi < bytes.len() && bytes[pi] == b'*' {
            while pi < bytes.len() && bytes[pi] == b'*' {
                pi += 1;
            }
            star = Some((pi, ni));
            continue;
        }

        if pi < bytes.len() {
            if let Some(ch) = name.get(ni..).and_then(|rest| rest.chars().next()) {
                if let Some(next) = match_one(pattern, pi, ch) {
                    pi = next;
                    ni += ch.len_utf8();
                    continue;
                }
            }
        } else if ni == name.len() {
            return true;
        }

        // Mismatch: let the last `*` swallow one more character.
        let Some((star_pi, star_ni)) = star else {
            return false;
        };
        let Some(ch) = name.get(star_ni..).and_then(|rest| rest.chars().next()) else {
            return false;
        };
        pi = star_pi;
        ni = star_ni + ch.len_utf8();
        star = Some((star_pi, ni));
    }
}

/// Matches the single pattern element at `pi` against `ch`, returning the
/// offset of the next element.
fn match_one(pattern: &str, pi: usize, ch: char) -> Option<usize> {
    let rest = pattern.get(pi..)?;
    let mut chars = rest.chars();
    let first = chars.next()?;
    match first {
        '?' => Some(pi + 1),
        '[' => match match_class(rest, ch) {
            Some((length, true)) => Some(pi + length),
            Some((_, false)) => None,
            None => (ch == '[').then_some(pi + 1),
        },
        '\\' => match chars.next() {
            Some(escaped) => (escaped == ch).then_some(pi + 1 + escaped.len_utf8()),
            None => (ch == '\\').then_some(pi + 1),
        },
        literal => (literal == ch).then_some(pi + literal.len_utf8()),
    }
}

/// Tests `ch` against a POSIX class name such as `alpha`.
pub fn posix_class(name: &str, ch: char) -> Option<bool> {
    Some(match name {
        "alpha" => ch.is_ascii_alphabetic(),
        "digit" => ch.is_ascii_digit(),
        "alnum" => ch.is_ascii_alphanumeric(),
        "upper" => ch.is_ascii_uppercase(),
        "lower" => ch.is_ascii_lowercase(),
        "space" => matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'),
        "blank" => matches!(ch, ' ' | '\t'),
        "punct" => ch.is_ascii_punctuation(),
        "print" => ch.is_ascii_graphic() || ch == ' ',
        "graph" => ch.is_ascii_graphic(),
        "cntrl" => ch.is_ascii_control(),
        "xdigit" => ch.is_ascii_hexdigit(),
        _ => return None,
    })
}

/// Parses the bracket expression at the start of `text`. Returns its length in
/// bytes and whether `ch` matched, or `None` when it is unterminated (a `[`
/// then stands for itself).
fn match_class(text: &str, ch: char) -> Option<(usize, bool)> {
    let mut chars = text.char_indices().peekable();
    chars.next()?;

    let mut negate = false;
    if let Some((_, '!' | '^')) = chars.peek() {
        negate = true;
        chars.next();
    }

    let mut matched = false;
    let mut first = true;
    while let Some((index, item)) = chars.next() {
        if item == ']' && !first {
            return Some((index + 1, matched != negate));
        }
        first = false;

        if item == '[' && text.get(index + 1..).is_some_and(|rest| rest.starts_with(':')) {
            if let Some(end) = text[index + 2..].find(":]") {
                if let Some(result) = posix_class(&text[index + 2..index + 2 + end], ch) {
                    matched |= result;
                    let stop = index + 2 + end + 1;
                    while chars.peek().is_some_and(|(at, _)| *at <= stop) {
                        chars.next();
                    }
                    continue;
                }
            }
        }

        let low = if item == '\\' {
            chars.next()?.1
        } else {
            item
        };

        let range = {
            let mut ahead = chars.clone();
            match (ahead.next(), ahead.next()) {
                (Some((_, '-')), Some((_, high))) if high != ']' => Some(high),
                _ => None,
            }
        };
        match range {
            Some(high) => {
                chars.next();
                chars.next();
                let high = if high == '\\' { chars.next()?.1 } else { high };
                matched |= (low..=high).contains(&ch);
            }
            None => matched |= low == ch,
        }
    }

    None
}

/// Returns `true` when `text` contains an unescaped glob metacharacter.
pub fn has_glob(text: &str) -> bool {
    let mut escaped = false;
    for byte in text.bytes() {
        match byte {
            b'\\' if !escaped => escaped = true,
            b'*' | b'?' | b'[' if !escaped => return true,
            _ => escaped = false,
        }
    }
    false
}

/// Removes the backslash escapes [`glob_match`] understands.
pub fn unescape_glob(text: &str) -> alloc::string::String {
    let mut result = alloc::string::String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                result.push(next);
                continue;
            }
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_stars_and_questions() {
        assert!(glob_match("*.txt", "readme.txt"));
        assert!(glob_match("*.txt", ".txt"));
        assert!(!glob_match("*.txt", "readme.md"));
        assert!(glob_match("file?.rs", "file1.rs"));
        assert!(!glob_match("file?.rs", "file10.rs"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn matches_character_classes() {
        assert!(glob_match("[abc]at", "cat"));
        assert!(!glob_match("[!abc]at", "cat"));
        assert!(glob_match("[a-z]at", "cat"));
        assert!(glob_match("[0-9][0-9]", "42"));
        assert!(glob_match("[[]x", "[x"));
        assert!(!glob_match("[abc", "a"));
        assert!(glob_match("[abc", "[abc"));
        assert!(glob_match("[]x]", "]"));
    }

    #[test]
    fn matches_posix_classes() {
        assert!(glob_match("[[:digit:]]x", "7x"));
        assert!(!glob_match("[[:digit:]]x", "ax"));
        assert!(glob_match("[![:space:]]", "a"));
        assert!(glob_match("[[:alpha:]_]*", "_foo"));
    }

    #[test]
    fn a_negated_class_needs_a_character() {
        assert!(!glob_match("a[!b]*", "a"));
        assert!(glob_match("a[!b]*", "ac"));
    }

    #[test]
    fn question_marks_consume_whole_characters() {
        assert!(glob_match("?", "é"));
        assert!(glob_match("?x", "éx"));
        assert!(!glob_match("??", "é"));
    }

    #[test]
    fn backslashes_escape_metacharacters() {
        assert!(glob_match(r"\*", "*"));
        assert!(!glob_match(r"\*", "x"));
        assert!(has_glob("a*"));
        assert!(!has_glob(r"a\*"));
        assert_eq!(unescape_glob(r"a\*b"), "a*b");
    }

    #[test]
    fn pathological_patterns_finish_quickly() {
        let pattern = "a*".repeat(64) + "b";
        let name = "a".repeat(4096);
        assert!(!glob_match(&pattern, &name));
    }
}
