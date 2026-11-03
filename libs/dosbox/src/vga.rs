//! VGA, EGA and CGA text and graphics adapters.
//!
//! Text mode keeps characters in the standard `B800:0000` buffer. Graphics
//! modes use the memory a real adapter uses:
//!
//! * mode 13h — 320x200, 256 colours, linear at `A000:0000`;
//! * CGA modes 4/5/6 — 320x200 four colours or 640x200 two colours at
//!   `B800:0000` with the even/odd scan-line banks;
//! * EGA/VGA planar modes — four 64 KiB planes behind the graphics
//!   controller's map-mask, read-map, set/reset and bit-mask registers.
//!
//! The renderer turns the active mode into the same 640x400 XRGB image the
//! desktop blits, so the window code does not care which adapter is selected.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use feos_gfx::Canvas;

use crate::font;
use crate::memory::{Memory, BDA_BASE, MONO_BASE, VIDEO_BASE};

/// Text columns in the default mode.
pub const COLUMNS: usize = 80;
/// Text rows in the default mode.
pub const ROWS: usize = 25;
/// Pixel width of the rendered screen.
pub const SCREEN_WIDTH: usize = 640;
/// Pixel height of the rendered screen.
pub const SCREEN_HEIGHT: usize = 400;

/// What kind of framebuffer the current mode uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphicsKind {
    Text,
    Cga320,
    Cga640,
    Vga13,
    Planar,
}

/// The video adapter state.
pub struct Vga {
    pub mode: u8,
    pub columns: usize,
    pub rows: usize,
    pub page: u8,
    /// Cursor position as `(row, column)` in text mode.
    pub cursor: (usize, usize),
    pub cursor_start: u8,
    pub cursor_end: u8,
    pub cursor_enabled: bool,
    /// First displayed word in the text buffer.
    pub start_addr: u16,
    pub crtc_index: u8,
    pub crtc: [u8; 32],
    pub bell: u32,
    pub blink: bool,
    pub dirty: bool,

    pub kind: GraphicsKind,
    pub width: usize,
    pub height: usize,
    /// 6-bit DAC entries for VGA mode 13h and planar modes.
    pub palette: [[u8; 3]; 256],
    pub dac_index: u8,
    pub dac_component: u8,
    /// CGA palette select (0 or 1) and intensity.
    pub cga_palette: u8,
    pub cga_intensity: bool,
    /// Graphics cursor used by the BIOS teletype in graphics modes.
    pub graphics_cursor: (usize, usize),
    pub graphics_color: u8,

    /// EGA/VGA planar memory, four 64 KiB planes.
    pub vram: Vec<u8>,
    pub plane_mask: u8,
    pub read_plane: u8,
    pub set_reset: u8,
    pub enable_set_reset: u8,
    pub bit_mask: u8,
    pub logic_op: u8,
    pub graphics_index: u8,
    pub sequencer_index: u8,
    pub attribute_index: u8,
    pub attribute_flipflop: bool,
}

impl Default for Vga {
    fn default() -> Self {
        Self::new()
    }
}

impl Vga {
    pub fn new() -> Self {
        let mut vga = Self {
            mode: 3,
            columns: COLUMNS,
            rows: ROWS,
            page: 0,
            cursor: (0, 0),
            cursor_start: 0x0E,
            cursor_end: 0x0F,
            cursor_enabled: true,
            start_addr: 0,
            crtc_index: 0,
            crtc: [0; 32],
            bell: 0,
            blink: true,
            dirty: true,
            kind: GraphicsKind::Text,
            width: 640,
            height: 400,
            palette: [[0; 3]; 256],
            dac_index: 0,
            dac_component: 0,
            cga_palette: 1,
            cga_intensity: false,
            graphics_cursor: (0, 0),
            graphics_color: 0x0F,
            vram: vec![0; 4 * 0x1_0000],
            plane_mask: 0x0F,
            read_plane: 0,
            set_reset: 0,
            enable_set_reset: 0,
            bit_mask: 0xFF,
            logic_op: 0,
            graphics_index: 0,
            sequencer_index: 0,
            attribute_index: 0,
            attribute_flipflop: false,
        };
        vga.reset_palette();
        vga
    }

