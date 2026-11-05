//! `edit`: a small full-screen text editor.
//!
//! Ctrl-S saves, Ctrl-Q quits, and Ctrl-F searches. The editing logic takes keys
//! and produces bytes to send, so it is tested without a terminal.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::tty::{move_to, on_terminal, read_key, screen_size, Key, RawTerminal};
use super::Ctx;
use crate::fs::FsError;

/// Largest file the editor opens.
const MAX_FILE: usize = 4 * 1024 * 1024;

/// Columns a tab occupies.
const TAB: usize = 8;

/// Status-bar prompts the editor can be asking.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Mode {
    Edit,
    /// Typing a search; the text so far.
    Find(String),
    /// Typing a name for a buffer that has none.
    SaveAs(String),
}

/// What the editor wants done after a key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Action {
    Nothing,
    Save,
    Quit,
}

pub(super) struct Editor {
    lines: Vec<Vec<char>>,
    /// Cursor row, and column in characters.
    row: usize,
    column: usize,
    /// First row and display column on screen.
    top: usize,
    left: usize,
    width: usize,
    height: usize,
    path: String,
    dirty: bool,
    quit_warned: bool,
    message: String,
    mode: Mode,
}

impl Editor {
    pub(super) fn new(text: &str, path: &str, width: usize, height: usize) -> Self {
        let mut lines: Vec<Vec<char>> = text.split('\n').map(|line| line.chars().collect()).collect();
        if lines.is_empty() {
            lines.push(Vec::new());
        }
        Self {
            lines,
            row: 0,
            column: 0,
            top: 0,
            left: 0,
            width: width.max(1),
            height: height.max(2),
            path: path.to_string(),
            dirty: false,
            quit_warned: false,
            message: String::from("^S save  ^Q quit  ^F find"),
            mode: Mode::Edit,
        }
    }

