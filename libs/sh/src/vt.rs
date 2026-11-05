//! A VT100/xterm-style terminal emulator.
//!
//! [`Vt`] turns a byte stream into a grid of cells: cursor addressing, colours,
//! line wrap, scroll regions, a capped scrollback ring and the alternate screen.
//! It holds no idea of how cells get drawn, so it can be tested on the host; the
//! desktop's Terminal window renders it with the bitmap font.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

/// A colour index meaning "use the theme's default".
pub const DEFAULT_COLOR: u8 = 255;

/// Bold or bright text.
pub const ATTR_BOLD: u8 = 1 << 0;
/// Underlined text.
pub const ATTR_UNDERLINE: u8 = 1 << 1;
/// Foreground and background swapped.
pub const ATTR_INVERSE: u8 = 1 << 2;

/// Lines kept above the screen. The ring is capped, so output cannot grow the
/// heap without bound.
pub const MAX_SCROLLBACK: usize = 1000;

const MAX_PARAMS: usize = 8;
const TAB_WIDTH: usize = 8;

/// One character cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The character.
    pub ch: char,
    /// Foreground colour: 0-255, with 255 as the default.
    pub fg: u8,
    /// Background colour: 0-255, with 255 as the default.
    pub bg: u8,
    /// `ATTR_*` flags.
    pub attrs: u8,
}

impl Cell {
    const BLANK: Self = Self {
        ch: ' ',
        fg: DEFAULT_COLOR,
        bg: DEFAULT_COLOR,
        attrs: 0,
    };

    fn is_blank(&self) -> bool {
        *self == Self::BLANK
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    /// `ESC (` and friends take one more byte, which means nothing here.
    Charset,
    Csi,
    Osc,
    OscEscape,
}

#[derive(Clone, Copy, Default)]
struct Pen {
    fg: u8,
    bg: u8,
    attrs: u8,
}

#[derive(Clone, Copy)]
struct SavedCursor {
    x: usize,
    y: usize,
    pen: Pen,
}

struct AltScreen {
    cells: Vec<Cell>,
    cursor: (usize, usize),
}

/// A terminal screen with scrollback.
pub struct Vt {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    scrollback: VecDeque<Vec<Cell>>,
    x: usize,
    y: usize,
    /// The last column was written, and the next character wraps.
    wrap_pending: bool,
    pen: Pen,
    saved: Option<SavedCursor>,
    top: usize,
    bottom: usize,
    alt: Option<AltScreen>,
    cursor_visible: bool,
    state: State,
    params: [u16; MAX_PARAMS],
    param_count: usize,
    param_active: bool,
    private: bool,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

impl Vt {
    /// Creates a blank screen.
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: alloc::vec![Cell::BLANK; cols * rows],
            scrollback: VecDeque::new(),
            x: 0,
            y: 0,
            wrap_pending: false,
            pen: Pen {
                fg: DEFAULT_COLOR,
                bg: DEFAULT_COLOR,
                attrs: 0,
            },
            saved: None,
            top: 0,
            bottom: rows - 1,
            alt: None,
            cursor_visible: true,
            state: State::Ground,
            params: [0; MAX_PARAMS],
            param_count: 0,
            param_active: false,
            private: false,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        }
    }

    /// Screen width in cells.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Screen height in cells.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The cursor as `(column, row)`.
    pub fn cursor(&self) -> (usize, usize) {
        (self.x, self.y)
    }