    fn reset_palette(&mut self) {
        // The standard VGA BIOS palette: 16 EGA colours, 16 greys, a 6x6x6
        // colour cube and eight black entries.
        for index in 0..16 {
            let (r, g, b) = palette16(index);
            self.palette[index] = [r >> 2, g >> 2, b >> 2];
        }
        for index in 16..32 {
            let value = ((index - 16) * 255 / 15) as u8;
            self.palette[index] = [value >> 2; 3];
        }
        let mut index = 32;
        for r in 0..6 {
            for g in 0..6 {
                for b in 0..6 {
                    self.palette[index] = [
                        (r * 51) as u8 >> 2,
                        (g * 51) as u8 >> 2,
                        (b * 51) as u8 >> 2,
                    ];
                    index += 1;
                }
            }
        }
    }

    /// Resets to mode 3 and clears the screen.
    pub fn reset(&mut self, mem: &mut Memory) {
        let vram = core::mem::take(&mut self.vram);
        *self = Self::new();
        self.vram = vram;
        self.clear(mem, 0x07);
        self.sync_bios(mem);
    }

    fn base(&self) -> u32 {
        if self.mode == 7 {
            MONO_BASE
        } else {
            VIDEO_BASE
        }
    }

    fn cell_offset(&self, row: usize, column: usize) -> u32 {
        self.base()
            .wrapping_add((self.start_addr as u32) * 2)
            .wrapping_add(((row * self.columns + column) * 2) as u32)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_text(&self) -> bool {
        self.kind == GraphicsKind::Text
    }

    pub fn is_planar(&self) -> bool {
        self.kind == GraphicsKind::Planar
    }

    // ------------------------------------------------------------------
    // Text mode
    // ------------------------------------------------------------------

    pub fn write_cell(&mut self, mem: &mut Memory, row: usize, column: usize, ch: u8, attr: u8) {
        if row < self.rows && column < self.columns {
            let offset = self.cell_offset(row, column);
            mem.write8(offset, ch);
            mem.write8(offset.wrapping_add(1), attr);
            self.dirty = true;
        }
    }

    pub fn read_cell(&self, mem: &Memory, row: usize, column: usize) -> (u8, u8) {
        if row < self.rows && column < self.columns {
            let offset = self.cell_offset(row, column);
            (mem.read8(offset), mem.read8(offset.wrapping_add(1)))
        } else {
            (b' ', 0x07)
        }
    }

    pub fn clear(&mut self, mem: &mut Memory, attr: u8) {
        let mut row = 0;
        while row < self.rows {
            let mut column = 0;
            while column < self.columns {
                self.write_cell(mem, row, column, b' ', attr);
                column += 1;
            }
            row += 1;
        }
        self.cursor = (0, 0);
        self.sync_bios(mem);
    }

    /// Sets a video mode and clears the memory that mode displays.
    pub fn set_mode(&mut self, mem: &mut Memory, mode: u8) {
        self.mode = mode;
        self.cursor = (0, 0);
        self.graphics_cursor = (0, 0);
        self.start_addr = 0;
        self.plane_mask = 0x0F;
        self.read_plane = 0;
        self.bit_mask = 0xFF;
        match mode {
            0x00 | 0x01 => {
                self.kind = GraphicsKind::Text;
                self.columns = 40;
                self.rows = 25;
                self.width = 320;
                self.height = 200;
                self.clear(mem, 0x07);
            }
            0x02 | 0x03 | 0x07 => {
                self.kind = GraphicsKind::Text;
                self.columns = 80;
                self.rows = 25;
                self.width = 640;
                self.height = 400;
                self.clear(mem, 0x07);
            }
            0x04 | 0x05 => {
                self.kind = GraphicsKind::Cga320;
                self.width = 320;
                self.height = 200;
                mem.fill(VIDEO_BASE, 0x4000, 0);
                self.cga_palette = 1;
                self.cga_intensity = false;
                self.dirty = true;
            }
            0x06 => {
                self.kind = GraphicsKind::Cga640;
                self.width = 640;
                self.height = 200;
                mem.fill(VIDEO_BASE, 0x4000, 0);
                self.cga_palette = 1;
                self.cga_intensity = false;
                self.dirty = true;
            }
            0x0D => {
                self.kind = GraphicsKind::Planar;
                self.width = 320;
                self.height = 200;
                self.clear_planes();
                self.reset_palette();
            }
            0x0E => {
                self.kind = GraphicsKind::Planar;
                self.width = 640;
                self.height = 200;
                self.clear_planes();
                self.reset_palette();
            }
            0x10 => {
                self.kind = GraphicsKind::Planar;
                self.width = 640;
                self.height = 350;
                self.clear_planes();
                self.reset_palette();
            }
            0x12 => {
                self.kind = GraphicsKind::Planar;
                self.width = 640;
                self.height = 480;
                self.clear_planes();
                self.reset_palette();
            }
            0x13 => {
                self.kind = GraphicsKind::Vga13;
                self.width = 320;
                self.height = 200;
                mem.fill(0xA0000, 0x1_0000, 0);
                self.reset_palette();
                self.dirty = true;
            }
            _ => {
                self.kind = GraphicsKind::Text;
                self.columns = 80;
                self.rows = 25;
                self.width = 640;
                self.height = 400;
                self.clear(mem, 0x07);
            }
        }
        self.sync_bios(mem);
        self.dirty = true;
    }

    fn clear_planes(&mut self) {
        for byte in self.vram.iter_mut() {
            *byte = 0;
        }
        self.dirty = true;
    }

    pub fn set_cursor(&mut self, mem: &mut Memory, row: usize, column: usize) {
        self.cursor = (
            row.min(self.rows.saturating_sub(1)),
            column.min(self.columns.saturating_sub(1)),
        );
        self.sync_bios(mem);
        self.dirty = true;
    }

    pub fn set_cursor_shape(&mut self, mem: &mut Memory, start: u8, end: u8) {
        self.cursor_start = start & 0x1F;
        self.cursor_end = end & 0x1F;
        self.cursor_enabled = self.cursor_start <= self.cursor_end;
        self.sync_bios(mem);
        self.dirty = true;
    }

    pub fn scroll_up(
        &mut self,
        mem: &mut Memory,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        lines: usize,
        attr: u8,
    ) {
        let top = top.min(self.rows.saturating_sub(1));
        let bottom = bottom.min(self.rows.saturating_sub(1));
        let left = left.min(self.columns.saturating_sub(1));
        let right = right.min(self.columns.saturating_sub(1));
        if top > bottom || left > right {
            return;
        }
        let height = bottom - top + 1;
        let lines = lines.min(height);
        for row in top..=bottom {
            for column in left..=right {
                let source_row = row + lines;
                let value = if source_row <= bottom {
                    self.read_cell(mem, source_row, column)
                } else {
                    (b' ', attr)
                };
                self.write_cell(mem, row, column, value.0, value.1);
            }
        }
        self.dirty = true;
    }

    pub fn scroll_down(
        &mut self,
        mem: &mut Memory,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        lines: usize,
        attr: u8,
    ) {
        let top = top.min(self.rows.saturating_sub(1));
        let bottom = bottom.min(self.rows.saturating_sub(1));
        let left = left.min(self.columns.saturating_sub(1));
        let right = right.min(self.columns.saturating_sub(1));
        if top > bottom || left > right {
            return;
        }
        let height = bottom - top + 1;
        let lines = lines.min(height);
        for row in (top..=bottom).rev() {
            for column in left..=right {
                let value = if row >= top + lines {
                    self.read_cell(mem, row - lines, column)
                } else {
                    (b' ', attr)
                };
                self.write_cell(mem, row, column, value.0, value.1);
            }
        }
        self.dirty = true;
    }

    /// BIOS teletype output (INT 10h function 0Eh).
    pub fn teletype(&mut self, mem: &mut Memory, ch: u8) {
        if !self.is_text() {
            self.graphics_teletype(mem, ch);
            return;
        }
        match ch {
            0x07 => self.bell = self.bell.wrapping_add(1),
            0x08 => {
                if self.cursor.1 > 0 {
                    self.cursor.1 -= 1;
                }
                self.sync_bios(mem);
            }
            0x09 => {
                let next = (self.cursor.1 + 8) & !7;
                self.cursor.1 = next.min(self.columns.saturating_sub(1));
                self.sync_bios(mem);
            }
            0x0A => {
                self.cursor.0 += 1;
                if self.cursor.0 >= self.rows {
                    self.scroll_up(mem, 0, 0, self.rows - 1, self.columns - 1, 1, 0x07);
                    self.cursor.0 = self.rows - 1;
                }
                self.sync_bios(mem);
            }
            0x0D => {
                self.cursor.1 = 0;
                self.sync_bios(mem);
            }
            _ => {
                let (row, column) = self.cursor;
                self.write_cell(mem, row, column, ch, 0x07);
                self.cursor.1 += 1;
                if self.cursor.1 >= self.columns {
                    self.cursor.1 = 0;
                    self.cursor.0 += 1;
                    if self.cursor.0 >= self.rows {
                        self.scroll_up(mem, 0, 0, self.rows - 1, self.columns - 1, 1, 0x07);
                        self.cursor.0 = self.rows - 1;
                    }
                }
                self.sync_bios(mem);
            }
        }
        self.dirty = true;
    }

    fn graphics_teletype(&mut self, mem: &mut Memory, ch: u8) {
        match ch {
            0x07 => self.bell = self.bell.wrapping_add(1),
            0x08 => {
                self.graphics_cursor.0 = self.graphics_cursor.0.saturating_sub(8);
            }
            0x0A => {
                self.graphics_cursor.1 += 8;
                if self.graphics_cursor.1 + 8 > self.height {
                    self.graphics_cursor.1 = 0;
                }
            }
            0x0D => self.graphics_cursor.0 = 0,
            _ => {
                let (x, y) = self.graphics_cursor;
                self.draw_char(mem, x, y, ch, self.graphics_color);
                self.graphics_cursor.0 += 8;
                if self.graphics_cursor.0 + 8 > self.width {
                    self.graphics_cursor.0 = 0;
                    self.graphics_cursor.1 += 8;
                    if self.graphics_cursor.1 + 8 > self.height {
                        self.graphics_cursor.1 = 0;
                    }
                }
            }
        }
        self.dirty = true;
    }

    pub fn write_str(&mut self, mem: &mut Memory, text: &[u8]) {
        for byte in text {
            self.teletype(mem, *byte);
        }
    }

    pub fn write_line(&mut self, mem: &mut Memory, text: &[u8]) {
        self.write_str(mem, text);
        self.teletype(mem, b'\r');
        self.teletype(mem, b'\n');
    }

    pub fn sync_bios(&self, mem: &mut Memory) {
        mem.write8(BDA_BASE + 0x49, self.mode);
        let columns = if self.is_text() {
            self.columns as u8
        } else if self.width <= 320 {
            40
        } else {
            80
        };
        mem.write8(BDA_BASE + 0x4A, columns);
        mem.write16(BDA_BASE + 0x4C, 0);
        mem.write16(
            BDA_BASE + 0x50,
            (self.cursor.0 * self.columns + self.cursor.1) as u16,
        );
        mem.write16(
            BDA_BASE + 0x60,
            u16::from(self.cursor_start) | (u16::from(self.cursor_end) << 8),
        );
        mem.write8(BDA_BASE + 0x62, self.page);
        mem.write8(BDA_BASE + 0x84, self.rows.saturating_sub(1) as u8);
        mem.write16(BDA_BASE + 0x85, 16);
    }

    // ------------------------------------------------------------------
    // Graphics
    // ------------------------------------------------------------------

    /// Writes a byte to planar memory through the graphics controller.
    pub fn write_plane_byte(&mut self, offset: usize, value: u8) {
        if offset >= 0x1_0000 {
            return;
        }
        for plane in 0..4 {
            if self.plane_mask & (1 << plane) == 0 {
                continue;
            }
            let mut byte = if self.enable_set_reset & (1 << plane) != 0 {
                if self.set_reset & (1 << plane) != 0 {
                    0xFF
                } else {
                    0x00
                }
            } else {
                value
            };
            if self.bit_mask != 0xFF {
                let old = self.vram[plane * 0x1_0000 + offset];
                byte = (byte & self.bit_mask) | (old & !self.bit_mask);
            }
            let index = plane * 0x1_0000 + offset;
            let old = self.vram[index];
            self.vram[index] = match self.logic_op & 3 {
                0 => byte,
                1 => byte & old,
                2 => byte | old,
                _ => byte ^ old,
            };
        }
        self.dirty = true;
    }

    pub fn read_plane_byte(&self, offset: usize) -> u8 {
        if offset >= 0x1_0000 {
            return 0xFF;
        }
        self.vram[(self.read_plane as usize & 3) * 0x1_0000 + offset]
    }

    /// Writes one pixel in the current graphics mode.
    pub fn put_pixel(&mut self, mem: &mut Memory, x: usize, y: usize, color: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        match self.kind {
            GraphicsKind::Text => {}
            GraphicsKind::Vga13 => {
                mem.write8(0xA0000 + (y * 320 + x) as u32, color);
            }
            GraphicsKind::Cga320 => {
                let bank = if y & 1 == 0 { 0 } else { 0x2000 };
                let offset = VIDEO_BASE as usize + bank + (y / 2) * 80 + x / 4;
                let shift = (3 - (x & 3)) * 2;
                let old = mem.read8(offset as u32);
                let value = (old & !(3 << shift)) | ((color & 3) << shift);
                mem.write8(offset as u32, value);
            }
            GraphicsKind::Cga640 => {
                let bank = if y & 1 == 0 { 0 } else { 0x2000 };
                let offset = VIDEO_BASE as usize + bank + (y / 2) * 80 + x / 8;
                let shift = 7 - (x & 7);
                let old = mem.read8(offset as u32);
                let value = (old & !(1 << shift)) | ((color & 1) << shift);
                mem.write8(offset as u32, value);
            }
            GraphicsKind::Planar => {
                let bytes_per_row = self.width / 8;
                let offset = y * bytes_per_row + x / 8;
                let shift = 7 - (x & 7);
                for plane in 0..4 {
                    let index = plane * 0x1_0000 + offset;
                    let bit = (color >> plane) & 1;
                    if bit != 0 {
                        self.vram[index] |= 1 << shift;
                    } else {
                        self.vram[index] &= !(1 << shift);
                    }
                }
            }
        }
        self.dirty = true;
    }

    pub fn read_pixel(&self, mem: &Memory, x: usize, y: usize) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        match self.kind {
            GraphicsKind::Text => 0,
            GraphicsKind::Vga13 => mem.read8(0xA0000 + (y * 320 + x) as u32),
            GraphicsKind::Cga320 => {
                let bank = if y & 1 == 0 { 0 } else { 0x2000 };
                let offset = VIDEO_BASE as usize + bank + (y / 2) * 80 + x / 4;
                let shift = (3 - (x & 3)) * 2;
                (mem.read8(offset as u32) >> shift) & 3
            }
            GraphicsKind::Cga640 => {
                let bank = if y & 1 == 0 { 0 } else { 0x2000 };
                let offset = VIDEO_BASE as usize + bank + (y / 2) * 80 + x / 8;
                let shift = 7 - (x & 7);
                (mem.read8(offset as u32) >> shift) & 1
            }
            GraphicsKind::Planar => {
                let bytes_per_row = self.width / 8;
                let offset = y * bytes_per_row + x / 8;
                let shift = 7 - (x & 7);
                let mut color = 0;
                for plane in 0..4 {
                    if self.vram[plane * 0x1_0000 + offset] & (1 << shift) != 0 {
                        color |= 1 << plane;
                    }
                }
                color
            }
        }
    }

