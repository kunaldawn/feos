//! Shell-style glob matching (`*`, `?`, `[...]`).
//!
//! Only path components are matched; callers walk the filesystem themselves.
//! Matching is byte-oriented, which is fine for the ASCII-heavy names used by
//! the embedded image and keeps the implementation allocation-free.

/// Returns `true` when `name` matches the glob `pattern`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    match_from(pattern.as_bytes(), name.as_bytes(), 0, 0)
}

fn match_from(pattern: &[u8], name: &[u8], pi: usize, ni: usize) -> bool {
    if pi == pattern.len() {
        return ni == name.len();
    }

    match pattern[pi] {
        b'*' => {
            let mut next = pi;
            while next < pattern.len() && pattern[next] == b'*' {
                next += 1;
            }
            if next == pattern.len() {
                return true;
            }
            for index in ni..=name.len() {
                if match_from(pattern, name, next, index) {
                    return true;
                }
            }
            false
        }
        b'?' => ni < name.len() && match_from(pattern, name, pi + 1, ni + 1),
        b'[' => {
            let Some((end, matches)) = match_class(pattern, pi, name.get(ni).copied()) else {
                return ni < name.len()
                    && name[ni] == b'['
                    && match_from(pattern, name, pi + 1, ni + 1);
            };
            matches && match_from(pattern, name, end, ni + 1)
        }
        b'\\' if pi + 1 < pattern.len() => {
            ni < name.len()
                && name[ni] == pattern[pi + 1]
                && match_from(pattern, name, pi + 2, ni + 1)
        }
        byte => ni < name.len() && name[ni] == byte && match_from(pattern, name, pi + 1, ni + 1),
    }
}

/// Parses a bracket expression. Returns the index after `]` and whether the
/// candidate byte matched.
fn match_class(pattern: &[u8], start: usize, candidate: Option<u8>) -> Option<(usize, bool)> {
    let mut index = start + 1;
    if index >= pattern.len() {
        return None;
    }

    let mut negate = false;
    if pattern[index] == b'!' || pattern[index] == b'^' {
        negate = true;
        index += 1;
    }

    let mut matched = false;
    let mut first = true;
    while index < pattern.len() {
        if pattern[index] == b']' && !first {
            let result = if negate { !matched } else { matched };
            return Some((index + 1, result));
        }
        first = false;

        let Some(&low) = pattern.get(index) else {
            break;
        };
        let low = if low == b'\\' && index + 1 < pattern.len() {
            index += 1;
            pattern[index]
        } else {
            low
        };

        if index + 2 < pattern.len() && pattern[index + 1] == b'-' && pattern[index + 2] != b']' {
            let mut high = pattern[index + 2];
            let mut advance = 3;
            if high == b'\\' && index + 3 < pattern.len() {
                high = pattern[index + 3];
                advance = 4;
            }
            if let Some(byte) = candidate {
                if (low..=high).contains(&byte) {
                    matched = true;
                }
            }
            index += advance;
        } else {
            if let Some(byte) = candidate {
                if byte == low {
                    matched = true;
                }
            }
            index += 1;
        }
    }

    // Unterminated class: treat the opening `[` literally.
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
    }

    #[test]
    fn backslashes_escape_metacharacters() {
        assert!(glob_match(r"\*", "*"));
        assert!(!glob_match(r"\*", "x"));
        assert!(has_glob("a*"));
        assert!(!has_glob(r"a\*"));
    }
}
