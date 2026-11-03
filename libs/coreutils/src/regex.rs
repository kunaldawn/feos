//! A tiny backtracking regular-expression matcher.
//!
//! This is not a complete POSIX engine. It supports the constructs that are
//! useful at an embedded shell prompt:
//!
//! - literals and escaped metacharacters
//! - `.` (any byte)
//! - `*`, `+`, `?` repetition of the preceding atom
//! - `[...]` classes with ranges and `!`/`^` negation
//! - `^` and `$` anchors
//!
//! Alternation and capture groups are deliberately absent; use `grep -F` for
//! literal searching when a pattern is not needed.

/// Returns `true` if `pattern` matches anywhere in `text`.
pub fn is_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();

    if pattern.first() == Some(&b'^') {
        return match_here(pattern, 0, text, 0);
    }

    for start in 0..=text.len() {
        if match_here(pattern, 0, text, start) {
            return true;
        }
    }
    false
}

fn match_here(pattern: &[u8], pi: usize, text: &[u8], ti: usize) -> bool {
    if pi == pattern.len() {
        return true;
    }
    if pattern[pi] == b'^' && pi == 0 {
        return match_here(pattern, 1, text, ti);
    }
    if pattern[pi] == b'$' && pi + 1 == pattern.len() {
        return ti == text.len();
    }

    let (atom_end, atom) = next_atom(pattern, pi);
    let Some(atom) = atom else {
        return false;
    };

    let (min, max) = match pattern.get(atom_end) {
        Some(b'*') => (0, usize::MAX),
        Some(b'+') => (1, usize::MAX),
        Some(b'?') => (0, 1),
        _ => (1, 1),
    };
    let after = if max == 1 && min == 1 {
        atom_end
    } else {
        atom_end + 1
    };

    // Greedy match, then backtrack.
    let mut count = 0usize;
    while count < max && ti + count < text.len() && atom.matches(text[ti + count]) {
        count += 1;
    }
    loop {
        if count >= min && match_here(pattern, after, text, ti + count) {
            return true;
        }
        if count == 0 {
            break;
        }
        count -= 1;
    }
    false
}

struct Atom {
    byte: Option<u8>,
    class: Option<Class>,
}

impl Atom {
    fn matches(&self, byte: u8) -> bool {
        if let Some(expected) = self.byte {
            return expected == byte;
        }
        if let Some(class) = &self.class {
            return class.matches(byte);
        }
        false
    }
}

struct Class {
    negate: bool,
    ranges: heapless_ranges::Ranges,
}

impl Class {
    fn matches(&self, byte: u8) -> bool {
        let inside = self.ranges.contains(byte);
        if self.negate {
            !inside
        } else {
            inside
        }
    }
}

/// A fixed-capacity range list, enough for any practical shell class.
mod heapless_ranges {
    pub struct Ranges {
        items: [(u8, u8); 32],
        len: usize,
    }

    impl Ranges {
        pub fn new() -> Self {
            Self {
                items: [(0, 0); 32],
                len: 0,
            }
        }

        pub fn push(&mut self, low: u8, high: u8) {
            if self.len < self.items.len() {
                self.items[self.len] = (low, high);
                self.len += 1;
            }
        }

        pub fn contains(&self, byte: u8) -> bool {
            self.items[..self.len]
                .iter()
                .any(|(low, high)| (*low..=*high).contains(&byte))
        }
    }
}

/// Returns the end of the atom starting at `pi` and the atom itself.
fn next_atom(pattern: &[u8], pi: usize) -> (usize, Option<Atom>) {
    match pattern.get(pi) {
        None => (pi, None),
        Some(b'.') => (
            pi + 1,
            Some(Atom {
                byte: None,
                class: Some(Class {
                    negate: true,
                    ranges: heapless_ranges::Ranges::new(),
                }),
            }),
        ),
        Some(b'\\') => match pattern.get(pi + 1) {
            Some(byte) => (
                pi + 2,
                Some(Atom {
                    byte: Some(*byte),
                    class: None,
                }),
            ),
            None => (
                pi + 1,
                Some(Atom {
                    byte: Some(b'\\'),
                    class: None,
                }),
            ),
        },
        Some(b'[') => {
            if let Some((end, class)) = parse_class(pattern, pi) {
                (
                    end,
                    Some(Atom {
                        byte: None,
                        class: Some(class),
                    }),
                )
            } else {
                (
                    pi + 1,
                    Some(Atom {
                        byte: Some(b'['),
                        class: None,
                    }),
                )
            }
        }
        Some(byte) => (
            pi + 1,
            Some(Atom {
                byte: Some(*byte),
                class: None,
            }),
        ),
    }
}

fn parse_class(pattern: &[u8], start: usize) -> Option<(usize, Class)> {
    let mut index = start + 1;
    let mut negate = false;
    if pattern.get(index) == Some(&b'^') || pattern.get(index) == Some(&b'!') {
        negate = true;
        index += 1;
    }

    let mut ranges = heapless_ranges::Ranges::new();
    let mut first = true;
    while index < pattern.len() {
        if pattern[index] == b']' && !first {
            return Some((index + 1, Class { negate, ranges }));
        }
        first = false;

        let low = pattern[index];
        if index + 2 < pattern.len() && pattern[index + 1] == b'-' && pattern[index + 2] != b']' {
            ranges.push(low, pattern[index + 2]);
            index += 3;
        } else {
            ranges.push(low, low);
            index += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_literals_and_dot() {
        assert!(is_match("abc", "xxabcxx"));
        assert!(!is_match("abc", "ab"));
        assert!(is_match("a.c", "abc"));
        assert!(is_match("a.c", "a-c"));
    }

    #[test]
    fn matches_repetitions() {
        assert!(is_match("ab*c", "ac"));
        assert!(is_match("ab*c", "abbbbc"));
        assert!(is_match("ab+c", "abc"));
        assert!(!is_match("ab+c", "ac"));
        assert!(is_match("ab?c", "ac"));
        assert!(is_match("ab?c", "abc"));
        assert!(!is_match("ab?c", "abbc"));
    }

    #[test]
    fn matches_classes_and_anchors() {
        assert!(is_match("[abc]at", "cat"));
        assert!(!is_match("[!abc]at", "cat"));
        assert!(is_match("[a-z]at", "cat"));
        assert!(is_match("^cat$", "cat"));
        assert!(!is_match("^cat$", "scat"));
        assert!(!is_match("^cat$", "cats"));
        assert!(is_match("^$", ""));
    }

    #[test]
    fn escapes_metacharacters() {
        assert!(is_match(r"a\.c", "a.c"));
        assert!(!is_match(r"a\.c", "abc"));
        assert!(is_match(r"\*", "x*y"));
    }
}