    pub fn draw_char(&mut self, mem: &mut Memory, x: usize, y: usize, ch: u8, color: u8) {
        let glyph = font::glyph(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..8 {
                if bits & (1 << column) != 0 {
                    self.put_pixel(mem, x + column, y + row, color);
                }
            }
        }
    }

    /// Converts a palette index to an XRGB pixel.
    pub fn color(&self, index: u8) -> u32 {
        let (r, g, b) = match self.kind {
            GraphicsKind::Cga320 => {
                let mut value = if index == 0 {
                    0
                } else {
                    let base = match self.cga_palette {
                        0 => [2, 4, 6][(index as usize - 1).min(2)],
                        _ => [3, 5, 7][(index as usize - 1).min(2)],
                    };
                    base + if self.cga_intensity { 8 } else { 0 }
                };
                if self.cga_intensity && index == 0 {
                    value = 8;
                }
                palette16(value)
            }
            GraphicsKind::Cga640 => {
                if index == 0 {
                    palette16(0)
                } else if self.cga_intensity {
                    palette16(15)
                } else {
                    palette16(7)
                }
            }
            _ => {
                let entry = self.palette[index as usize];
                (
                    (entry[0] << 2) | (entry[0] >> 4),
                    (entry[1] << 2) | (entry[1] >> 4),
                    (entry[2] << 2) | (entry[2] >> 4),
                )
            }
        };
        ((r as u32) << 16) | ((g as u32) << 8) | b as u32
    }