    pub(super) fn text(&self) -> String {
        let mut text = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.extend(line.iter());
        }
        text
    }

    fn line_len(&self) -> usize {
        self.lines.get(self.row).map_or(0, Vec::len)
    }

    fn clamp_column(&mut self) {
        self.column = self.column.min(self.line_len());
    }

    fn body_rows(&self) -> usize {
        self.height.saturating_sub(1).max(1)
    }

    /// Display column of the cursor, with tabs expanded.
    fn display_column(&self) -> usize {
        let line = self.lines.get(self.row).map_or(&[][..], Vec::as_slice);
        let mut column = 0;
        for ch in line.iter().take(self.column) {
            column = advance(column, *ch);
        }
        column
    }

    fn scroll_into_view(&mut self) {
        let rows = self.body_rows();
        if self.row < self.top {
            self.top = self.row;
        }
        if self.row >= self.top + rows {
            self.top = self.row + 1 - rows;
        }
        let column = self.display_column();
        if column < self.left {
            self.left = column;
        }
        if column >= self.left + self.width {
            self.left = column + 1 - self.width;
        }
    }

    fn insert(&mut self, ch: char) {
        let column = self.column;
        if let Some(line) = self.lines.get_mut(self.row) {
            line.insert(column, ch);
            self.column += 1;
            self.dirty = true;
        }
    }

    fn newline(&mut self) {
        let column = self.column;
        if let Some(line) = self.lines.get_mut(self.row) {
            let rest = line.split_off(column);
            self.lines.insert(self.row + 1, rest);
            self.row += 1;
            self.column = 0;
            self.dirty = true;
        }
    }

    fn backspace(&mut self) {
        if self.column > 0 {
            self.column -= 1;
            let column = self.column;
            if let Some(line) = self.lines.get_mut(self.row) {
                line.remove(column);
            }
            self.dirty = true;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.column = self.line_len();
            if let Some(previous) = self.lines.get_mut(self.row) {
                previous.extend(line);
            }
            self.dirty = true;
        }
    }

    fn delete(&mut self) {
        if self.column < self.line_len() {
            let column = self.column;
            if let Some(line) = self.lines.get_mut(self.row) {
                line.remove(column);
            }
            self.dirty = true;
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            if let Some(line) = self.lines.get_mut(self.row) {
                line.extend(next);
            }
            self.dirty = true;
        }
    }

    /// Moves to the next occurrence of `needle` after the cursor, wrapping.
    fn find(&mut self, needle: &str) -> bool {
        let needle: Vec<char> = needle.chars().collect();
        if needle.is_empty() {
            return false;
        }
        let count = self.lines.len();
        for step in 0..=count {
            let row = (self.row + step) % count;
            let from = if step == 0 { self.column + 1 } else { 0 };
            let Some(line) = self.lines.get(row) else {
                continue;
            };
            let found = line
                .windows(needle.len())
                .enumerate()
                .skip(from)
                .find(|(_, window)| *window == needle.as_slice());
            if let Some((column, _)) = found {
                self.row = row;
                self.column = column;
                return true;
            }
        }
        false
    }

    /// Applies one key.
    pub(super) fn handle(&mut self, key: Key) -> Action {
        let action = match self.mode.clone() {
            Mode::Edit => self.handle_edit(key),
            Mode::Find(text) => self.handle_find(key, text),
            Mode::SaveAs(text) => self.handle_save_as(key, text),
        };
        self.clamp_column();
        self.scroll_into_view();
        action
    }

    fn handle_edit(&mut self, key: Key) -> Action {
        if key != Key::Ctrl(b'q') {
            self.quit_warned = false;
        }
        self.message.clear();
        match key {
            Key::Char(ch) => self.insert(ch),
            Key::Tab => self.insert('\t'),
            Key::Enter => self.newline(),
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete(),
            Key::Up => self.row = self.row.saturating_sub(1),
            Key::Down => self.row = (self.row + 1).min(self.lines.len() - 1),
            Key::Left => {
                if self.column > 0 {
                    self.column -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.column = self.line_len();
                }
            }
            Key::Right => {
                if self.column < self.line_len() {
                    self.column += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.column = 0;
                }
            }
            Key::Home | Key::Ctrl(b'a') => self.column = 0,
            Key::End | Key::Ctrl(b'e') => self.column = self.line_len(),
            Key::PageUp => self.row = self.row.saturating_sub(self.body_rows()),
            Key::PageDown => self.row = (self.row + self.body_rows()).min(self.lines.len() - 1),
            Key::Ctrl(b'k') => {
                if self.column < self.line_len() {
                    let column = self.column;
                    if let Some(line) = self.lines.get_mut(self.row) {
                        line.truncate(column);
                    }
                    self.dirty = true;
                } else {
                    self.delete();
                }
            }
            Key::Ctrl(b'f') => self.mode = Mode::Find(String::new()),
            Key::Ctrl(b's') => {
                if self.path.is_empty() {
                    self.mode = Mode::SaveAs(String::new());
                } else {
                    return Action::Save;
                }
            }
            Key::Ctrl(b'q') => {
                if self.dirty && !self.quit_warned {
                    self.quit_warned = true;
                    self.message = String::from("unsaved changes: ^Q again to discard them");
                } else {
                    return Action::Quit;
                }
            }
            Key::Eof => return Action::Quit,
            _ => {}
        }
        Action::Nothing
    }

    fn handle_find(&mut self, key: Key, mut text: String) -> Action {
        match key {
            Key::Char(ch) => {
                text.push(ch);
                self.mode = Mode::Find(text);
            }
            Key::Backspace => {
                text.pop();
                self.mode = Mode::Find(text);
            }
            Key::Enter => {
                self.mode = Mode::Edit;
                self.message = if self.find(&text) {
                    format!("found: {text}")
                } else {
                    format!("not found: {text}")
                };
            }
            Key::Escape | Key::Ctrl(b'c') | Key::Eof => self.mode = Mode::Edit,
            _ => {}
        }
        Action::Nothing
    }

    fn handle_save_as(&mut self, key: Key, mut text: String) -> Action {
        match key {
            Key::Char(ch) => {
                text.push(ch);
                self.mode = Mode::SaveAs(text);
            }
            Key::Backspace => {
                text.pop();
                self.mode = Mode::SaveAs(text);
            }
            Key::Enter => {
                self.mode = Mode::Edit;
                if !text.is_empty() {
                    self.path = text;
                    return Action::Save;
                }
            }
            Key::Escape | Key::Ctrl(b'c') | Key::Eof => self.mode = Mode::Edit,
            _ => {}
        }
        Action::Nothing
    }

    pub(super) fn saved(&mut self, result: Result<usize, FsError>) {
        match result {
            Ok(bytes) => {
                self.dirty = false;
                self.message = format!("wrote {bytes} bytes");
            }
            Err(error) => self.message = format!("cannot save: {}", error.text()),
        }
    }

    /// Everything to send to the terminal to show the current state.
    pub(super) fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x1b[?25l");
        for screen_row in 0..self.body_rows() {
            move_to(&mut out, screen_row, 0);
            out.extend_from_slice(b"\x1b[K");
            if let Some(line) = self.lines.get(self.top + screen_row) {
                let mut column = 0;
                let mut text = String::new();
                for ch in line {
                    let next = advance(column, *ch);
                    if column >= self.left + self.width {
                        break;
                    }
                    for shown in column..next {
                        if shown >= self.left && shown < self.left + self.width {
                            text.push(if *ch == '\t' || ch.is_control() { ' ' } else { *ch });
                        }
                    }
                    column = next;
                }
                out.extend_from_slice(text.as_bytes());
            } else {
                out.push(b'~');
            }
        }

        move_to(&mut out, self.height - 1, 0);
        out.extend_from_slice(b"\x1b[7m\x1b[K");
        let status = match &self.mode {
            Mode::Find(text) => format!("find: {text}"),
            Mode::SaveAs(text) => format!("save as: {text}"),
            Mode::Edit => {
                let name = if self.path.is_empty() { "[new file]" } else { &self.path };
                let mark = if self.dirty { " *" } else { "" };
                format!("{name}{mark}  {}:{}  {}", self.row + 1, self.column + 1, self.message)
            }
        };
        let status: String = status.chars().take(self.width).collect();
        out.extend_from_slice(status.as_bytes());
        out.extend_from_slice(b"\x1b[0m");

        match &self.mode {
            Mode::Edit => {
                move_to(
                    &mut out,
                    self.row - self.top,
                    self.display_column().saturating_sub(self.left),
                );
            }
            _ => move_to(&mut out, self.height - 1, status.chars().count().min(self.width - 1)),
        }
        out.extend_from_slice(b"\x1b[?25h");
        out
    }
}