    /// False after `ESC [ ? 25 l`.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Number of lines stored above the screen.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Changes the screen size. Rows pushed off the top go to the scrollback,
    /// and rows are cut or padded rather than rewrapped.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        // Keep the cursor's row on the new screen.
        while self.y >= rows {
            self.push_top_to_scrollback();
            let start = self.cols;
            let end = self.rows * self.cols;
            self.cells.copy_within(start..end, 0);
            self.cells[(self.rows - 1) * self.cols..].fill(Cell::BLANK);
            self.y -= 1;
        }
        let mut cells = alloc::vec![Cell::BLANK; cols * rows];
        for row in 0..rows.min(self.rows) {
            let from = row * self.cols;
            let to = row * cols;
            let width = cols.min(self.cols);
            cells[to..to + width].copy_from_slice(&self.cells[from..from + width]);
        }
        self.cells = cells;
        self.cols = cols;
        self.rows = rows;
        self.x = self.x.min(cols - 1);
        self.y = self.y.min(rows - 1);
        self.top = 0;
        self.bottom = rows - 1;
        self.wrap_pending = false;
        self.alt = None;
    }

    /// Clears the screen and scrollback.
    pub fn clear_all(&mut self) {
        self.cells.fill(Cell::BLANK);
        self.scrollback.clear();
        self.x = 0;
        self.y = 0;
        self.wrap_pending = false;
    }

    /// The cells of one displayed row. `scroll` lines back from the live screen,
    /// row 0 being the top of the window.
    pub fn visible_row(&self, row: usize, scroll: usize) -> &[Cell] {
        let scroll = scroll.min(self.scrollback.len());
        let first = self.scrollback.len() - scroll;
        let index = first + row;
        if index < self.scrollback.len() {
            return &self.scrollback[index];
        }
        let screen_row = index - self.scrollback.len();
        self.row_slice(screen_row.min(self.rows - 1))
    }

    fn row_slice(&self, row: usize) -> &[Cell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }

    /// All text, one string per line with trailing blanks removed. Blank rows at
    /// the bottom of the screen are left out.
    pub fn text_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .scrollback
            .iter()
            .map(|cells| line_text(cells))
            .collect();
        for row in 0..self.rows {
            lines.push(line_text(self.row_slice(row)));
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    /// Processes output bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.feed_byte(*byte);
        }
    }

    fn feed_byte(&mut self, byte: u8) {
        match self.state {
            State::Ground => self.ground(byte),
            State::Escape => self.escape(byte),
            State::Charset => self.state = State::Ground,
            State::Csi => self.csi(byte),
            State::Osc => match byte {
                0x07 => self.state = State::Ground,
                0x1b => self.state = State::OscEscape,
                _ => {}
            },
            State::OscEscape => {
                // `ESC \` ends the string; anything else starts over.
                self.state = if byte == b'\\' {
                    State::Ground
                } else {
                    State::Osc
                };
            }
        }
    }

    fn ground(&mut self, byte: u8) {
        if self.utf8_need > 0 {
            if byte & 0xc0 == 0x80 {
                self.utf8[self.utf8_len] = byte;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_need {
                    let ch = core::str::from_utf8(&self.utf8[..self.utf8_len])
                        .ok()
                        .and_then(|text| text.chars().next())
                        .unwrap_or('\u{fffd}');
                    self.utf8_need = 0;
                    self.utf8_len = 0;
                    self.print(ch);
                }
                return;
            }
            self.utf8_need = 0;
            self.utf8_len = 0;
            self.print('\u{fffd}');
        }

        match byte {
            0x1b => self.state = State::Escape,
            b'\n' | 0x0b | 0x0c => {
                // There is no tty layer, so a newline also returns the carriage.
                self.x = 0;
                self.wrap_pending = false;
                self.index();
            }
            b'\r' => {
                self.x = 0;
                self.wrap_pending = false;
            }
            0x08 => {
                self.x = self.x.saturating_sub(1);
                self.wrap_pending = false;
            }
            b'\t' => {
                self.x = ((self.x / TAB_WIDTH + 1) * TAB_WIDTH).min(self.cols - 1);
                self.wrap_pending = false;
            }
            0x00..=0x1f | 0x7f => {}
            0x20..=0x7e => self.print(byte as char),
            0xc2..=0xdf => self.start_utf8(byte, 2),
            0xe0..=0xef => self.start_utf8(byte, 3),
            0xf0..=0xf4 => self.start_utf8(byte, 4),
            _ => self.print('\u{fffd}'),
        }
    }

    fn start_utf8(&mut self, byte: u8, need: usize) {
        self.utf8[0] = byte;
        self.utf8_len = 1;
        self.utf8_need = need;
    }

    fn print(&mut self, ch: char) {
        if self.wrap_pending {
            self.x = 0;
            self.wrap_pending = false;
            self.index();
        }
        let cell = Cell {
            ch,
            fg: self.pen.fg,
            bg: self.pen.bg,
            attrs: self.pen.attrs,
        };
        let at = self.y * self.cols + self.x;
        if let Some(slot) = self.cells.get_mut(at) {
            *slot = cell;
        }
        if self.x + 1 >= self.cols {
            self.wrap_pending = true;
        } else {
            self.x += 1;
        }
    }

    /// Moves down one row, scrolling at the bottom of the region.
    fn index(&mut self) {
        if self.y == self.bottom {
            self.scroll_up(1);
        } else if self.y + 1 < self.rows {
            self.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.y == self.top {
            self.scroll_down(1);
        } else {
            self.y = self.y.saturating_sub(1);
        }
    }

    fn push_top_to_scrollback(&mut self) {
        let mut line = self.row_slice(0).to_vec();
        while line.last().is_some_and(Cell::is_blank) {
            line.pop();
        }
        if self.scrollback.len() >= MAX_SCROLLBACK {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(line);
    }

    /// Scrolls the region up, with text off the top of the main screen going to
    /// the scrollback.
    fn scroll_up(&mut self, count: usize) {
        for _ in 0..count.min(self.bottom - self.top + 1) {
            if self.top == 0 && self.alt.is_none() {
                self.push_top_to_scrollback();
            }
            self.shift_rows_up(self.top);
        }
    }

    /// Removes row `from`, moving the rows below it up inside the region.
    fn shift_rows_up(&mut self, from: usize) {
        let start = (from + 1) * self.cols;
        let end = (self.bottom + 1) * self.cols;
        self.cells.copy_within(start..end, from * self.cols);
        self.cells[self.bottom * self.cols..end].fill(Cell::BLANK);
    }

    /// Inserts a blank row at `from`, dropping the region's bottom row.
    fn shift_rows_down(&mut self, from: usize) {
        let start = from * self.cols;
        let end = self.bottom * self.cols;
        self.cells.copy_within(start..end, start + self.cols);
        self.cells[start..start + self.cols].fill(Cell::BLANK);
    }

    fn scroll_down(&mut self, count: usize) {
        for _ in 0..count.min(self.bottom - self.top + 1) {
            self.shift_rows_down(self.top);
        }
    }

    fn escape(&mut self, byte: u8) {
        self.state = State::Ground;
        match byte {
            b'[' => {
                self.params = [0; MAX_PARAMS];
                self.param_count = 0;
                self.param_active = false;
                self.private = false;
                self.state = State::Csi;
            }
            b']' | b'P' | b'^' | b'_' | b'X' => self.state = State::Osc,
            b'(' | b')' | b'*' | b'+' | b'#' | b'%' => self.state = State::Charset,
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.index(),
            b'E' => {
                self.x = 0;
                self.index();
            }
            b'M' => self.reverse_index(),
            b'c' => {
                let (cols, rows) = (self.cols, self.rows);
                let scrollback = core::mem::take(&mut self.scrollback);
                *self = Self::new(cols, rows);
                self.scrollback = scrollback;
            }
            _ => {}
        }
    }

    fn csi(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                if self.param_count < MAX_PARAMS {
                    let value = &mut self.params[self.param_count];
                    *value = value
                        .saturating_mul(10)
                        .saturating_add(u16::from(byte - b'0'));
                    self.param_active = true;
                }
            }
            b';' | b':' => {
                self.param_count = (self.param_count + 1).min(MAX_PARAMS);
                self.param_active = false;
            }
            b'?' | b'>' | b'<' | b'=' => self.private = true,
            // Intermediate bytes change nothing this emulator supports.
            0x20..=0x2f => {}
            0x40..=0x7e => {
                self.state = State::Ground;
                if self.param_active {
                    self.param_count = (self.param_count + 1).min(MAX_PARAMS);
                }
                self.dispatch_csi(byte);
            }
            0x1b => self.state = State::Escape,
            _ => {}
        }
    }

    fn param(&self, index: usize, default: usize) -> usize {
        if index >= self.param_count {
            return default;
        }
        match self.params[index] {
            0 => default,
            value => usize::from(value),
        }
    }

    fn dispatch_csi(&mut self, command: u8) {
        if self.private {
            self.private_mode(command);
            return;
        }
        let count = self.param(0, 1);
        match command {
            b'A' => self.move_vertical(self.y.saturating_sub(count)),
            b'B' | b'e' => self.move_vertical(self.y.saturating_add(count)),
            b'C' | b'a' => self.move_to_column(self.x.saturating_add(count)),
            b'D' => self.move_to_column(self.x.saturating_sub(count)),
            b'E' => {
                self.move_vertical(self.y.saturating_add(count));
                self.x = 0;
            }
            b'F' => {
                self.move_vertical(self.y.saturating_sub(count));
                self.x = 0;
            }
            b'G' | b'`' => self.move_to_column(count - 1),
            b'd' => {
                self.y = (count - 1).min(self.rows - 1);
                self.wrap_pending = false;
            }
            b'H' | b'f' => {
                self.y = (self.param(0, 1) - 1).min(self.rows - 1);
                self.x = (self.param(1, 1) - 1).min(self.cols - 1);
                self.wrap_pending = false;
            }
            b'J' => self.erase_display(self.param(0, 0)),
            b'K' => self.erase_line(self.param(0, 0)),
            b'L' => self.insert_lines(count),
            b'M' => self.delete_lines(count),
            b'P' => self.delete_chars(count),
            b'@' => self.insert_chars(count),
            b'X' => {
                let end = (self.x + count).min(self.cols);
                let row = self.y * self.cols;
                self.cells[row + self.x..row + end].fill(Cell::BLANK);
            }
            b'S' => self.scroll_up(count),
            b'T' => self.scroll_down(count),
            b'm' => self.set_graphics(),
            b'r' => {
                let top = self.param(0, 1) - 1;
                let bottom = self.param(1, self.rows) - 1;
                if top < bottom && bottom < self.rows {
                    self.top = top;
                    self.bottom = bottom;
                    self.x = 0;
                    self.y = 0;
                    self.wrap_pending = false;
                }
            }
            b's' => self.save_cursor(),
            b'u' => self.restore_cursor(),
            _ => {}
        }
    }

    fn private_mode(&mut self, command: u8) {
        if command != b'h' && command != b'l' {
            return;
        }
        let enable = command == b'h';
        for index in 0..self.param_count {
            match self.params[index] {
                25 => self.cursor_visible = enable,
                47 | 1047 | 1049 => self.set_alternate_screen(enable),
                _ => {}
            }
        }
    }

    fn set_alternate_screen(&mut self, enable: bool) {
        if enable && self.alt.is_none() {
            let cells = core::mem::replace(
                &mut self.cells,
                alloc::vec![Cell::BLANK; self.cols * self.rows],
            );
            self.alt = Some(AltScreen {
                cells,
                cursor: (self.x, self.y),
            });
            self.x = 0;
            self.y = 0;
        } else if !enable {
            if let Some(saved) = self.alt.take() {
                self.cells = saved.cells;
                self.x = saved.cursor.0;
                self.y = saved.cursor.1;
            }
        }
        self.wrap_pending = false;
    }

    fn move_vertical(&mut self, row: usize) {
        // Relative moves stop at the scroll region when they start inside it.
        let (low, high) = if (self.top..=self.bottom).contains(&self.y) {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        self.y = row.clamp(low, high);
        self.wrap_pending = false;
    }

    fn move_to_column(&mut self, column: usize) {
        self.x = column.min(self.cols - 1);
        self.wrap_pending = false;
    }

    fn erase_display(&mut self, mode: usize) {
        let here = self.y * self.cols + self.x;
        match mode {
            0 => self.cells[here..].fill(Cell::BLANK),
            1 => self.cells[..=here].fill(Cell::BLANK),
            2 => self.cells.fill(Cell::BLANK),
            3 => self.scrollback.clear(),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let row = self.y * self.cols;
        let (from, to) = match mode {
            0 => (self.x, self.cols),
            1 => (0, self.x + 1),
            2 => (0, self.cols),
            _ => return,
        };
        self.cells[row + from..row + to].fill(Cell::BLANK);
    }

    fn insert_lines(&mut self, count: usize) {
        if !(self.top..=self.bottom).contains(&self.y) {
            return;
        }
        for _ in 0..count.min(self.bottom - self.y + 1) {
            self.shift_rows_down(self.y);
        }
        self.x = 0;
    }

    fn delete_lines(&mut self, count: usize) {
        if !(self.top..=self.bottom).contains(&self.y) {
            return;
        }
        for _ in 0..count.min(self.bottom - self.y + 1) {
            self.shift_rows_up(self.y);
        }
        self.x = 0;
    }

    fn delete_chars(&mut self, count: usize) {
        let row = self.y * self.cols;
        let count = count.min(self.cols - self.x);
        self.cells
            .copy_within(row + self.x + count..row + self.cols, row + self.x);
        self.cells[row + self.cols - count..row + self.cols].fill(Cell::BLANK);
    }

    fn insert_chars(&mut self, count: usize) {
        let row = self.y * self.cols;
        let count = count.min(self.cols - self.x);
        self.cells
            .copy_within(row + self.x..row + self.cols - count, row + self.x + count);
        self.cells[row + self.x..row + self.x + count].fill(Cell::BLANK);
    }

    fn save_cursor(&mut self) {
        self.saved = Some(SavedCursor {
            x: self.x,
            y: self.y,
            pen: self.pen,
        });
    }

    fn restore_cursor(&mut self) {
        if let Some(saved) = self.saved {
            self.x = saved.x.min(self.cols - 1);
            self.y = saved.y.min(self.rows - 1);
            self.pen = saved.pen;
            self.wrap_pending = false;
        }
    }

    fn set_graphics(&mut self) {
        if self.param_count == 0 {
            self.reset_pen();
            return;
        }
        let mut index = 0usize;
        while index < self.param_count {
            let value = self.params[index];
            index += 1;
            match value {
                0 => self.reset_pen(),
                1 => self.pen.attrs |= ATTR_BOLD,
                4 => self.pen.attrs |= ATTR_UNDERLINE,
                7 => self.pen.attrs |= ATTR_INVERSE,
                22 => self.pen.attrs &= !ATTR_BOLD,
                24 => self.pen.attrs &= !ATTR_UNDERLINE,
                27 => self.pen.attrs &= !ATTR_INVERSE,
                30..=37 => self.pen.fg = (value - 30) as u8,
                39 => self.pen.fg = DEFAULT_COLOR,
                40..=47 => self.pen.bg = (value - 40) as u8,
                49 => self.pen.bg = DEFAULT_COLOR,
                90..=97 => self.pen.fg = (value - 90 + 8) as u8,
                100..=107 => self.pen.bg = (value - 100 + 8) as u8,
                38 | 48 => {
                    let color = match self.params.get(index) {
                        Some(5) => {
                            let color = self.params.get(index + 1).copied().unwrap_or(0);
                            index += 2;
                            (color & 0xff) as u8
                        }
                        Some(2) => {
                            let channel =
                                |at: usize| self.params.get(at).copied().unwrap_or(0).min(255);
                            let color = nearest_color(
                                channel(index + 1) as u8,
                                channel(index + 2) as u8,
                                channel(index + 3) as u8,
                            );
                            index += 4;
                            color
                        }
                        _ => continue,
                    };
                    if value == 38 {
                        self.pen.fg = color;
                    } else {
                        self.pen.bg = color;
                    }
                }
                _ => {}
            }
        }
    }

    fn reset_pen(&mut self) {
        self.pen = Pen {
            fg: DEFAULT_COLOR,
            bg: DEFAULT_COLOR,
            attrs: 0,
        };
    }
}

fn line_text(cells: &[Cell]) -> String {
    let mut line: String = cells.iter().map(|cell| cell.ch).collect();
    let kept = line.trim_end_matches(' ').len();
    line.truncate(kept);
    line
}

/// Maps a true colour to the xterm 6x6x6 cube.
fn nearest_color(r: u8, g: u8, b: u8) -> u8 {
    let level = |value: u8| -> u8 {
        if value < 48 {
            0
        } else if value < 115 {
            1
        } else {
            (value - 35) / 40
        }
    };
    16 + 36 * level(r) + 6 * level(g) + level(b)
}

/// The xterm palette: returns `(r, g, b)` for a colour index.
pub fn palette(index: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    match index {
        0..=15 => BASE[usize::from(index)],
        16..=231 => {
            let value = index - 16;
            let channel = |level: u8| if level == 0 { 0 } else { 55 + level * 40 };
            (
                channel(value / 36),
                channel(value / 6 % 6),
                channel(value % 6),
            )
        }
        _ => {
            let gray = 8 + (index - 232).saturating_mul(10);
            (gray, gray, gray)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(vt: &Vt) -> Vec<String> {
        vt.text_lines()
    }

    #[test]
    fn prints_and_wraps() {
        let mut vt = Vt::new(4, 3);
        vt.feed(b"abcdef\n");
        assert_eq!(screen(&vt), ["abcd", "ef"]);
        assert_eq!(vt.cursor(), (0, 2));
    }

    #[test]
    fn exactly_full_rows_wrap_only_on_the_next_character() {
        let mut vt = Vt::new(4, 3);
        vt.feed(b"abcd");
        assert_eq!(vt.cursor(), (3, 0));
        vt.feed(b"\r");
        assert_eq!(vt.cursor(), (0, 0));
        vt.feed(b"x");
        assert_eq!(screen(&vt), ["xbcd"]);
    }

    #[test]
    fn carriage_return_overwrites() {
        let mut vt = Vt::new(10, 3);
        vt.feed(b"12345\rab");
        assert_eq!(screen(&vt), ["ab345"]);
    }

    #[test]
    fn tabs_go_to_stops() {
        let mut vt = Vt::new(20, 2);
        vt.feed(b"a\tb");
        assert_eq!(screen(&vt), ["a       b"]);
    }

    #[test]
    fn osc_strings_do_not_leak() {
        let mut vt = Vt::new(20, 2);
        vt.feed(b"\x1b]0;title\x07x\x1b]2;t\x1b\\y");
        assert_eq!(screen(&vt), ["xy"]);
    }

    #[test]
    fn colours_are_kept() {
        let mut vt = Vt::new(10, 2);
        vt.feed(b"\x1b[31;1mr\x1b[0mn\x1b[38;5;200mp\x1b[44mq");
        let row = vt.visible_row(0, 0);
        assert_eq!(row[0].fg, 1);
        assert_eq!(row[0].attrs, ATTR_BOLD);
        assert_eq!(row[1].fg, DEFAULT_COLOR);
        assert_eq!(row[2].fg, 200);
        assert_eq!(row[3].bg, 4);
    }

    #[test]
    fn clear_screen_only_clears_what_came_before() {
        let mut vt = Vt::new(10, 3);
        vt.feed(b"before\n\x1b[2J\x1b[Hafter");
        assert_eq!(screen(&vt), ["after"]);
    }

    #[test]
    fn cursor_addressing_works() {
        let mut vt = Vt::new(10, 4);
        vt.feed(b"\x1b[2;3Hx\x1b[1;1Hy\x1b[4;10Hz");
        assert_eq!(screen(&vt), ["y", "  x", "", "         z"]);
        vt.feed(b"\x1b[2;3H\x1b[A\x1b[2C!");
        assert_eq!(vt.visible_row(0, 0)[4].ch, '!');
    }

    #[test]
    fn erase_commands_work() {
        let mut vt = Vt::new(5, 2);
        vt.feed(b"abcde\nfghij\x1b[1;3H\x1b[K");
        assert_eq!(screen(&vt), ["ab", "fghij"]);
        vt.feed(b"\x1b[2;2H\x1b[1K");
        assert_eq!(screen(&vt), ["ab", "  hij"]);
        vt.feed(b"\x1b[1;1H\x1b[J");
        assert_eq!(screen(&vt).len(), 0);
    }

    #[test]
    fn scrolling_fills_a_capped_ring() {
        let mut vt = Vt::new(10, 2);
        for number in 0..(MAX_SCROLLBACK + 50) {
            vt.feed(alloc::format!("{number}\n").as_bytes());
        }
        assert_eq!(vt.scrollback_len(), MAX_SCROLLBACK);
        assert_eq!(vt.visible_row(0, 2)[0].ch, '1');
        assert!(screen(&vt).len() <= MAX_SCROLLBACK + 2);
    }

    #[test]
    fn scroll_regions_keep_the_rest_in_place() {
        let mut vt = Vt::new(5, 4);
        vt.feed(b"\x1b[1;1Htop\x1b[4;1Hbot\x1b[2;3r\x1b[3;1Ha\nb\nc");
        let lines = screen(&vt);
        assert_eq!(lines[0], "top");
        assert_eq!(lines[3], "bot");
        assert_eq!(vt.scrollback_len(), 0);
    }

    #[test]
    fn inserting_and_deleting_lines_and_characters() {
        let mut vt = Vt::new(5, 3);
        vt.feed(b"a\nb\nc\x1b[2;1H\x1b[L");
        assert_eq!(screen(&vt), ["a", "", "b"]);
        vt.feed(b"\x1b[M");
        assert_eq!(screen(&vt), ["a", "b"]);
        vt.feed(b"\x1b[1;1Habcd\x1b[1;2H\x1b[2P");
        assert_eq!(screen(&vt)[0], "ad");
        vt.feed(b"\x1b[1;1H\x1b[2@");
        assert_eq!(screen(&vt)[0], "  ad");
        vt.feed(b"\x1b[1;3H\x1b[X");
        assert_eq!(screen(&vt)[0], "   d");
    }

    #[test]
    fn the_alternate_screen_restores_what_was_there() {
        let mut vt = Vt::new(10, 3);
        vt.feed(b"main");
        vt.feed(b"\x1b[?1049h\x1b[2;2Halt");
        assert_eq!(screen(&vt), ["", " alt"]);
        vt.feed(b"\x1b[?1049l");
        assert_eq!(screen(&vt), ["main"]);
        assert_eq!(vt.cursor(), (4, 0));
    }

    #[test]
    fn resizing_keeps_the_cursor_row() {
        let mut vt = Vt::new(4, 4);
        vt.feed(b"a\nb\nc\nd");
        vt.resize(6, 2);
        assert_eq!(vt.cols(), 6);
        assert_eq!(screen(&vt), ["a", "b", "c", "d"]);
        assert_eq!(vt.cursor().1, 1);
        vt.resize(1, 1);
        vt.feed(b"\xc3\xa9");
        assert!(screen(&vt).last().unwrap().contains('é'));
    }

    #[test]
    fn hostile_sequences_stay_in_bounds() {
        let mut vt = Vt::new(3, 2);
        for bytes in [
            &b"\x1b[999;999H"[..],
            b"\x1b[999A\x1b[999B\x1b[999C\x1b[999D",
            b"\x1b[999L\x1b[999M\x1b[999P\x1b[999@\x1b[999X",
            b"\x1b[999S\x1b[999T",
            b"\x1b[5;1r\x1b[2;1r\x1b[0;0r",
            b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12m",
            b"\x1b[38;5m\x1b[38;2m\x1b[48;2;1",
            b"\x1b[65535;65535;65535H",
            b"\xff\xfe\xc3\xe2\x82",
            b"\x1b[?1049h\x1b[?1049h\x1b[?1049l\x1b[?1049l",
        ] {
            vt.feed(bytes);
            let (x, y) = vt.cursor();
            assert!(x < 3 && y < 2);
        }
    }

    #[test]
    fn the_palette_covers_every_index() {
        for index in 0..=255u8 {
            let _ = palette(index);
        }
        assert_eq!(palette(16), (0, 0, 0));
        assert_eq!(palette(231), (255, 255, 255));
        assert_eq!(nearest_color(255, 0, 0), 196);
    }
}