    fn render_graphics(&mut self, mem: &Memory, screen: &mut [u32]) {
        for pixel in screen[..SCREEN_WIDTH * SCREEN_HEIGHT].iter_mut() {
            *pixel = 0;
        }
        let (offset_x, offset_y, scale_x, scale_y) = match (self.kind, self.width, self.height) {
            (GraphicsKind::Vga13, _, _) | (GraphicsKind::Cga320, _, _) => (0, 0, 2, 2),
            (GraphicsKind::Cga640, _, _) => (0, 0, 1, 2),
            (GraphicsKind::Planar, 320, _) => (0, 0, 2, 2),
            (GraphicsKind::Planar, 640, 200) => (0, 0, 1, 2),
            (GraphicsKind::Planar, 640, 350) => (0, 25, 1, 1),
            (GraphicsKind::Planar, 640, 480) => (0, 0, 1, 1),
            _ => (0, 0, 1, 1),
        };
        let max_y = (self.height).min((SCREEN_HEIGHT - offset_y) / scale_y);
        for y in 0..max_y {
            for x in 0..self.width {
                let index = self.read_pixel(mem, x, y);
                let color = self.color(index);
                let px = offset_x + x * scale_x;
                let py = offset_y + y * scale_y;
                for dy in 0..scale_y {
                    let row = py + dy;
                    if row >= SCREEN_HEIGHT {
                        continue;
                    }
                    for dx in 0..scale_x {
                        let column = px + dx;
                        if column < SCREEN_WIDTH {
                            screen[row * SCREEN_WIDTH + column] = color;
                        }
                    }
                }
            }
        }
        self.dirty = false;
    }

