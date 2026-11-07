//! The line editor both terminals share.
//!
//! [`LineEditor`] is a pure state machine: it takes key presses and reports what
//! happened, and holds no opinion about where keys come from or how text gets to
//! a screen. The serial console feeds it decoded bytes, and the desktop window
//! feeds it GUI events. [`render_line`] produces the control sequences that redraw
//! the line, including when it wraps.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use feos_keys::{KeyCode, KeyPress};

use crate::{lexer, Host, Shell};

/// What a key press did.
pub enum EditEvent {
    /// Nothing to show.
    None,
    /// The line or cursor changed.
    Changed,
    /// A complete command is ready. Embedded newlines come from continuations.
    Submit(String),
    /// The line needs more input, so a continuation prompt comes next.
    Continuation,
    /// Ctrl-C.
    Interrupted,
    /// Ctrl-D on an empty line.
    Eof,
    /// Ctrl-L.
    ClearScreen,
    /// Tab found several candidates to show.
    Candidates(Vec<String>),
}

struct Search {
    query: String,
    /// The history entry on show.
    found: Option<usize>,
    /// What to restore on cancel.
    original: Vec<char>,
    original_cursor: usize,
}

/// The state of one line being edited.
pub struct LineEditor {
    buffer: Vec<char>,
    cursor: usize,
    pending: String,
    history_index: usize,
    saved_line: String,
    search: Option<Search>,
}

impl LineEditor {
    /// Creates an empty editor positioned after the last history entry.
    pub fn new(history_len: usize) -> Self {
        Self {
            buffer: Vec::new(),
            cursor: 0,
            pending: String::new(),
            history_index: history_len,
            saved_line: String::new(),
            search: None,
        }
    }

    /// Starts over for a new command.
    pub fn reset(&mut self, history_len: usize) {
        *self = Self::new(history_len);
    }

    /// The text being edited.
    pub fn text(&self) -> &[char] {
        &self.buffer
    }

