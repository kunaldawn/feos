//! `less`: page through text, on the terminal's keys even when reading a pipe.
//!
//! Not on a terminal, it just copies, as `less` does.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::tty::{move_to, on_terminal, read_key, screen_size, Key, RawTerminal};
use super::Ctx;
use crate::io::MAX_STREAM_BYTES;

pub(super) struct Pager {
    lines: Vec<String>,
    top: usize,
    left: usize,
    width: usize,
    height: usize,
    name: String,
    /// `/` typed, and the text so far.
    searching: Option<String>,
    last_search: String,
    message: String,
}

impl Pager {
    pub(super) fn new(text: &str, name: &str, width: usize, height: usize) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(clean).collect();
        if text.ends_with('\n') {
            lines.pop();
        }
        Self {
            lines,
            top: 0,
            left: 0,
            width: width.max(1),
            height: height.max(2),
            name: name.to_string(),
            searching: None,
            last_search: String::new(),
            message: String::new(),
        }
    }

    fn page(&self) -> usize {
        self.height.saturating_sub(1).max(1)
    }

    fn last_top(&self) -> usize {
        self.lines.len().saturating_sub(self.page())
    }

    fn search(&mut self, needle: &str, from: usize) {
        match self
            .lines
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, line)| line.contains(needle))
        {
            Some((row, _)) => {
                self.top = row;
                self.message.clear();
            }
            None => self.message = format!("not found: {needle}"),
        }
    }

    /// Applies a key. Returns `true` when the pager should end.
    pub(super) fn handle(&mut self, key: Key) -> bool {
        if let Some(mut text) = self.searching.take() {
            match key {
                Key::Char(ch) => {
                    text.push(ch);
                    self.searching = Some(text);
                }
                Key::Backspace => {
                    text.pop();
                    self.searching = Some(text);
                }
                Key::Enter => {
                    if !text.is_empty() {
                        self.search(&text, self.top);
                        self.last_search = text;
                    }
                }
                _ => {}
            }
            return false;
        }
        self.message.clear();
        match key {
            Key::Char('q' | 'Q') | Key::Ctrl(b'c') | Key::Eof => return true,
            Key::Down | Key::Enter | Key::Char('j') => self.top += 1,
            Key::Up | Key::Char('k') => self.top = self.top.saturating_sub(1),
            Key::PageDown | Key::Char(' ' | 'f') => self.top += self.page(),
            Key::PageUp | Key::Char('b') => self.top = self.top.saturating_sub(self.page()),
            Key::Home | Key::Char('g') => self.top = 0,
            Key::End | Key::Char('G') => self.top = self.last_top(),
            Key::Right => self.left += self.width / 2,
            Key::Left => self.left = self.left.saturating_sub(self.width / 2),
            Key::Char('/') => self.searching = Some(String::new()),
            Key::Char('n') if !self.last_search.is_empty() => {
                let needle = self.last_search.clone();
                self.search(&needle, self.top + 1);
            }
            _ => {}
        }
        self.top = self.top.min(self.last_top());
        false
    }

    pub(super) fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x1b[?25l");
        for row in 0..self.page() {
            move_to(&mut out, row, 0);
            out.extend_from_slice(b"\x1b[K");
            match self.lines.get(self.top + row) {
                Some(line) => {
                    let shown: String = line.chars().skip(self.left).take(self.width).collect();
                    out.extend_from_slice(shown.as_bytes());
                }
                None => out.push(b'~'),
            }
        }
        move_to(&mut out, self.height - 1, 0);
        out.extend_from_slice(b"\x1b[7m\x1b[K");
        let status = match &self.searching {
            Some(text) => format!("/{text}"),
            None if !self.message.is_empty() => self.message.clone(),
            None => {
                let shown = (self.top + self.page()).min(self.lines.len());
                format!("{}  {shown}/{}  (q quits, / searches)", self.name, self.lines.len())
            }
        };
        let status: String = status.chars().take(self.width).collect();
        out.extend_from_slice(status.as_bytes());
        out.extend_from_slice(b"\x1b[0m");
        out
    }
}

/// A line's text with controls that would upset the screen made visible.
fn clean(line: &str) -> String {
    let mut text = String::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            '\t' => {
                let spaces = 8 - text.chars().count() % 8;
                text.extend(core::iter::repeat_n(' ', spaces));
            }
            ch if ch.is_control() => text.push('?'),
            ch => text.push(ch),
        }
    }
    text
}

/// `less [file]`
pub(super) fn less(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let (data, name) = match args.first().filter(|path| *path != "-") {
        Some(path) => match ctx.fs.read_file(path) {
            Ok(data) => (data, path.as_str()),
            Err(error) => {
                ctx.stderr.write(format!("less: {path}: {}\n", error.text()).as_bytes());
                return 1;
            }
        },
        None => match ctx.stdin.try_read_to_end(MAX_STREAM_BYTES) {
            Ok(data) => (data, "(stdin)"),
            Err(error) => {
                ctx.stderr.write(format!("less: {}\n", error.text()).as_bytes());
                return 1;
            }
        },
    };
    if !on_terminal(ctx.stdin, ctx.stdout) {
        ctx.stdout.write(&data);
        return 0;
    }

    let text = String::from_utf8_lossy(&data);
    let (width, height) = screen_size(ctx.env);
    let mut pager = Pager::new(&text, name, width, height);
    let terminal = RawTerminal::enter(ctx);
    let mut pending = Vec::new();
    loop {
        ctx.stdout.write(&pager.render());
        if pager.handle(read_key(ctx.stdin, &mut pending)) || ctx.stdout.is_broken() {
            break;
        }
    }
    terminal.leave(ctx);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbers(count: usize) -> String {
        (0..count).map(|n| format!("line {n}\n")).collect()
    }

    #[test]
    fn paging_stops_at_both_ends() {
        let mut pager = Pager::new(&numbers(10), "x", 40, 5);
        pager.handle(Key::PageUp);
        assert_eq!(pager.top, 0);
        pager.handle(Key::PageDown);
        assert_eq!(pager.top, 4);
        for _ in 0..5 {
            pager.handle(Key::Down);
        }
        assert_eq!(pager.top, 6);
        pager.handle(Key::Home);
        assert_eq!(pager.top, 0);
        pager.handle(Key::End);
        assert_eq!(pager.top, 6);
    }

    #[test]
    fn text_shorter_than_the_screen_cannot_scroll() {
        let mut pager = Pager::new("a\nb\n", "x", 40, 24);
        pager.handle(Key::PageDown);
        pager.handle(Key::End);
        assert_eq!(pager.top, 0);
        assert_eq!(pager.lines.len(), 2);
    }

    #[test]
    fn searching_jumps_and_repeats() {
        let mut pager = Pager::new("a\nb\nc\nb\nd\ne\nf\ng\n", "x", 40, 3);
        pager.handle(Key::Char('/'));
        pager.handle(Key::Char('b'));
        assert!(!pager.handle(Key::Enter));
        assert_eq!(pager.top, 1);
        pager.handle(Key::Char('n'));
        assert_eq!(pager.top, 3);
        pager.handle(Key::Char('/'));
        pager.handle(Key::Char('z'));
        pager.handle(Key::Enter);
        assert!(pager.message.contains("not found"));
        assert!(pager.handle(Key::Char('q')));
    }

    #[test]
    fn controls_cannot_reach_the_terminal() {
        let pager = Pager::new("a\x1b[2Jb\tc\n", "x", 40, 3);
        assert_eq!(pager.lines[0], "a?[2Jb  c");
    }
}