    // ------------------------------------------------------------------
    // Ports
    // ------------------------------------------------------------------

    pub fn crtc_index(&mut self, value: u8) {
        self.crtc_index = value & 0x1F;
    }

    pub fn crtc_write(&mut self, mem: &mut Memory, value: u8) {
        let index = self.crtc_index as usize;
        if index < self.crtc.len() {
            self.crtc[index] = value;
        }
        match index {
            0x0A => self.cursor_start = value & 0x1F,
            0x0B => self.cursor_end = value & 0x1F,
            0x0C => {
                self.start_addr = (self.start_addr & 0x00FF) | ((value as u16) << 8);
                self.dirty = true;
            }
            0x0D => {
                self.start_addr = (self.start_addr & 0xFF00) | value as u16;
                self.dirty = true;
            }
            0x0E => {
                let position = (self.cursor.0 * self.columns + self.cursor.1) as u16;
                let position = (position & 0x00FF) | ((value as u16) << 8);
                self.cursor.0 = (position as usize) / self.columns;
                self.cursor.1 = (position as usize) % self.columns;
                self.dirty = true;
            }
            0x0F => {
                let position = (self.cursor.0 * self.columns + self.cursor.1) as u16;
                let position = (position & 0xFF00) | value as u16;
                self.cursor.0 = (position as usize) / self.columns;
                self.cursor.1 = (position as usize) % self.columns;
                self.dirty = true;
            }
            _ => {}
        }
        self.sync_bios(mem);
    }