    /// The cursor, as an index into [`LineEditor::text`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// True while a multi-line command is being entered.
    pub fn is_continuation(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The prompt to show now.
    pub fn prompt(&self, shell: &Shell<'_>) -> String {
        if let Some(search) = &self.search {
            return format!("(reverse-i-search)`{}': ", search.query);
        }
        if self.is_continuation() {
            shell.continuation_prompt()
        } else {
            shell.prompt()
        }
    }

    fn line(&self) -> String {
        self.buffer.iter().collect()
    }

    fn set_line(&mut self, text: &str) {
        self.buffer = text.chars().collect();
        self.cursor = self.buffer.len();
    }

    /// Handles one key press.
    pub fn handle(&mut self, key: KeyPress, shell: &mut Shell<'_>, host: &dyn Host) -> EditEvent {
        if self.search.is_some() {
            if let Some(event) = self.handle_search(key, shell) {
                return event;
            }
        }

        if key.ctrl {
            return self.handle_ctrl(key, shell, host);
        }
        if key.alt {
            return match key.code {
                KeyCode::Char('b') | KeyCode::Left => {
                    self.cursor = self.word_start();
                    EditEvent::Changed
                }
                KeyCode::Char('f') | KeyCode::Right => {
                    self.cursor = self.word_end();
                    EditEvent::Changed
                }
                KeyCode::Backspace => {
                    self.delete_word();
                    EditEvent::Changed
                }
                _ => EditEvent::None,
            };
        }

        match key.code {
            KeyCode::Char(ch) => {
                self.buffer.insert(self.cursor, ch);
                self.cursor += 1;
                EditEvent::Changed
            }
            KeyCode::Enter => self.enter(),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.buffer.remove(self.cursor);
                    EditEvent::Changed
                } else {
                    EditEvent::None
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    self.buffer.remove(self.cursor);
                    EditEvent::Changed
                } else {
                    EditEvent::None
                }
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                EditEvent::Changed
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.buffer.len());
                EditEvent::Changed
            }
            KeyCode::Home => {
                self.cursor = 0;
                EditEvent::Changed
            }
            KeyCode::End => {
                self.cursor = self.buffer.len();
                EditEvent::Changed
            }
            KeyCode::Up => self.history_up(shell),
            KeyCode::Down => self.history_down(shell),
            KeyCode::Tab => self.complete(shell, host),
            _ => EditEvent::None,
        }
    }

    fn handle_ctrl(&mut self, key: KeyPress, shell: &mut Shell<'_>, host: &dyn Host) -> EditEvent {
        let KeyCode::Char(letter) = key.code else {
            return match key.code {
                KeyCode::Left => {
                    self.cursor = self.word_start();
                    EditEvent::Changed
                }
                KeyCode::Right => {
                    self.cursor = self.word_end();
                    EditEvent::Changed
                }
                _ => EditEvent::None,
            };
        };
        match letter.to_ascii_lowercase() {
            'a' => {
                self.cursor = 0;
                EditEvent::Changed
            }
            'e' => {
                self.cursor = self.buffer.len();
                EditEvent::Changed
            }
            'b' => {
                self.cursor = self.cursor.saturating_sub(1);
                EditEvent::Changed
            }
            'f' => {
                self.cursor = (self.cursor + 1).min(self.buffer.len());
                EditEvent::Changed
            }
            'h' => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.buffer.remove(self.cursor);
                }
                EditEvent::Changed
            }
            'p' => self.history_up(shell),
            'n' => self.history_down(shell),
            'u' => {
                self.buffer.drain(..self.cursor);
                self.cursor = 0;
                EditEvent::Changed
            }
            'k' => {
                self.buffer.truncate(self.cursor);
                EditEvent::Changed
            }
            'w' => {
                self.delete_word();
                EditEvent::Changed
            }
            'l' => EditEvent::ClearScreen,
            'c' => EditEvent::Interrupted,
            'd' => {
                if self.buffer.is_empty() && self.pending.is_empty() {
                    EditEvent::Eof
                } else if self.cursor < self.buffer.len() {
                    self.buffer.remove(self.cursor);
                    EditEvent::Changed
                } else {
                    EditEvent::None
                }
            }
            'j' | 'm' => self.enter(),
            'i' => self.complete(shell, host),
            'r' => {
                self.search = Some(Search {
                    query: String::new(),
                    found: None,
                    original: self.buffer.clone(),
                    original_cursor: self.cursor,
                });
                EditEvent::Changed
            }
            _ => EditEvent::None,
        }
    }

    fn word_start(&self) -> usize {
        let mut start = self.cursor;
        while start > 0 && self.buffer[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.buffer[start - 1].is_whitespace() {
            start -= 1;
        }
        start
    }

    fn word_end(&self) -> usize {
        let mut end = self.cursor;
        while end < self.buffer.len() && self.buffer[end].is_whitespace() {
            end += 1;
        }
        while end < self.buffer.len() && !self.buffer[end].is_whitespace() {
            end += 1;
        }
        end
    }

    fn delete_word(&mut self) {
        let start = self.word_start();
        self.buffer.drain(start..self.cursor);
        self.cursor = start;
    }

    fn enter(&mut self) -> EditEvent {
        let line = self.line();
        let mut complete = core::mem::take(&mut self.pending);
        complete.push_str(&line);
        if lexer::needs_more(&complete) {
            complete.push('\n');
            self.pending = complete;
            self.buffer.clear();
            self.cursor = 0;
            return EditEvent::Continuation;
        }
        EditEvent::Submit(complete)
    }

    fn history_up(&mut self, shell: &Shell<'_>) -> EditEvent {
        if self.history_index == shell.history.len() {
            self.saved_line = self.line();
        }
        if self.history_index > 0 {
            self.history_index -= 1;
            if let Some(entry) = shell.history.get(self.history_index) {
                self.set_line(entry);
            }
            return EditEvent::Changed;
        }
        EditEvent::None
    }

    fn history_down(&mut self, shell: &Shell<'_>) -> EditEvent {
        if self.history_index >= shell.history.len() {
            return EditEvent::None;
        }
        self.history_index += 1;
        let text = match shell.history.get(self.history_index) {
            Some(entry) => entry.clone(),
            None => self.saved_line.clone(),
        };
        self.set_line(&text);
        EditEvent::Changed
    }

    fn complete(&mut self, shell: &mut Shell<'_>, host: &dyn Host) -> EditEvent {
        let line = self.line();
        let byte_cursor = line
            .char_indices()
            .nth(self.cursor)
            .map_or(line.len(), |(index, _)| index);
        let completion = shell.complete(&line, byte_cursor, host);
        let cursor = completion
            .line
            .get(..completion.cursor)
            .map_or(0, |head| head.chars().count());
        self.buffer = completion.line.chars().collect();
        self.cursor = cursor;
        if completion.candidates.is_empty() {
            EditEvent::Changed
        } else {
            EditEvent::Candidates(completion.candidates)
        }
    }

    /// Finds the newest history entry before `from` that holds the query.
    fn find(shell: &Shell<'_>, query: &str, from: usize) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        (0..from.min(shell.history.len()))
            .rev()
            .find(|index| shell.history[*index].contains(query))
    }

    /// Handles a key during Ctrl-R. `None` means the search ended and the key
    /// should go on to the normal handling.
    fn handle_search(&mut self, key: KeyPress, shell: &Shell<'_>) -> Option<EditEvent> {
        let code = key.code;

        if key.ctrl {
            match code {
                // Ctrl-R again steps to the next older match.
                KeyCode::Char('r') => {
                    let search = self.search.as_mut()?;
                    let from = search.found.unwrap_or(shell.history.len());
                    let older = Self::find(shell, &search.query, from);
                    if older.is_some() {
                        search.found = older;
                    }
                    let entry = search.found.and_then(|at| shell.history.get(at)).cloned();
                    if let Some(entry) = entry {
                        self.set_line(&entry);
                    }
                    return Some(EditEvent::Changed);
                }
                KeyCode::Char('c' | 'g') => {
                    return Some(self.cancel_search());
                }
                _ => {}
            }
        }

        let search = self.search.as_mut()?;
        match code {
            KeyCode::Char(ch) if !key.ctrl && !key.alt => {
                search.query.push(ch);
                let start = search
                    .found
                    .map_or(shell.history.len(), |at| at.saturating_add(1));
                search.found = Self::find(shell, &search.query, start);
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.found = Self::find(shell, &search.query, shell.history.len());
            }
            KeyCode::Escape => return Some(self.cancel_search()),
            _ => {
                // Any other key accepts the match and then acts normally.
                self.search = None;
                return None;
            }
        }

        let entry = search.found.and_then(|at| shell.history.get(at)).cloned();
        match entry {
            Some(entry) => self.set_line(&entry),
            None => {
                self.buffer.clear();
                self.cursor = 0;
            }
        }
        Some(EditEvent::Changed)
    }

    fn cancel_search(&mut self) -> EditEvent {
        if let Some(search) = self.search.take() {
            self.buffer = search.original;
            self.cursor = search.original_cursor;
        }
        EditEvent::Changed
    }
}