fn advance(column: usize, ch: char) -> usize {
    if ch == '\t' {
        (column / TAB + 1) * TAB
    } else {
        column + 1
    }
}

/// `edit [file]`
pub(super) fn edit(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.len() > 1 {
        ctx.stderr.write(b"usage: edit [file]\n");
        return 2;
    }
    if !on_terminal(ctx.stdin, ctx.stdout) {
        ctx.stderr.write(b"edit: needs a terminal\n");
        return 1;
    }
    let path = args.first().map_or("", String::as_str);
    let text = if path.is_empty() {
        String::new()
    } else {
        match ctx.fs.meta(path) {
            Ok(meta) if meta.size > MAX_FILE => {
                ctx.stderr.write(format!("edit: {path}: too large\n").as_bytes());
                return 1;
            }
            _ => {}
        }
        match ctx.fs.read_file(path) {
            Ok(data) => match String::from_utf8(data) {
                Ok(text) => text,
                Err(_) => {
                    ctx.stderr.write(format!("edit: {path}: not text\n").as_bytes());
                    return 1;
                }
            },
            Err(FsError::NotFound) => String::new(),
            Err(error) => {
                ctx.stderr.write(format!("edit: {path}: {}\n", error.text()).as_bytes());
                return 1;
            }
        }
    };

    let (width, height) = screen_size(ctx.env);
    let mut editor = Editor::new(&text, path, width, height);
    let terminal = RawTerminal::enter(ctx);
    let mut pending = vec![];
    loop {
        ctx.stdout.write(&editor.render());
        match editor.handle(read_key(ctx.stdin, &mut pending)) {
            Action::Nothing => {}
            Action::Quit => break,
            Action::Save => {
                let text = editor.text();
                let result = ctx
                    .fs
                    .write_file(&editor.path, text.as_bytes(), false)
                    .map(|()| text.len());
                editor.saved(result);
            }
        }
        if ctx.stdout.is_broken() {
            break;
        }
    }
    terminal.leave(ctx);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(editor: &mut Editor, text: &str) {
        for ch in text.chars() {
            let key = if ch == '\n' { Key::Enter } else { Key::Char(ch) };
            assert_eq!(editor.handle(key), Action::Nothing);
        }
    }

    fn fresh(text: &str) -> Editor {
        Editor::new(text, "/f", 20, 6)
    }

    #[test]
    fn typing_builds_lines() {
        let mut editor = fresh("");
        typed(&mut editor, "ab\ncd");
        assert_eq!(editor.text(), "ab\ncd");
        assert!(editor.dirty);
    }

    #[test]
    fn a_trailing_newline_survives_a_round_trip() {
        assert_eq!(fresh("a\nb\n").text(), "a\nb\n");
    }

    #[test]
    fn backspace_and_delete_join_lines() {
        let mut editor = fresh("ab\ncd");
        editor.handle(Key::Down);
        editor.handle(Key::Backspace);
        assert_eq!(editor.text(), "abcd");
        assert_eq!((editor.row, editor.column), (0, 2));
        editor.handle(Key::Delete);
        assert_eq!(editor.text(), "abd");
        // At the very start and end, nothing happens.
        editor.handle(Key::Home);
        editor.handle(Key::Backspace);
        editor.handle(Key::End);
        editor.handle(Key::Delete);
        assert_eq!(editor.text(), "abd");
    }

    #[test]
    fn moving_keeps_the_cursor_in_the_text() {
        let mut editor = fresh("long line\nx");
        editor.handle(Key::End);
        editor.handle(Key::Down);
        assert_eq!(editor.column, 1);
        editor.handle(Key::Right);
        editor.handle(Key::Down);
        editor.handle(Key::PageDown);
        assert_eq!(editor.row, 1);
        editor.handle(Key::PageUp);
        editor.handle(Key::Left);
        assert_eq!((editor.row, editor.column), (0, 0));
    }

    #[test]
    fn quitting_with_changes_asks_first() {
        let mut editor = fresh("");
        assert_eq!(editor.handle(Key::Ctrl(b'q')), Action::Quit);
        let mut editor = fresh("");
        typed(&mut editor, "x");
        assert_eq!(editor.handle(Key::Ctrl(b'q')), Action::Nothing);
        assert_eq!(editor.handle(Key::Ctrl(b'q')), Action::Quit);
    }

    #[test]
    fn a_buffer_with_no_name_asks_for_one() {
        let mut editor = Editor::new("", "", 20, 6);
        assert_eq!(editor.handle(Key::Ctrl(b's')), Action::Nothing);
        typed(&mut editor, "/n");
        assert_eq!(editor.handle(Key::Enter), Action::Save);
        assert_eq!(editor.path, "/n");
        editor.saved(Ok(0));
        assert!(!editor.dirty);
    }

    #[test]
    fn search_finds_and_wraps() {
        let mut editor = fresh("one\ntwo\none");
        editor.handle(Key::Ctrl(b'f'));
        typed(&mut editor, "one");
        editor.handle(Key::Enter);
        assert_eq!(editor.row, 2);
        editor.handle(Key::Ctrl(b'f'));
        typed(&mut editor, "one");
        editor.handle(Key::Enter);
        assert_eq!(editor.row, 0);
        editor.handle(Key::Ctrl(b'f'));
        typed(&mut editor, "nope");
        editor.handle(Key::Enter);
        assert!(editor.message.contains("not found"));
    }

    #[test]
    fn the_view_follows_the_cursor() {
        let mut editor = fresh(&"x\n".repeat(50));
        for _ in 0..20 {
            editor.handle(Key::Down);
        }
        assert!(editor.row >= editor.top && editor.row < editor.top + editor.body_rows());
        let mut editor = fresh(&"y".repeat(100));
        editor.handle(Key::End);
        assert!(editor.left > 0);
        assert!(!editor.render().is_empty());
    }

    #[test]
    fn tabs_and_wide_lines_render_without_trouble() {
        let mut editor = Editor::new("\ta\tb\n", "/f", 4, 2);
        editor.handle(Key::End);
        assert_eq!(editor.display_column(), 17);
        let shown = editor.render();
        assert!(!shown.is_empty());
    }
}