    pub fn crtc_read(&self) -> u8 {
        let index = self.crtc_index as usize;
        if index < self.crtc.len() {
            self.crtc[index]
        } else {
            0
        }
    }

    pub fn sequencer_write(&mut self, index: u8, value: u8) {
        match index {
            0x02 => {
                self.plane_mask = value & 0x0F;
                self.dirty = true;
            }
            _ => {}
        }
    }

    pub fn graphics_write(&mut self, index: u8, value: u8) {
        match index {
            0x00 => self.set_reset = value & 0x0F,
            0x01 => self.enable_set_reset = value & 0x0F,
            0x03 => self.logic_op = value & 0x03,
            0x04 => self.read_plane = value & 0x03,
            0x06 => self.bit_mask = value,
            0x08 => self.bit_mask = value,
            _ => {}
        }
    }

    pub fn dac_index(&mut self, value: u8) {
        self.dac_index = value;
        self.dac_component = 0;
    }

    pub fn dac_write(&mut self, value: u8) {
        self.palette[self.dac_index as usize][self.dac_component as usize] = value & 0x3F;
        self.dac_component = (self.dac_component + 1) % 3;
        if self.dac_component == 0 {
            self.dac_index = self.dac_index.wrapping_add(1);
        }
        self.dirty = true;
    }