/// Finds where `(column, row)` a text position lands on a screen `cols` wide.
fn advance(mut column: usize, mut row: usize, text: &[char], cols: usize) -> (usize, usize) {
    for ch in text {
        if *ch == '\n' {
            column = 0;
            row += 1;
        } else {
            let width = feos_text::char_width(*ch);
            // A wide character that won't fit goes to the next row whole, as a terminal
            // puts it.
            if width == 2 && column + 1 >= cols && column > 0 {
                column = 0;
                row += 1;
            }
            column += width;
            if column >= cols {
                column = 0;
                row += 1;
            }
        }
    }
    (column, row)
}

/// Counts a prompt's visible width, ignoring colour sequences. Only the part
/// after the last newline counts, because earlier lines never get redrawn.
pub fn prompt_width(prompt: &str) -> usize {
    let last = prompt.rsplit('\n').next().unwrap_or(prompt);
    let mut width = 0usize;
    let mut chars = last.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for inner in chars.by_ref() {
                if inner.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            width += feos_text::char_width(ch);
        }
    }
    width
}

/// Builds the bytes that redraw the line and put the cursor in place.
///
/// `previous_row` is how far the cursor sat below the prompt's row after the
/// last redraw. Returns the bytes and the new value. Only the last line of the
/// prompt is rewritten, so a multi-line prompt stays intact.
pub fn render_line(
    prompt: &str,
    text: &[char],
    cursor: usize,
    cols: usize,
    previous_row: usize,
) -> (Vec<u8>, usize) {
    let cols = cols.max(1);
    let mut out = Vec::new();
    if previous_row > 0 {
        out.extend_from_slice(format!("\x1b[{previous_row}A").as_bytes());
    }
    out.extend_from_slice(b"\r\x1b[J");
    let last_line = prompt.rsplit('\n').next().unwrap_or(prompt);
    out.extend_from_slice(last_line.as_bytes());
    let line: String = text.iter().collect();
    out.extend_from_slice(line.as_bytes());

    let width = prompt_width(prompt);
    let start = (width % cols, width / cols);
    let (end_column, end_row) = advance(start.0, start.1, text, cols);
    let cursor = cursor.min(text.len());
    let (cursor_column, cursor_row) = advance(start.0, start.1, &text[..cursor], cols);

    // A line that exactly fills the row leaves the cursor in the margin; moving
    // down makes every terminal agree on where it is.
    if end_column == 0 && end_row > 0 && text.last().is_some_and(|ch| *ch != '\n') {
        out.push(b'\n');
    }

    let up = end_row - cursor_row;
    if up > 0 {
        out.extend_from_slice(format!("\x1b[{up}A").as_bytes());
    }
    out.push(b'\r');
    if cursor_column > 0 {
        out.extend_from_slice(format!("\x1b[{cursor_column}C").as_bytes());
    }
    (out, cursor_row)
}

/// How far below the prompt's row the end of the line is.
pub fn rows_below(prompt: &str, text: &[char], cols: usize) -> usize {
    let cols = cols.max(1);
    let width = prompt_width(prompt);
    advance(width % cols, width / cols, text, cols).1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::NoHost;
    use feos_coreutils::NoSystem;

    fn plain(ch: char) -> KeyPress {
        KeyPress::plain(KeyCode::Char(ch))
    }

    fn ctrl(ch: char) -> KeyPress {
        KeyPress::ctrl(ch)
    }

    fn type_text(editor: &mut LineEditor, shell: &mut Shell<'_>, text: &str) {
        for ch in text.chars() {
            editor.handle(plain(ch), shell, &NoHost);
        }
    }

    fn text(editor: &LineEditor) -> String {
        editor.text().iter().collect()
    }

    #[test]
    fn edits_the_line() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut editor = LineEditor::new(0);
        type_text(&mut editor, &mut shell, "hello world");
        editor.handle(ctrl('w'), &mut shell, &NoHost);
        assert_eq!(text(&editor), "hello ");
        editor.handle(KeyPress::plain(KeyCode::Home), &mut shell, &NoHost);
        editor.handle(KeyPress::plain(KeyCode::Delete), &mut shell, &NoHost);
        assert_eq!(text(&editor), "ello ");
        editor.handle(ctrl('e'), &mut shell, &NoHost);
        editor.handle(KeyPress::plain(KeyCode::Backspace), &mut shell, &NoHost);
        assert_eq!(text(&editor), "ello");
        editor.handle(ctrl('a'), &mut shell, &NoHost);
        editor.handle(ctrl('k'), &mut shell, &NoHost);
        assert_eq!(text(&editor), "");
    }

    #[test]
    fn multi_line_commands_collect_until_complete() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut editor = LineEditor::new(0);
        type_text(&mut editor, &mut shell, "for i in 1 2; do");
        let event = editor.handle(KeyPress::plain(KeyCode::Enter), &mut shell, &NoHost);
        assert!(matches!(event, EditEvent::Continuation));
        assert!(editor.is_continuation());
        assert_eq!(editor.prompt(&shell), "> ");
        type_text(&mut editor, &mut shell, "echo $i; done");
        match editor.handle(KeyPress::plain(KeyCode::Enter), &mut shell, &NoHost) {
            EditEvent::Submit(line) => assert_eq!(line, "for i in 1 2; do\necho $i; done"),
            _ => panic!("expected a complete command"),
        }
    }

    #[test]
    fn an_open_quote_continues_on_the_next_line() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut editor = LineEditor::new(0);
        type_text(&mut editor, &mut shell, "echo 'a");
        assert!(matches!(
            editor.handle(KeyPress::plain(KeyCode::Enter), &mut shell, &NoHost),
            EditEvent::Continuation
        ));
        type_text(&mut editor, &mut shell, "b'");
        assert!(matches!(
            editor.handle(KeyPress::plain(KeyCode::Enter), &mut shell, &NoHost),
            EditEvent::Submit(line) if line == "echo 'a\nb'"
        ));
    }

    #[test]
    fn history_navigation_restores_the_draft() {
        let mut shell = Shell::new(None, &NoSystem);
        shell.history = alloc::vec![String::from("one"), String::from("two")];
        let mut editor = LineEditor::new(2);
        type_text(&mut editor, &mut shell, "draft");
        editor.handle(KeyPress::plain(KeyCode::Up), &mut shell, &NoHost);
        assert_eq!(text(&editor), "two");
        editor.handle(KeyPress::plain(KeyCode::Up), &mut shell, &NoHost);
        assert_eq!(text(&editor), "one");
        editor.handle(KeyPress::plain(KeyCode::Up), &mut shell, &NoHost);
        assert_eq!(text(&editor), "one");
        editor.handle(KeyPress::plain(KeyCode::Down), &mut shell, &NoHost);
        editor.handle(KeyPress::plain(KeyCode::Down), &mut shell, &NoHost);
        assert_eq!(text(&editor), "draft");
    }

    #[test]
    fn reverse_search_cycles_through_older_matches() {
        let mut shell = Shell::new(None, &NoSystem);
        shell.history = alloc::vec![
            String::from("echo first"),
            String::from("ls"),
            String::from("echo second"),
        ];
        let mut editor = LineEditor::new(3);
        editor.handle(ctrl('r'), &mut shell, &NoHost);
        type_text(&mut editor, &mut shell, "echo");
        assert_eq!(text(&editor), "echo second");
        assert!(editor.prompt(&shell).contains("echo"));
        editor.handle(ctrl('r'), &mut shell, &NoHost);
        assert_eq!(text(&editor), "echo first");
        editor.handle(ctrl('r'), &mut shell, &NoHost);
        assert_eq!(text(&editor), "echo first");
        // Enter accepts the match and runs it.
        assert!(matches!(
            editor.handle(KeyPress::plain(KeyCode::Enter), &mut shell, &NoHost),
            EditEvent::Submit(line) if line == "echo first"
        ));
    }

    #[test]
    fn cancelling_a_search_restores_the_line() {
        let mut shell = Shell::new(None, &NoSystem);
        shell.history = alloc::vec![String::from("echo first")];
        let mut editor = LineEditor::new(1);
        type_text(&mut editor, &mut shell, "draft");
        editor.handle(ctrl('r'), &mut shell, &NoHost);
        type_text(&mut editor, &mut shell, "echo");
        assert_eq!(text(&editor), "echo first");
        editor.handle(KeyPress::plain(KeyCode::Escape), &mut shell, &NoHost);
        assert_eq!(text(&editor), "draft");
    }

    #[test]
    fn ctrl_d_closes_only_an_empty_line() {
        let mut shell = Shell::new(None, &NoSystem);
        let mut editor = LineEditor::new(0);
        assert!(matches!(
            editor.handle(ctrl('d'), &mut shell, &NoHost),
            EditEvent::Eof
        ));
        type_text(&mut editor, &mut shell, "x");
        assert!(matches!(
            editor.handle(ctrl('d'), &mut shell, &NoHost),
            EditEvent::None
        ));
    }

    #[test]
    fn rendering_stays_on_one_row_for_short_lines() {
        let text: Vec<char> = "abc".chars().collect();
        let (out, row) = render_line("$ ", &text, 3, 80, 0);
        assert_eq!(row, 0);
        assert_eq!(out, b"\r\x1b[J$ abc\r\x1b[5C");
        let (out, _) = render_line("$ ", &text, 1, 80, 0);
        assert!(out.ends_with(b"\r\x1b[3C"));
    }

    #[test]
    fn rendering_accounts_for_wrapped_lines() {
        let text: Vec<char> = "x".repeat(25).chars().collect();
        // Prompt 2 wide on 10 columns: 27 cells, three rows.
        let (out, row) = render_line("$ ", &text, 25, 10, 0);
        assert_eq!(row, 2);
        assert!(out.ends_with(b"\r\x1b[7C"));
        // The next redraw climbs back to the prompt's row.
        let (out, row) = render_line("$ ", &text, 0, 10, 2);
        assert!(out.starts_with(b"\x1b[2A\r\x1b[J"));
        assert_eq!(row, 0);
        assert!(out.ends_with(b"\x1b[2A\r\x1b[2C"));
    }

    #[test]
    fn rendering_handles_an_exactly_full_row() {
        let text: Vec<char> = "x".repeat(8).chars().collect();
        let (out, row) = render_line("$ ", &text, 8, 10, 0);
        assert_eq!(row, 1);
        // The newline forces the cursor onto the next row.
        assert!(out.ends_with(b"\n\r"));
    }

    #[test]
    fn only_the_last_prompt_line_is_redrawn() {
        let text: Vec<char> = "a".chars().collect();
        let (out, _) = render_line("top\n$ ", &text, 1, 80, 0);
        let shown = String::from_utf8(out).unwrap();
        assert!(!shown.contains("top"));
        assert!(shown.contains("$ a"));
        assert_eq!(prompt_width("top line\n$ "), 2);
        assert_eq!(prompt_width("\x1b[32m$\x1b[0m "), 2);
    }

    #[test]
    fn rows_below_counts_wrapped_rows() {
        let text: Vec<char> = "x".repeat(25).chars().collect();
        assert_eq!(rows_below("$ ", &text, 10), 2);
        assert_eq!(rows_below("$ ", &[], 10), 0);
    }
}