    pub fn dac_read(&mut self) -> u8 {
        let value = self.palette[self.dac_index as usize][self.dac_component as usize];
        self.dac_component = (self.dac_component + 1) % 3;
        if self.dac_component == 0 {
            self.dac_index = self.dac_index.wrapping_add(1);
        }
        value
    }

    pub fn attribute_write(&mut self, value: u8) {
        if !self.attribute_flipflop {
            self.attribute_index = value & 0x3F;
            self.attribute_flipflop = true;
        } else {
            let index = self.attribute_index as usize;
            if index < 16 {
                let (r, g, b) = palette16((value & 0x0F) as usize);
                self.palette[index] = [r >> 2, g >> 2, b >> 2];
            }
            self.attribute_flipflop = false;
            self.dirty = true;
        }
    }

    pub fn set_cga_palette(&mut self, value: u8) {
        self.cga_palette = if value & 0x01 != 0 { 1 } else { 0 };
        self.cga_intensity = value & 0x10 != 0;
        self.dirty = true;
    }

    pub fn set_ega_register(&mut self, index: u8, value: u8) {
        let index = (index & 0x0F) as usize;
        let (r, g, b) = palette16((value & 0x0F) as usize);
        self.palette[index] = [r >> 2, g >> 2, b >> 2];
        self.dirty = true;
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    pub fn render(&mut self, mem: &Memory, screen: &mut [u32], blink_on: bool) {
        if screen.len() < SCREEN_WIDTH * SCREEN_HEIGHT {
            return;
        }
        if !self.is_text() {
            self.render_graphics(mem, screen);
            return;
        }
        self.blink = blink_on;
        let background = 0;
        for pixel in screen[..SCREEN_WIDTH * SCREEN_HEIGHT].iter_mut() {
            *pixel = background;
        }

        for row in 0..self.rows {
            for column in 0..self.columns {
                let (ch, attr) = self.read_cell(mem, row, column);
                let mut fg = palette16((attr & 0x0F) as usize);
                let mut bg = palette16(((attr >> 4) & 0x07) as usize);
                if attr & 0x80 != 0 && !blink_on {
                    fg = bg;
                }
                if self.cursor_enabled && self.cursor == (row, column) && blink_on {
                    core::mem::swap(&mut fg, &mut bg);
                }

                let fg = ((fg.0 as u32) << 16) | ((fg.1 as u32) << 8) | fg.2 as u32;
                let bg = ((bg.0 as u32) << 16) | ((bg.1 as u32) << 8) | bg.2 as u32;
                let x0 = column * 8;
                let y0 = row * 16;
                for y in 0..16 {
                    for x in 0..8 {
                        screen[(y0 + y) * SCREEN_WIDTH + x0 + x] = bg;
                    }
                }

                let glyph = font::glyph(ch);
                for (line, bits) in glyph.iter().enumerate() {
                    if *bits == 0 {
                        continue;
                    }
                    let y = y0 + line * 2;
                    let mut x = 0;
                    while x < 8 {
                        if bits & (1 << x) == 0 {
                            x += 1;
                            continue;
                        }
                        let start = x;
                        while x < 8 && bits & (1 << x) != 0 {
                            x += 1;
                        }
                        for px in start..x {
                            screen[y * SCREEN_WIDTH + x0 + px] = fg;
                            screen[(y + 1) * SCREEN_WIDTH + x0 + px] = fg;
                        }
                    }
                }
            }
        }
        self.dirty = false;
    }

    pub fn blit(&self, canvas: &mut Canvas<'_>, screen: &[u32]) {
        let width = canvas.width() as i32;
        let height = canvas.height() as i32;
        canvas.clear(feos_gfx::Color::BLACK);
        if width <= 0 || height <= 0 {
            return;
        }
        let scale_x = width / SCREEN_WIDTH as i32;
        let scale_y = height / SCREEN_HEIGHT as i32;
        let scale = scale_x.min(scale_y).max(1) as usize;
        let draw_width = (SCREEN_WIDTH * scale).min(width as usize);
        let draw_height = (SCREEN_HEIGHT * scale).min(height as usize);
        let offset_x = (width as usize - draw_width) / 2;
        let offset_y = (height as usize - draw_height) / 2;

        if scale == 1 {
            canvas.blit(
                offset_x as i32,
                offset_y as i32,
                draw_width as u32,
                draw_height as u32,
                &screen[..draw_width * draw_height],
            );
            return;
        }

        let mut row = vec![0u32; SCREEN_WIDTH * scale];
        for sy in 0..draw_height / scale {
            for sx in 0..SCREEN_WIDTH {
                let pixel = screen[sy * SCREEN_WIDTH + sx];
                for dx in 0..scale {
                    row[sx * scale + dx] = pixel;
                }
            }
            for repeat in 0..scale {
                let dy = offset_y + sy * scale + repeat;
                if dy < height as usize {
                    canvas.blit(
                        offset_x as i32,
                        dy as i32,
                        (SCREEN_WIDTH * scale) as u32,
                        1,
                        &row,
                    );
                }
            }
        }
    }

    pub fn text(&self, mem: &Memory) -> String {
        let mut out = String::new();
        if !self.is_text() {
            return out;
        }
        for row in 0..self.rows {
            let mut line = String::new();
            for column in 0..self.columns {
                let (ch, _) = self.read_cell(mem, row, column);
                line.push(if ch >= 0x20 { ch as char } else { ' ' });
            }
            while line.ends_with(' ') {
                line.pop();
            }
            out.push_str(&line);
            if row + 1 < self.rows {
                out.push('\n');
            }
        }
        out
    }
}

/// The standard 16-colour VGA/EGA palette.
pub fn palette16(index: usize) -> (u8, u8, u8) {
    const COLORS: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (0, 0, 170),
        (0, 170, 0),
        (0, 170, 170),
        (170, 0, 0),
        (170, 0, 170),
        (170, 85, 0),
        (170, 170, 170),
        (85, 85, 85),
        (85, 85, 255),
        (85, 255, 85),
        (85, 255, 255),
        (255, 85, 85),
        (255, 85, 255),
        (255, 255, 85),
        (255, 255, 255),
    ];
    COLORS[index & 0x0F]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teletype_wraps_and_scrolls() {
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.reset(&mut mem);
        for _ in 0..26 {
            vga.write_line(&mut mem, b"hello");
        }
        let text = vga.text(&mem);
        assert!(text.starts_with("hello"));
        assert!(text.contains("hello"));
    }

    #[test]
    fn crtc_cursor_round_trips() {
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.reset(&mut mem);
        vga.crtc_index(0x0E);
        vga.crtc_write(&mut mem, 0x01);
        vga.crtc_index(0x0F);
        vga.crtc_write(&mut mem, 0x40);
        assert_eq!(vga.cursor, (4, 0));
    }

    #[test]
    fn mode_13h_pixels_round_trip() {
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.set_mode(&mut mem, 0x13);
        vga.put_pixel(&mut mem, 319, 199, 42);
        assert_eq!(vga.read_pixel(&mem, 319, 199), 42);
    }

    #[test]
    fn cga_pixels_round_trip() {
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.set_mode(&mut mem, 0x04);
        vga.put_pixel(&mut mem, 319, 199, 3);
        assert_eq!(vga.read_pixel(&mem, 319, 199), 3);
    }

    #[test]
    fn planar_pixels_round_trip() {
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.set_mode(&mut mem, 0x0D);
        vga.put_pixel(&mut mem, 319, 199, 0x0A);
        assert_eq!(vga.read_pixel(&mem, 319, 199), 0x0A);
    }
}
