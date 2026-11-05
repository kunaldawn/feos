//! `INT 10h`: the video BIOS.
//!
//! The services work in every mode: characters, the cursor and scrolling use
//! the same text-cell model in text and graphics modes. Everything a guest can
//! pass here is bounded before it is used, and `int10` returns the work done in
//! guest instruction units so a long service counts against the guest's time.

use crate::cpu::CpuRegs;
use crate::memory::{Memory, BDA_BASE};
use crate::vga::{
    font_block_offset, rom_ptr, GraphicsKind, Vga, ROM_ALT_FONT, ROM_FONT14, ROM_FONT16, ROM_FONT8,
    ROM_SEGMENT, ROM_STATIC, ROM_VESA_MODES, ROM_VESA_OEM, ROM_VESA_PM, ROM_VESA_WINDOW, SVGA_SIZE,
    VESA_PM_TABLE,
};

/// The cost of a mode set.
const MODE_SET_COST: u32 = 20_000;
/// The cost of the smallest service.
const BASE_COST: u32 = 30;
/// Work units for loading a whole ROM font.
const FONT_LOAD_WORK: u32 = 4096;
/// VBE "function failed" in AX.
const VBE_FAILED: u16 = 0x014F;
/// VBE "success" in AX.
const VBE_OK: u16 = 0x004F;

/// Physical address of a real-mode pointer.
fn linear(segment: u16, offset: u16) -> u32 {
    ((segment as u32) << 4).wrapping_add(offset as u32)
}

impl Vga {
    /// `INT 10h`. Returns the service cost in guest instruction units.
    pub fn int10(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) -> u32 {
        self.work = 0;
        let mut cost = BASE_COST;
        match regs.ah() {
            0x00 => {
                if self.set_mode(mem, regs.al()) {
                    cost = MODE_SET_COST;
                    let mode = regs.al() & 0x7F;
                    regs.set_al(match mode {
                        6 => 0x3F,
                        0..=7 => 0x30,
                        _ => 0x20,
                    });
                }
            }
            0x01 => self.set_cursor_shape(mem, regs.ch(), regs.cl()),
            0x02 => {
                let page = regs.bh() & 7;
                self.set_page_cursor(mem, page, regs.dh() as usize, regs.dl() as usize);
            }
            0x03 => {
                let (row, column) = self.cursor_of(regs.bh() & 7);
                regs.set_dx(((row as u16) << 8) | column as u16);
                regs.set_cx(((self.cursor_start as u16) << 8) | self.cursor_end as u16);
            }
            0x04 => regs.set_ah(0),
            0x05 => self.select_page(mem, regs.al()),
            0x06 | 0x07 => {
                let up = regs.ah() == 0x06;
                self.scroll_window(
                    mem,
                    self.page,
                    regs.ch() as usize,
                    regs.cl() as usize,
                    regs.dh() as usize,
                    regs.dl() as usize,
                    regs.al() as usize,
                    regs.bh(),
                    up,
                );
            }
            0x08 => {
                let page = regs.bh() & 7;
                let (row, column) = self.cursor_of(page);
                if self.is_text() {
                    let (ch, attr) = self.read_cell_on(mem, page, row, column);
                    regs.set_ax(((attr as u16) << 8) | ch as u16);
                } else {
                    let ch = self.gfx_read_char(mem, page, row, column);
                    regs.set_ax(ch as u16);
                    self.work = self.work.saturating_add(2000);
                }
            }
            0x09 | 0x0A => {
                let keep_attr = regs.ah() == 0x0A;
                self.write_repeated(
                    mem,
                    regs.bh() & 7,
                    regs.al(),
                    regs.bl(),
                    regs.cx(),
                    keep_attr,
                );
            }
            0x0B => self.background_or_palette(mem, regs.bh(), regs.bl()),
            0x0C => {
                let color = regs.al();
                let xor = color & 0x80 != 0
                    && self.kind != GraphicsKind::Vga13
                    && self.kind != GraphicsKind::Vesa;
                let color = if xor { color & 0x7F } else { color };
                self.plot(
                    mem,
                    regs.bh() & 7,
                    regs.cx() as usize,
                    regs.dx() as usize,
                    color,
                    xor,
                );
            }
            0x0D => regs.set_al(self.pixel_on(
                mem,
                regs.bh() & 7,
                regs.cx() as usize,
                regs.dx() as usize,
            )),
            0x0E => {
                let color = if self.is_text() {
                    None
                } else {
                    Some(regs.bl())
                };
                self.teletype_on(mem, self.page, regs.al(), color);
            }
            0x0F => {
                regs.set_ah(self.columns.min(255) as u8);
                regs.set_al(self.mode | if self.no_clear { 0x80 } else { 0 });
                regs.set_bh(self.page);
            }
            0x10 => self.palette_service(regs, mem),
            0x11 => self.font_service(regs, mem),
            0x12 => self.alternate_select(regs, mem),
            0x13 => self.write_string(regs, mem),
            0x1A => {
                if regs.al() <= 1 {
                    if regs.al() == 0 {
                        regs.set_bl(if self.crtc_port() == 0x3B4 {
                            0x07
                        } else {
                            0x08
                        });
                        regs.set_bh(0);
                    }
                    regs.set_al(0x1A);
                }
            }
            0x1B => {
                if regs.bx() == 0 {
                    self.functionality_state(mem, linear(regs.es(), regs.di()));
                    regs.set_al(0x1B);
                    self.work = self.work.saturating_add(64);
                }
            }
            0x4F => self.vesa_service(regs, mem),
            _ => {}
        }
        cost.saturating_add(self.work / 4)
    }

    fn select_page(&mut self, mem: &mut Memory, page: u8) {
        if page >= self.pages {
            return;
        }
        self.page = page;
        let (row, column) = self.page_cursors[page as usize & 7];
        self.cursor = (row as usize, column as usize);
        let shift = if self.is_text() { 1 } else { self.crtc_shift() };
        let start = self.page_start(page) >> shift;
        self.crtc[0x0C] = (start >> 8) as u8;
        self.crtc[0x0D] = start as u8;
        self.program_cursor_location();
        mem.write16(BDA_BASE + 0x4E, self.page_start(page) as u16);
        mem.write8(BDA_BASE + 0x62, page);
        self.dirty = true;
    }

    fn crtc_shift(&self) -> u32 {
        if self.crtc[0x14] & 0x40 != 0 {
            2
        } else if self.crtc[0x17] & 0x40 == 0 {
            1
        } else {
            0
        }
    }

    /// `AH=09h` and `AH=0Ah`. Text runs through memory; graphics stops at the
    /// end of the row, as the BIOS does.
    fn write_repeated(
        &mut self,
        mem: &mut Memory,
        page: u8,
        ch: u8,
        color: u8,
        count: u16,
        keep_attr: bool,
    ) {
        let (row, column) = self.cursor_of(page);
        let count = count as usize;
        if self.is_text() {
            let attr = if keep_attr { None } else { Some(color) };
            let rows = self.rows;
            let columns = self.columns.max(1);
            if row >= rows || column >= columns {
                return;
            }
            let room = (rows - row) * columns - column;
            let total = count.min(room);
            for index in 0..total {
                let at = column + index;
                self.write_cell_on(mem, page, row + at / columns, at % columns, ch, attr);
            }
            self.work = self.work.saturating_add(total as u32);
        } else {
            let end = (column.saturating_add(count)).min(self.columns);
            for target in column..end {
                self.gfx_char(mem, page, row, target, ch, color);
            }
        }
    }

    /// `AH=0Bh`: CGA background or palette, by way of the attribute palette.
    fn background_or_palette(&mut self, mem: &mut Memory, id: u8, value: u8) {
        let current = self.cga_palette;
        let next = match id {
            0 => (current & 0xE0) | (value & 0x1F),
            1 => {
                if value & 1 != 0 {
                    current | 0x20
                } else {
                    current & !0x20
                }
            }
            _ => return,
        };
        self.set_cga_palette(next);
        mem.write8(BDA_BASE + 0x66, next);
    }

    // ------------------------------------------------------------------
    // AH=10h
    // ------------------------------------------------------------------

    fn palette_service(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        match regs.al() {
            0x00 => {
                if regs.bl() < 0x10 {
                    self.set_ac(regs.bl() as usize, regs.bh());
                }
            }
            0x01 => self.set_ac(0x11, regs.bh()),
            0x02 => {
                let at = linear(regs.es(), regs.dx());
                for index in 0..16u32 {
                    self.set_ac(index as usize, mem.read8(at.wrapping_add(index)));
                }
                self.set_ac(0x11, mem.read8(at.wrapping_add(16)));
            }
            0x03 => {
                let control = self.ac[0x10];
                let blink = regs.bl() & 1 != 0;
                self.set_ac(
                    0x10,
                    if blink {
                        control | 0x08
                    } else {
                        control & !0x08
                    },
                );
                let mode = self.cga_mode;
                self.cga_mode = if blink { mode | 0x20 } else { mode & !0x20 };
                mem.write8(BDA_BASE + 0x65, self.cga_mode);
            }
            0x07 => {
                let value = self.ac.get(regs.bl() as usize).copied().unwrap_or(0);
                regs.set_bh(value);
            }
            0x08 => regs.set_bh(self.ac[0x11]),
            0x09 => {
                let at = linear(regs.es(), regs.dx());
                for index in 0..16u32 {
                    mem.write8(at.wrapping_add(index), self.ac[index as usize]);
                }
                mem.write8(at.wrapping_add(16), self.ac[0x11]);
            }
            0x10 => {
                if regs.bx() < 256 {
                    self.set_dac(regs.bx() as u8, regs.dh(), regs.ch(), regs.cl());
                }
            }
            0x12 => {
                let first = regs.bx() as usize;
                let at = linear(regs.es(), regs.dx());
                let count = (regs.cx() as usize).min(256usize.saturating_sub(first));
                for index in 0..count {
                    let from = at.wrapping_add((index * 3) as u32);
                    self.set_dac(
                        (first + index) as u8,
                        mem.read8(from),
                        mem.read8(from.wrapping_add(1)),
                        mem.read8(from.wrapping_add(2)),
                    );
                }
                self.work = self.work.saturating_add(count as u32 * 4);
            }
            0x13 => {
                let control = self.ac[0x10];
                if regs.bl() == 0 {
                    let on = regs.bh() & 1 != 0;
                    self.set_ac(0x10, if on { control | 0x80 } else { control & !0x80 });
                } else if regs.bl() == 1 {
                    let page = regs.bh();
                    let value = if control & 0x80 != 0 {
                        page & 0x0F
                    } else {
                        (page & 0x03) << 2
                    };
                    self.set_ac(0x14, value);
                }
            }
            0x15 => {
                if regs.bx() < 256 {
                    let entry = self.dac[regs.bx() as usize];
                    regs.set_dh(entry[0]);
                    regs.set_ch(entry[1]);
                    regs.set_cl(entry[2]);
                }
            }
            0x17 => {
                let first = regs.bx() as usize;
                let at = linear(regs.es(), regs.dx());
                let count = (regs.cx() as usize).min(256usize.saturating_sub(first));
                for index in 0..count {
                    let to = at.wrapping_add((index * 3) as u32);
                    let entry = self.dac[first + index];
                    mem.write8(to, entry[0]);
                    mem.write8(to.wrapping_add(1), entry[1]);
                    mem.write8(to.wrapping_add(2), entry[2]);
                }
                self.work = self.work.saturating_add(count as u32 * 4);
            }
            0x18 => {
                self.pel_mask = regs.bl();
                self.invalidate_luts();
            }
            0x19 => regs.set_bl(self.pel_mask),
            0x1A => {
                let control = self.ac[0x10];
                let select = self.ac[0x14];
                regs.set_bl(control >> 7);
                regs.set_bh(if control & 0x80 != 0 {
                    select & 0x0F
                } else {
                    (select >> 2) & 3
                });
            }
            0x1B => {
                let first = regs.bx() as usize;
                let count = (regs.cx() as usize).min(256usize.saturating_sub(first));
                for index in first..first + count {
                    let [red, green, blue] = self.dac[index];
                    let grey = (red as u32 * 30 + green as u32 * 59 + blue as u32 * 11 + 50) / 100;
                    let grey = grey as u8;
                    self.set_dac(index as u8, grey, grey, grey);
                }
                self.work = self.work.saturating_add(count as u32 * 4);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // AH=11h
    // ------------------------------------------------------------------

    /// Reprograms the text geometry for a font height, as the `1x` subfunctions
    /// that "activate" a font do.
    fn activate_font(&mut self, mem: &mut Memory, height: u8) {
        if !self.is_text() {
            return;
        }
        let height = height.clamp(1, 32);
        self.char_height = height;
        self.crtc[0x09] = (self.crtc[0x09] & 0xE0) | (height - 1);
        self.update_geometry();
        let cells = self.rows.saturating_mul(self.columns).saturating_mul(2);
        self.page_size = (((cells | 0xFF) + 1).min(0x8000)) as u16;
        self.program_cursor_shape();
        self.program_cursor_location();
        self.sync_bios(mem);
        self.dirty = true;
    }

    /// Writes glyphs to a character map in plane 2.
    fn load_user_font(
        &mut self,
        mem: &Memory,
        at: u32,
        first: usize,
        count: usize,
        height: usize,
        block: u8,
    ) {
        let base = font_block_offset(block);
        let count = count.min(256usize.saturating_sub(first));
        for code in 0..count {
            let from = at.wrapping_add((code * height) as u32);
            let to = base + (first + code) * 32;
            for line in 0..height.min(32) {
                self.set_plane_byte(to + line, 2, mem.read8(from.wrapping_add(line as u32)));
            }
        }
        self.work = self.work.saturating_add((count * height) as u32);
        self.dirty = true;
    }

    /// Stores the row count from a `2x` subfunction's BL (and DL).
    fn set_graphics_rows(&mut self, mem: &mut Memory, code: u8, dl: u8) {
        let rows = match code {
            0 => dl as usize,
            1 => 14,
            2 => 25,
            _ => 43,
        };
        self.rows = rows.clamp(1, 255);
        mem.write8(BDA_BASE + 0x84, (self.rows - 1) as u8);
    }

    fn font_service(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        match regs.al() {
            sub @ (0x00 | 0x10) => {
                let height = if regs.bh() == 0 { 16 } else { regs.bh() };
                let at = linear(regs.es(), regs.bp());
                self.load_user_font(
                    mem,
                    at,
                    regs.dx() as usize,
                    regs.cx() as usize,
                    height as usize,
                    regs.bl(),
                );
                if sub == 0x10 {
                    self.activate_font(mem, height);
                }
            }
            sub @ (0x01 | 0x11) => {
                self.load_rom_font(14, regs.bl());
                self.work = self.work.saturating_add(FONT_LOAD_WORK);
                if sub == 0x11 {
                    self.activate_font(mem, 14);
                }
            }
            sub @ (0x02 | 0x12) => {
                self.load_rom_font(8, regs.bl());
                self.work = self.work.saturating_add(FONT_LOAD_WORK);
                if sub == 0x12 {
                    self.activate_font(mem, 8);
                }
            }
            0x03 => {
                self.seq[0x03] = regs.bl();
                self.dirty = true;
            }
            sub @ (0x04 | 0x14) => {
                self.load_rom_font(16, regs.bl());
                self.work = self.work.saturating_add(FONT_LOAD_WORK);
                if sub == 0x14 {
                    self.activate_font(mem, 16);
                }
            }
            0x20 => {
                self.int1f = (regs.es(), regs.bp());
                self.sync_font_vectors(mem);
            }
            0x21 => {
                let height = regs.cx().clamp(1, 32) as u8;
                self.int43 = (regs.es(), regs.bp());
                self.sync_font_vectors(mem);
                if !self.is_text() {
                    self.gfx_font = Some((linear(regs.es(), regs.bp()), height));
                    self.char_height = height;
                    self.set_graphics_rows(mem, regs.bl(), regs.dl());
                    mem.write16(BDA_BASE + 0x85, height as u16);
                }
            }
            sub @ (0x22 | 0x23 | 0x24) => {
                let (address, height) = match sub {
                    0x22 => (ROM_FONT14, 14),
                    0x23 => (ROM_FONT8, 8),
                    _ => (ROM_FONT16, 16),
                };
                self.int43 = rom_ptr(address);
                self.sync_font_vectors(mem);
                if !self.is_text() {
                    self.gfx_font = None;
                    self.char_height = height;
                    self.set_graphics_rows(mem, regs.bl(), regs.dl());
                    mem.write16(BDA_BASE + 0x85, height as u16);
                }
            }
            0x30 => {
                let (segment, offset) = match regs.bh() {
                    0 => self.int1f,
                    1 => self.int43,
                    2 => rom_ptr(ROM_FONT14),
                    3 => rom_ptr(ROM_FONT8),
                    4 => rom_ptr(ROM_FONT8 + 0x400),
                    6 => rom_ptr(ROM_FONT16),
                    5 | 7 => rom_ptr(ROM_ALT_FONT),
                    _ => return,
                };
                regs.set_es(segment);
                regs.set_bp(offset);
                regs.set_cx(mem.read16(BDA_BASE + 0x85));
                regs.set_dl(self.rows.saturating_sub(1) as u8);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // AH=12h
    // ------------------------------------------------------------------

    fn alternate_select(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        match regs.bl() {
            0x10 => {
                regs.set_bh(if self.crtc_port() == 0x3B4 { 1 } else { 0 });
                regs.set_bl(0x03);
                regs.set_ch(0x00);
                regs.set_cl(0x09);
            }
            0x20 => regs.set_al(0x12),
            0x30 => {
                if regs.al() <= 2 {
                    self.scanline_select = regs.al();
                    self.sync_bios(mem);
                    regs.set_al(0x12);
                }
            }
            0x31 => {
                if regs.al() <= 1 {
                    self.default_palette_off = regs.al() == 1;
                    self.sync_bios(mem);
                    regs.set_al(0x12);
                }
            }
            0x32 | 0x33 | 0x35 | 0x36 => regs.set_al(0x12),
            0x34 => {
                if regs.al() <= 1 {
                    self.cursor_emulation = regs.al() == 0;
                    self.program_cursor_shape();
                    self.sync_bios(mem);
                    regs.set_al(0x12);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // AH=13h
    // ------------------------------------------------------------------

    fn write_string(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        let flags = regs.al();
        if flags > 3 {
            return;
        }
        let page = regs.bh() & 7;
        let attr = regs.bl();
        let count = regs.cx() as usize;
        let source = linear(regs.es(), regs.bp());
        let saved = self.cursor_of(page);
        self.set_page_cursor(mem, page, regs.dh() as usize, regs.dl() as usize);
        let with_attrs = flags & 0x02 != 0;
        let mut at = source;
        for _ in 0..count {
            let ch = mem.read8(at);
            at = at.wrapping_add(1);
            let cell = if with_attrs {
                let value = mem.read8(at);
                at = at.wrapping_add(1);
                value
            } else {
                attr
            };
            self.teletype_on(mem, page, ch, Some(cell));
        }
        self.work = self.work.saturating_add(count as u32 * 8);
        if flags & 0x01 == 0 {
            self.set_page_cursor(mem, page, saved.0, saved.1);
        }
    }

    // ------------------------------------------------------------------
    // AX=1B00h
    // ------------------------------------------------------------------

    fn functionality_state(&self, mem: &mut Memory, at: u32) {
        let put8 =
            |mem: &mut Memory, offset: u32, value: u8| mem.write8(at.wrapping_add(offset), value);
        let put16 =
            |mem: &mut Memory, offset: u32, value: u16| mem.write16(at.wrapping_add(offset), value);
        for offset in 0..64 {
            put8(mem, offset, 0);
        }
        put16(mem, 0x00, (ROM_STATIC & 0xFFFF) as u16);
        put16(mem, 0x02, ROM_SEGMENT);
        put8(mem, 0x04, self.mode);
        put16(mem, 0x05, self.columns as u16);
        put16(mem, 0x07, self.page_size);
        put16(mem, 0x09, self.page_start(self.page) as u16);
        for page in 0..8u32 {
            let (row, column) = self.cursor_of(page as u8);
            put8(mem, 0x0B + page * 2, column as u8);
            put8(mem, 0x0C + page * 2, row as u8);
        }
        put8(mem, 0x1B, self.cursor_end);
        put8(mem, 0x1C, self.cursor_start);
        put8(mem, 0x1D, self.page);
        put16(mem, 0x1E, self.crtc_port());
        put8(mem, 0x20, self.cga_mode);
        put8(mem, 0x21, self.cga_palette);
        put8(mem, 0x22, self.rows.min(255) as u8);
        put16(mem, 0x23, self.char_height as u16);
        put8(
            mem,
            0x25,
            if self.crtc_port() == 0x3B4 {
                0x07
            } else {
                0x08
            },
        );
        put8(mem, 0x26, 0);
        let colors: u16 = match self.kind {
            GraphicsKind::Vga13 | GraphicsKind::Vesa => 256,
            GraphicsKind::Cga320 => 4,
            GraphicsKind::Cga640 => 2,
            _ if self.mode == 0x11 || self.mode == 0x0F || self.mode == 7 => 2,
            _ => 16,
        };
        put16(mem, 0x27, colors);
        put8(mem, 0x29, self.pages);
        put8(
            mem,
            0x2A,
            match self.display_size().1 {
                0..=200 => 0,
                201..=350 => 1,
                351..=400 => 2,
                _ => 3,
            },
        );
        let (map_b, map_a) = (self.seq[0x03] & 3, (self.seq[0x03] >> 2) & 3);
        put8(mem, 0x2B, map_b);
        put8(mem, 0x2C, map_a);
        let mut flags = 0x20;
        if self.ac[0x10] & 0x08 != 0 {
            flags |= 0x10;
        }
        put8(mem, 0x2D, flags);
        put8(mem, 0x31, 0x03);
    }

    // ------------------------------------------------------------------
    // AX=4Fxxh
    // ------------------------------------------------------------------

    fn vesa_service(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        let result = match regs.al() {
            0x00 => {
                self.vesa_info(mem, linear(regs.es(), regs.di()));
                VBE_OK
            }
            0x01 => {
                if self.vesa_mode_info(mem, regs.cx() & 0x1FF, linear(regs.es(), regs.di())) {
                    VBE_OK
                } else {
                    VBE_FAILED
                }
            }
            0x02 => {
                let request = regs.bx();
                let mode = request & 0x3FFF;
                let clear = request & 0x8000 == 0;
                let done = if request & 0x4000 != 0 {
                    false
                } else if mode < 0x100 {
                    self.set_mode(mem, mode as u8 | if clear { 0 } else { 0x80 })
                } else {
                    self.set_vesa_mode(mem, mode, clear)
                };
                if done {
                    // `int10` divides the work by four.
                    self.work = self.work.saturating_add(MODE_SET_COST.saturating_mul(4));
                    VBE_OK
                } else {
                    VBE_FAILED
                }
            }
            0x03 => {
                let mode = if self.vesa_mode != 0 {
                    self.vesa_mode
                } else {
                    self.mode as u16
                };
                regs.set_bx(mode);
                VBE_OK
            }
            0x05 => {
                let window = regs.bl();
                match regs.bh() {
                    _ if window != 0 || self.vesa_mode == 0 => VBE_FAILED,
                    0 => {
                        if (regs.dx() as usize) < SVGA_SIZE / 0x1_0000 {
                            self.vesa_bank = regs.dx();
                            VBE_OK
                        } else {
                            VBE_FAILED
                        }
                    }
                    1 => {
                        regs.set_dx(self.vesa_bank);
                        VBE_OK
                    }
                    _ => VBE_FAILED,
                }
            }
            0x06 if self.vesa_mode != 0 => {
                let lines = self.height.max(1);
                let widest = SVGA_SIZE / lines;
                match regs.bl() {
                    0 | 2 => {
                        self.vesa_pitch = (regs.cx() as usize).clamp(640, widest);
                        self.dirty = true;
                    }
                    1 => {}
                    3 => {
                        regs.set_bx(widest as u16);
                        regs.set_cx(widest as u16);
                        regs.set_dx((SVGA_SIZE / widest) as u16);
                        regs.set_ax(VBE_OK);
                        return;
                    }
                    _ => {
                        regs.set_ax(VBE_FAILED);
                        return;
                    }
                }
                regs.set_bx(self.vesa_pitch as u16);
                regs.set_cx(self.vesa_pitch as u16);
                regs.set_dx((SVGA_SIZE / self.vesa_pitch) as u16);
                VBE_OK
            }
            0x07 if self.vesa_mode != 0 => match regs.bl() {
                0 | 0x80 => {
                    self.vesa_start = (regs.dx() as usize * self.vesa_pitch + regs.cx() as usize)
                        .min(SVGA_SIZE - 1);
                    self.dirty = true;
                    VBE_OK
                }
                1 => {
                    regs.set_bh(0);
                    regs.set_cx((self.vesa_start % self.vesa_pitch) as u16);
                    regs.set_dx((self.vesa_start / self.vesa_pitch) as u16);
                    VBE_OK
                }
                _ => VBE_FAILED,
            },
            // The DAC is always 6 bits wide, which a program can ask for and see.
            0x08 => match regs.bl() {
                0 | 1 => {
                    regs.set_bh(6);
                    VBE_OK
                }
                _ => VBE_FAILED,
            },
            0x0A if regs.bl() == 0 => {
                regs.set_es(ROM_SEGMENT);
                regs.set_di((ROM_VESA_PM & 0xFFFF) as u16);
                regs.set_cx(VESA_PM_TABLE.len() as u16);
                VBE_OK
            }
            0x09 => {
                let table = ((regs.es() as u32) << 4).wrapping_add(regs.di() as u32);
                let first = regs.dx() as usize;
                let count = regs.cx() as usize;
                match regs.bl() {
                    0 | 0x80 if first + count <= 256 => {
                        for index in 0..count {
                            let at = table.wrapping_add(index as u32 * 4);
                            self.set_dac(
                                (first + index) as u8,
                                mem.read8(at.wrapping_add(2)),
                                mem.read8(at.wrapping_add(1)),
                                mem.read8(at),
                            );
                        }
                        self.work = self.work.saturating_add(count as u32 / 4);
                        VBE_OK
                    }
                    1 if first + count <= 256 => {
                        for index in 0..count {
                            let at = table.wrapping_add(index as u32 * 4);
                            let entry = self.dac[first + index];
                            mem.write8(at, entry[2]);
                            mem.write8(at.wrapping_add(1), entry[1]);
                            mem.write8(at.wrapping_add(2), entry[0]);
                            mem.write8(at.wrapping_add(3), 0);
                        }
                        VBE_OK
                    }
                    _ => VBE_FAILED,
                }
            }
            _ => VBE_FAILED,
        };
        regs.set_ax(result);
    }

    fn vesa_info(&self, mem: &mut Memory, at: u32) {
        let wants_v2 = mem.read8(at) == b'V'
            && mem.read8(at.wrapping_add(1)) == b'B'
            && mem.read8(at.wrapping_add(2)) == b'E'
            && mem.read8(at.wrapping_add(3)) == b'2';
        let size = if wants_v2 { 512 } else { 256 };
        for offset in 0..size {
            mem.write8(at.wrapping_add(offset), 0);
        }
        let put32 = |mem: &mut Memory, offset: u32, value: u32| {
            mem.write16(at.wrapping_add(offset), value as u16);
            mem.write16(at.wrapping_add(offset + 2), (value >> 16) as u16);
        };
        mem.copy_from(at, b"VESA");
        mem.write16(at.wrapping_add(0x04), 0x0200);
        let far = |address: u32| ((ROM_SEGMENT as u32) << 16) | (address & 0xFFFF);
        put32(mem, 0x06, far(ROM_VESA_OEM));
        put32(mem, 0x0A, 0);
        put32(mem, 0x0E, far(ROM_VESA_MODES));
        mem.write16(at.wrapping_add(0x12), (SVGA_SIZE / 0x1_0000) as u16);
        if wants_v2 {
            mem.write16(at.wrapping_add(0x14), 0x0100);
            put32(mem, 0x16, far(ROM_VESA_OEM));
            put32(mem, 0x1A, far(ROM_VESA_OEM));
            put32(mem, 0x1E, far(ROM_VESA_OEM));
        }
    }

    fn vesa_mode_info(&self, mem: &mut Memory, mode: u16, at: u32) -> bool {
        let height: u16 = match mode {
            0x100 => 400,
            0x101 => 480,
            _ => return false,
        };
        for offset in 0..256 {
            mem.write8(at.wrapping_add(offset), 0);
        }
        // Supported, extended information present, BIOS text output, colour,
        // graphics. Bit 7 (linear frame buffer) stays clear.
        mem.write16(at, 0x001F);
        mem.write8(at.wrapping_add(0x02), 0x07);
        mem.write8(at.wrapping_add(0x03), 0x00);
        mem.write16(at.wrapping_add(0x04), 64);
        mem.write16(at.wrapping_add(0x06), 64);
        mem.write16(at.wrapping_add(0x08), 0xA000);
        mem.write16(at.wrapping_add(0x0A), 0);
        mem.write16(at.wrapping_add(0x0C), (ROM_VESA_WINDOW & 0xFFFF) as u16);
        mem.write16(at.wrapping_add(0x0E), ROM_SEGMENT);
        mem.write16(at.wrapping_add(0x10), 640);
        mem.write16(at.wrapping_add(0x12), 640);
        mem.write16(at.wrapping_add(0x14), height);
        mem.write8(at.wrapping_add(0x16), 8);
        mem.write8(at.wrapping_add(0x17), 16);
        mem.write8(at.wrapping_add(0x18), 1);
        mem.write8(at.wrapping_add(0x19), 8);
        mem.write8(at.wrapping_add(0x1A), 1);
        mem.write8(at.wrapping_add(0x1B), 4);
        mem.write8(at.wrapping_add(0x1C), 0);
        let pages = SVGA_SIZE / (640 * height as usize);
        mem.write8(at.wrapping_add(0x1D), (pages - 1) as u8);
        mem.write8(at.wrapping_add(0x1E), 1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::{AX, BP, BX, CX, DI, DX, ES};
    use crate::memory::VIDEO_BASE;
    use crate::vga::{palette16, GraphicsKind, SCREEN_HEIGHT, SCREEN_WIDTH};

    const BUFFER_SEGMENT: u16 = 0x2000;
    const BUFFER: u32 = 0x2_0000;
    const A: u32 = 0xA_0000;

    #[derive(Default, Clone, Copy)]
    struct Call {
        ax: u16,
        bx: u16,
        cx: u16,
        dx: u16,
        bp: u16,
        di: u16,
        es: u16,
    }

    struct Out {
        ax: u16,
        bx: u16,
        cx: u16,
        dx: u16,
        bp: u16,
        es: u16,
        cost: u32,
    }

    fn int10(vga: &mut Vga, mem: &mut Memory, call: Call) -> Out {
        let mut r = [0u32; 8];
        let mut sregs = [0u16; 6];
        let mut flags = 0u32;
        r[AX] = call.ax as u32;
        r[BX] = call.bx as u32;
        r[CX] = call.cx as u32;
        r[DX] = call.dx as u32;
        r[BP] = call.bp as u32;
        r[DI] = call.di as u32;
        sregs[ES] = call.es;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        let cost = vga.int10(&mut regs, mem);
        Out {
            ax: regs.ax(),
            bx: regs.bx(),
            cx: regs.cx(),
            dx: regs.dx(),
            bp: regs.bp(),
            es: regs.es(),
            cost,
        }
    }

    fn setup(mode: u8) -> (Vga, Memory, Vec<u32>) {
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.reset(&mut mem);
        vga.install_rom(&mut mem);
        assert!(vga.set_mode(&mut mem, mode));
        (vga, mem, vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT])
    }

    fn rgb(index: usize) -> u32 {
        let (r, g, b) = palette16(index);
        ((r as u32) << 16) | ((g as u32) << 8) | b as u32
    }

    fn at(screen: &[u32], x: usize, y: usize) -> u32 {
        screen[y * SCREEN_WIDTH + x]
    }

    // ------------------------------------------------------------------
    // AH=00h, 0Fh
    // ------------------------------------------------------------------

    #[test]
    fn set_mode_reports_and_costs() {
        let (mut vga, mut mem, _) = setup(3);
        vga.mode_changed = false;
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0013,
                ..Call::default()
            },
        );
        assert_eq!(vga.mode, 0x13);
        assert_eq!(out.ax & 0xFF, 0x20);
        assert!(out.cost >= 20_000);
        assert!(vga.mode_changed);

        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0F00,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x2813);
        assert_eq!(out.bx >> 8, 0);

        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0006,
                ..Call::default()
            },
        );
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0F00,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x5006);
    }

    #[test]
    fn set_mode_edge_cases() {
        let (mut vga, mut mem, _) = setup(3);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0013,
                ..Call::default()
            },
        );
        vga.vram_write(A, 7);
        vga.mode_changed = false;
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0055,
                ..Call::default()
            },
        );
        assert_eq!(vga.mode, 0x13);
        assert_eq!(out.ax, 0x0055);
        assert!(!vga.mode_changed);
        assert!(out.cost < 1000);

        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0093,
                ..Call::default()
            },
        );
        assert!(vga.mode_changed);
        assert_eq!(vga.mode, 0x13);
        assert_eq!(vga.vram_read(A), 7);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0F00,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, 0x93);
    }

    // ------------------------------------------------------------------
    // Cursor
    // ------------------------------------------------------------------

    #[test]
    fn cursor_shape_and_position_read_back() {
        let (mut vga, mut mem, _) = setup(3);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0300,
                ..Call::default()
            },
        );
        assert_eq!(out.cx, 0x0607);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0100,
                cx: 0x0A0C,
                ..Call::default()
            },
        );
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0200,
                dx: 0x0512,
                ..Call::default()
            },
        );
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0300,
                ..Call::default()
            },
        );
        assert_eq!(out.cx, 0x0A0C);
        assert_eq!(out.dx, 0x0512);
        assert_eq!(vga.cursor, (5, 0x12));
        assert_eq!(mem.read16(BDA_BASE + 0x60), 0x0A0C);
        assert_eq!(mem.read16(BDA_BASE + 0x50), 0x0512);
    }

    #[test]
    fn cursor_2000h_hides_it() {
        let (mut vga, mut mem, mut screen) = setup(3);
        vga.render(&mem, &mut screen, true);
        assert_eq!(at(&screen, 0, 14), rgb(7));
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0100,
                cx: 0x2000,
                ..Call::default()
            },
        );
        assert_eq!(vga.crtc[0x0A] & 0x20, 0x20);
        vga.render(&mem, &mut screen, true);
        for y in 0..16 {
            assert_eq!(at(&screen, 0, y), 0);
        }
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0300,
                ..Call::default()
            },
        );
        assert_eq!(out.cx, 0x2000);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0100,
                cx: 0x0607,
                ..Call::default()
            },
        );
        vga.render(&mem, &mut screen, true);
        assert_eq!(at(&screen, 0, 14), rgb(7));
    }

    #[test]
    fn cursor_is_per_page_and_survives_off_screen_row() {
        let (mut vga, mut mem, _) = setup(3);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0200,
                bx: 0x0100,
                dx: 0x0102,
                ..Call::default()
            },
        );
        assert_eq!(vga.cursor, (0, 0));
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0300,
                bx: 0x0100,
                ..Call::default()
            },
        );
        assert_eq!(out.dx, 0x0102);
        assert_eq!(mem.read16(BDA_BASE + 0x52), 0x0102);

        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0200,
                dx: 0x1900,
                ..Call::default()
            },
        );
        assert_eq!(vga.cursor, (25, 0));
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0300,
                ..Call::default()
            },
        );
        assert_eq!(out.dx, 0x1900);
    }

    #[test]
    fn graphics_cursor_moves_with_ah02() {
        let (mut vga, mut mem, _) = setup(0x13);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0200,
                dx: 0x0203,
                ..Call::default()
            },
        );
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0E00 | b'H' as u16,
                bx: 0x000C,
                ..Call::default()
            },
        );
        assert_eq!(vga.cursor, (2, 4));
        let glyph = crate::font::rom8(b'H');
        for line in 0..8 {
            for bit in 0..8 {
                let expected = if glyph[line] & (0x80 >> bit) != 0 {
                    12
                } else {
                    0
                };
                assert_eq!(vga.read_pixel(&mem, 24 + bit, 16 + line), expected);
            }
        }
    }

    // ------------------------------------------------------------------
    // Pages
    // ------------------------------------------------------------------

    #[test]
    fn text_page_select_moves_start_address() {
        let (mut vga, mut mem, mut screen) = setup(3);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0200,
                bx: 0x0200,
                dx: 0x0304,
                ..Call::default()
            },
        );
        // Write a character on page 2 without selecting it.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0900 | 0xDB,
                bx: 0x020F,
                cx: 1,
                dx: 0,
                ..Call::default()
            },
        );
        assert_eq!(mem.read8(VIDEO_BASE + 0x2000 + (3 * 80 + 4) * 2), 0xDB);
        assert_eq!(mem.read8(VIDEO_BASE + (3 * 80 + 4) * 2), b' ');

        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0502,
                ..Call::default()
            },
        );
        assert_eq!(vga.page, 2);
        assert_eq!(vga.cursor, (3, 4));
        assert_eq!(vga.start_counter(), 0x1000);
        assert_eq!(mem.read16(BDA_BASE + 0x4E), 0x2000);
        assert_eq!(mem.read8(BDA_BASE + 0x62), 2);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 32, 48), rgb(15));
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0F00,
                ..Call::default()
            },
        );
        assert_eq!(out.bx >> 8, 2);

        // Out-of-range pages are ignored.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0508,
                ..Call::default()
            },
        );
        assert_eq!(vga.page, 2);
    }

    #[test]
    fn planar_pages_flip_through_bios() {
        let (mut vga, mut mem, mut screen) = setup(0x0D);
        // AH=0Ch to page 1.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0C05,
                bx: 0x0100,
                cx: 1,
                dx: 2,
                ..Call::default()
            },
        );
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0D00,
                bx: 0x0100,
                cx: 1,
                dx: 2,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, 5);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0D00,
                cx: 1,
                dx: 2,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, 0);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 2, 4), 0);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0501,
                ..Call::default()
            },
        );
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 2, 4), rgb(5));
        assert_eq!(vga.start_counter(), 0x2000);
    }

    // ------------------------------------------------------------------
    // Characters
    // ------------------------------------------------------------------

    #[test]
    fn text_read_and_write_characters() {
        let (mut vga, mut mem, _) = setup(3);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0200,
                dx: 0x0178,
                ..Call::default()
            },
        );
        assert_eq!(vga.cursor, (1, 0x78));
        vga.set_cursor(&mut mem, 1, 78);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0900 | b'x' as u16,
                bx: 0x001E,
                cx: 3,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 1, 78), (b'x', 0x1E));
        assert_eq!(vga.read_cell(&mem, 2, 0), (b'x', 0x1E));
        assert_eq!(vga.cursor, (1, 78));
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0800,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x1E00 | b'x' as u16);

        // AH=0Ah keeps the attribute.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0A00 | b'y' as u16,
                bx: 0x0007,
                cx: 1,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 1, 78), (b'y', 0x1E));

        // A huge count is limited to the page.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0900 | b'z' as u16,
                bx: 0x0007,
                cx: 0xFFFF,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 24, 79).0, b'z');
        assert_eq!(mem.read8(VIDEO_BASE + 25 * 160), b' ');
        assert!(out.cost < 2000);
    }

    fn check_graphics_char_modes(mode: u8, cell: usize) {
        let (mut vga, mut mem, _) = setup(mode);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0200,
                dx: 0x0101,
                ..Call::default()
            },
        );
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0900 | b'#' as u16,
                bx: 0x0003,
                cx: 2,
                ..Call::default()
            },
        );
        let (x, y) = (8, cell);
        let mut lit = 0;
        for line in 0..cell {
            for bit in 0..16 {
                if vga.read_pixel(&mem, x + bit, y + line) != 0 {
                    lit += 1;
                }
            }
        }
        assert!(lit > 20, "mode {mode:x}: lit {lit}");
        // Nothing went to the text buffer in VGA modes.
        if mode >= 0x0D {
            assert_eq!(mem.read16(VIDEO_BASE), 0x0720);
        }
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0800,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, b'#' as u16, "mode {mode:x}");
        assert_eq!(vga.cursor, (1, 1));
    }

    #[test]
    fn graphics_characters_in_every_mode() {
        for (mode, cell) in [
            (4, 8),
            (6, 8),
            (0x0D, 8),
            (0x0E, 8),
            (0x10, 14),
            (0x12, 16),
            (0x13, 8),
        ] {
            check_graphics_char_modes(mode, cell);
        }
    }

    #[test]
    fn graphics_character_color_and_xor() {
        let (mut vga, mut mem, _) = setup(0x12);
        let glyph = crate::font::rom16(0xDB);
        assert_eq!(glyph[0], 0xFF);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0900 | 0xDB,
                bx: 0x0005,
                cx: 1,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 0, 0), 5);
        // Bit 7 XORs.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0900 | 0xDB,
                bx: 0x0083,
                cx: 1,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 0, 0), 6);
        // AH=0Ah takes BL in graphics modes too.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0A00 | 0xDB,
                bx: 0x0002,
                cx: 1,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 0, 0), 2);
        // An opaque write clears what was there.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0900 | b' ' as u16,
                bx: 0x0007,
                cx: 1,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 0, 0), 0);
        // XOR through AH=0Ch.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0C05,
                cx: 3,
                dx: 3,
                ..Call::default()
            },
        );
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0C83,
                cx: 3,
                dx: 3,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 3, 3), 6);
    }

    #[test]
    fn teletype_uses_bl_in_graphics_and_ignores_it_in_text() {
        let (mut vga, mut mem, _) = setup(0x0D);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0E00 | 0xDB,
                bx: 0x000A,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 0, 0), 0x0A);
        assert_eq!(vga.cursor, (0, 1));
        // Wraps and scrolls with the same cursor model.
        vga.set_cursor(&mut mem, 24, 39);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0E00 | 0xDB,
                bx: 0x0004,
                ..Call::default()
            },
        );
        assert_eq!(vga.cursor, (24, 0));
        assert_eq!(vga.read_pixel(&mem, 319, 191), 4);
        assert_eq!(vga.read_pixel(&mem, 319, 199), 0);

        let (mut vga, mut mem, _) = setup(3);
        vga.write_cell(&mut mem, 0, 0, b' ', 0x1E);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0E00 | b'A' as u16,
                bx: 0x004F,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 0, 0), (b'A', 0x1E));
    }

    #[test]
    fn write_string_all_forms() {
        let (mut vga, mut mem, _) = setup(3);
        mem.copy_from(BUFFER, b"Hi\r\nyo");
        // AL=0: attribute in BL, cursor not moved.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1300,
                bx: 0x001F,
                cx: 6,
                dx: 0x0203,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.cursor, (0, 0));
        assert_eq!(vga.read_cell(&mem, 2, 3), (b'H', 0x1F));
        assert_eq!(vga.read_cell(&mem, 3, 0), (b'y', 0x1F));
        // AL=1: cursor follows.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1301,
                bx: 0x0007,
                cx: 2,
                dx: 0x0500,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.cursor, (5, 2));
        // AL=2 and 3: character and attribute pairs; BP used.
        mem.copy_from(BUFFER + 0x10, &[b'a', 0x2A, b'b', 0x3B]);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1302,
                cx: 2,
                dx: 0x0600,
                bp: 0x10,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 6, 1), (b'b', 0x3B));
        assert_eq!(vga.cursor, (5, 2));
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1303,
                cx: 2,
                dx: 0x0700,
                bp: 0x10,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 7, 0), (b'a', 0x2A));
        assert_eq!(vga.cursor, (7, 2));
        // Other pages.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1301,
                bx: 0x0107,
                cx: 1,
                dx: 0x0100,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(mem.read8(VIDEO_BASE + 0x1000 + 160), b'H');
        assert_eq!(vga.cursor, (7, 2));
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0300,
                bx: 0x0100,
                ..Call::default()
            },
        );
        assert_eq!(out.dx, 0x0101);

        // Graphics.
        let (mut vga, mut mem, _) = setup(0x13);
        mem.write8(BUFFER, 0xDB);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1301,
                bx: 0x0009,
                cx: 1,
                dx: 0x0102,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 16, 8), 9);
        assert_eq!(vga.cursor, (1, 3));
    }

    // ------------------------------------------------------------------
    // Scrolling
    // ------------------------------------------------------------------

    #[test]
    fn text_scrolling() {
        let (mut vga, mut mem, _) = setup(3);
        for row in 0..5 {
            vga.write_cell(&mut mem, row, 2, b'a' + row as u8, 0x07);
        }
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0601,
                bx: 0x1700,
                cx: 0x0002,
                dx: 0x0402,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 0, 2).0, b'b');
        assert_eq!(vga.read_cell(&mem, 3, 2).0, b'e');
        assert_eq!(vga.read_cell(&mem, 4, 2), (b' ', 0x17));
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0702,
                bx: 0x2700,
                cx: 0x0002,
                dx: 0x0402,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 2, 2).0, b'b');
        assert_eq!(vga.read_cell(&mem, 0, 2), (b' ', 0x27));
        // AL=0 clears, and out-of-range corners clamp.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0600,
                bx: 0x3000,
                cx: 0x0000,
                dx: 0xFFFF,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_cell(&mem, 24, 79), (b' ', 0x30));
        assert!(out.cost > 100);
    }

    #[test]
    fn graphics_scrolling_in_every_kind() {
        for (mode, cell) in [
            (4usize, 8usize),
            (6, 8),
            (0x0D, 8),
            (0x10, 14),
            (0x12, 16),
            (0x13, 8),
        ] {
            let (mut vga, mut mem, _) = setup(mode as u8);
            int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x0200,
                    dx: 0x0201,
                    ..Call::default()
                },
            );
            int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x0900 | 0xDB,
                    bx: 1,
                    cx: 1,
                    ..Call::default()
                },
            );
            assert_eq!(vga.read_pixel(&mem, 8, 2 * cell), 1, "mode {mode:x}");

            int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x0601,
                    bx: 0x0000,
                    cx: 0x0000,
                    dx: 0x1827,
                    ..Call::default()
                },
            );
            assert_eq!(vga.read_pixel(&mem, 8, cell), 1, "mode {mode:x}");
            assert_eq!(vga.read_pixel(&mem, 8, 2 * cell), 0, "mode {mode:x}");

            int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x0702,
                    bx: 0x0100,
                    cx: 0x0000,
                    dx: 0x1827,
                    ..Call::default()
                },
            );
            assert_eq!(vga.read_pixel(&mem, 8, 3 * cell), 1, "mode {mode:x}");
            // The new rows take the fill colour, within the window.
            assert_eq!(vga.read_pixel(&mem, 8, 0), 1, "mode {mode:x}");
            assert_eq!(vga.read_pixel(&mem, 40 * 8 - 1, 0), 1, "mode {mode:x}");
            assert_eq!(vga.read_pixel(&mem, 8, 2 * cell), 0, "mode {mode:x}");
        }
    }

    // ------------------------------------------------------------------
    // Pixels and CGA palette
    // ------------------------------------------------------------------

    #[test]
    fn pixels_in_every_graphics_mode() {
        for (mode, x, y, color) in [
            (4u16, 319u16, 199u16, 2u8),
            (5, 319, 199, 3),
            (6, 639, 199, 1),
            (0x0D, 319, 199, 9),
            (0x0E, 639, 199, 9),
            (0x10, 639, 349, 9),
            (0x12, 639, 479, 9),
            (0x13, 319, 199, 200),
        ] {
            let (mut vga, mut mem, _) = setup(mode as u8);
            let cx = x;
            let dx = y;
            let write = if mode == 0x13 {
                color as u16
            } else {
                (color & 0x7F) as u16
            };
            int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x0C00 | write,
                    cx,
                    dx,
                    ..Call::default()
                },
            );
            let out = int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x0D00,
                    cx,
                    dx,
                    ..Call::default()
                },
            );
            assert_eq!(out.ax & 0xFF, color as u16, "mode {mode:x}");
            // Outside the mode: ignored and reads 0.
            int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x0C01,
                    cx: 0xFFFF,
                    dx: 0xFFFF,
                    ..Call::default()
                },
            );
            let out = int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x0D00,
                    cx: 0xFFFF,
                    dx: 0xFFFF,
                    ..Call::default()
                },
            );
            assert_eq!(out.ax & 0xFF, 0);
        }
    }

    #[test]
    fn pixels_in_unchained_mode_go_to_the_planes() {
        let (mut vga, mut mem, _) = setup(0x13);
        vga.port_out(0x3C4, 4, &mut mem);
        vga.port_out(0x3C5, 0x06, &mut mem);
        vga.put_pixel(&mut mem, 5, 0, 77);
        assert_eq!(vga.read_pixel(&mem, 5, 0), 77);
        assert_eq!(vga.plane_byte(1, 1), 77);
    }

    #[test]
    fn cga_palette_service() {
        let (mut vga, mut mem, mut screen) = setup(4);
        mem.write8(VIDEO_BASE, 0b01_00_00_00);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0B00,
                bx: 0x0100,
                ..Call::default()
            },
        );
        vga.render(&mem, &mut screen, false);
        // Palette 0, keeping intensity: light green.
        assert_eq!(at(&screen, 0, 0), rgb(10));
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0B00,
                bx: 0x0001,
                ..Call::default()
            },
        );
        vga.render(&mem, &mut screen, false);
        // Background 1, intensity off.
        assert_eq!(at(&screen, 2, 0), rgb(1));
        assert_eq!(at(&screen, 0, 0), rgb(2));
        assert_eq!(mem.read8(BDA_BASE + 0x66), 0x01);
    }

    // ------------------------------------------------------------------
    // AH=10h
    // ------------------------------------------------------------------

    #[test]
    fn attribute_palette_services_leave_the_dac_alone() {
        let (mut vga, mut mem, mut screen) = setup(0x12);
        let dac = vga.dac;
        // AX=1000h.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1000,
                bx: 0x3F01,
                ..Call::default()
            },
        );
        assert_eq!(vga.ac[1], 0x3F);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1007,
                bx: 0x0001,
                ..Call::default()
            },
        );
        assert_eq!(out.bx >> 8, 0x3F);
        // AX=1002h with 17 bytes from ES:DX.
        let mut table = [0u8; 17];
        for (index, slot) in table.iter_mut().enumerate() {
            *slot = (16 - index) as u8;
        }
        mem.copy_from(BUFFER, &table);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1002,
                dx: 0,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.ac[0], 16);
        assert_eq!(vga.ac[15], 1);
        assert_eq!(vga.ac[0x11], 0);
        assert!(vga.dac == dac);
        // AX=1001h and 1008h.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1001,
                bx: 0x2200,
                ..Call::default()
            },
        );
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1008,
                ..Call::default()
            },
        );
        assert_eq!(out.bx >> 8, 0x22);
        // AX=1009h reads all back.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1009,
                dx: 0x100,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(mem.read8(BUFFER + 0x100), 16);
        assert_eq!(mem.read8(BUFFER + 0x100 + 16), 0x22);
        // It reaches the screen.
        vga.planes[0] = 0xFF;
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), vga.lut[15]);
    }

    #[test]
    fn blink_service() {
        let (mut vga, mut mem, mut screen) = setup(3);
        vga.write_cell(&mut mem, 0, 0, b' ', 0xC0);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1003,
                bx: 0x0000,
                ..Call::default()
            },
        );
        assert_eq!(vga.ac[0x10] & 0x08, 0);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 2), rgb(12));
        assert_eq!(mem.read8(BDA_BASE + 0x65) & 0x20, 0);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1003,
                bx: 0x0001,
                ..Call::default()
            },
        );
        assert_eq!(vga.ac[0x10] & 0x08, 0x08);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 2), rgb(4));
    }

    #[test]
    fn dac_services_use_bx_es_dx_and_cx() {
        let (mut vga, mut mem, _) = setup(0x13);
        // AX=1010h: single set.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1010,
                bx: 0x0050,
                cx: 0x0203,
                dx: 0x0100,
                ..Call::default()
            },
        );
        assert_eq!(vga.dac[0x50], [1, 2, 3]);
        // AX=1015h: single read.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1015,
                bx: 0x0050,
                ..Call::default()
            },
        );
        assert_eq!(out.dx >> 8, 1);
        assert_eq!(out.cx, 0x0203);
        // AX=1012h: block set, BX first, CX count, ES:DX source (not ES:BP).
        mem.copy_from(BUFFER + 0x40, &[10, 11, 12, 20, 21, 22]);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1012,
                bx: 0x00F0,
                cx: 2,
                dx: 0x40,
                bp: 0x7777,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.dac[0xF0], [10, 11, 12]);
        assert_eq!(vga.dac[0xF1], [20, 21, 22]);
        // AX=1017h: block read.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1017,
                bx: 0x00F0,
                cx: 2,
                dx: 0x80,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(mem.read8(BUFFER + 0x80), 10);
        assert_eq!(mem.read8(BUFFER + 0x85), 22);
        // Values are 6 bits, and the block stops at the end of the DAC.
        mem.copy_from(BUFFER, &[0xFF, 0xFF, 0xFF]);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1012,
                bx: 0x00FF,
                cx: 0xFFFF,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.dac[0xFF], [0x3F, 0x3F, 0x3F]);
        assert!(out.cost < 200);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1012,
                bx: 0xFFFF,
                cx: 0xFFFF,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert!(out.cost < 200);
        // AX=1018h, 1019h: PEL mask.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1018,
                bx: 0x000F,
                ..Call::default()
            },
        );
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1019,
                ..Call::default()
            },
        );
        assert_eq!(out.bx & 0xFF, 0x0F);
        // AX=1013h, 101Ah: colour paging.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1013,
                bx: 0x0100,
                ..Call::default()
            },
        );
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1013,
                bx: 0x0501,
                ..Call::default()
            },
        );
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x101A,
                ..Call::default()
            },
        );
        assert_eq!(out.bx, 0x0501);
        // AX=101Bh: grey summing.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1010,
                bx: 3,
                cx: 0x3F00,
                dx: 0,
                ..Call::default()
            },
        );
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x101B,
                bx: 3,
                cx: 1,
                ..Call::default()
            },
        );
        let grey = ((0x3F * 59 + 50) / 100) as u8;
        assert_eq!(vga.dac[3], [grey, grey, grey]);
    }

    // ------------------------------------------------------------------
    // AH=11h
    // ------------------------------------------------------------------

    #[test]
    fn font_pointers_leave_registers_alone_except_where_specified() {
        let (mut vga, mut mem, _) = setup(3);
        for (bh, offset) in [
            (2u16, ROM_FONT14),
            (3, ROM_FONT8),
            (4, ROM_FONT8 + 0x400),
            (6, ROM_FONT16),
        ] {
            let out = int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x1130,
                    bx: bh << 8,
                    cx: 0x1111,
                    dx: 0x2200,
                    ..Call::default()
                },
            );
            assert_eq!(out.es, ROM_SEGMENT);
            assert_eq!(out.bp, (offset & 0xFFFF) as u16);
            assert_eq!(out.cx, 16);
            assert_eq!(out.dx, 0x2218);
            assert_eq!(out.ax, 0x1130);
            assert_eq!(out.bx, bh << 8);
        }
        // The fonts are in ROM, in the BIOS layout.
        for line in 0..8 {
            assert_eq!(
                mem.read8(ROM_FONT8 + b'A' as u32 * 8 + line),
                crate::font::rom8(b'A')[line as usize]
            );
        }
        assert_eq!(mem.read8(ROM_FONT16 + 0xDB * 16), 0xFF);
        assert_eq!(mem.read8(ROM_FONT14 + 0xDB * 14), 0xFF);

        // BH=1 is the graphics font of the mode, also in the vector.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1130,
                bx: 0x0100,
                ..Call::default()
            },
        );
        assert_eq!(out.bp, (ROM_FONT16 & 0xFFFF) as u16);
        assert_eq!(mem.read16(0x43 * 4), (ROM_FONT16 & 0xFFFF) as u16);
        assert_eq!(mem.read16(0x43 * 4 + 2), ROM_SEGMENT);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1130,
                bx: 0x0000,
                ..Call::default()
            },
        );
        assert_eq!(out.bp, ((ROM_FONT8 + 0x400) & 0xFFFF) as u16);

        // Other subfunctions leave CX and DL alone.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1103,
                bx: 0,
                cx: 0x1234,
                dx: 0x5678,
                ..Call::default()
            },
        );
        assert_eq!((out.cx, out.dx), (0x1234, 0x5678));
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x11FF,
                cx: 0x1234,
                dx: 0x5678,
                ..Call::default()
            },
        );
        assert_eq!((out.cx, out.dx), (0x1234, 0x5678));
    }

    #[test]
    fn rom_font_activation_changes_text_rows() {
        let (mut vga, mut mem, mut screen) = setup(3);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1112,
                ..Call::default()
            },
        );
        assert_eq!(vga.rows, 50);
        assert_eq!(mem.read8(BDA_BASE + 0x84), 49);
        assert_eq!(mem.read16(BDA_BASE + 0x85), 8);
        assert_eq!(mem.read16(BDA_BASE + 0x4C), 0x2000);
        assert_eq!(vga.display_size(), (640, 400));
        vga.write_cell(&mut mem, 49, 79, 0xDB, 0x0F);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 639, 399), rgb(15));
        assert_eq!(at(&screen, 639, 391), 0);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1130,
                bx: 0x0300,
                ..Call::default()
            },
        );
        assert_eq!((out.cx, out.dx & 0xFF), (8, 49));

        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1111,
                ..Call::default()
            },
        );
        assert_eq!(vga.rows, 28);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1114,
                ..Call::default()
            },
        );
        assert_eq!(vga.rows, 25);
    }

    #[test]
    fn user_text_font_reaches_the_screen() {
        let (mut vga, mut mem, mut screen) = setup(3);
        mem.copy_from(BUFFER, &[0xAA; 16]);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1100,
                bx: 0x1000,
                cx: 1,
                dx: b'Q' as u16,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        vga.write_cell(&mut mem, 0, 0, b'Q', 0x0F);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), rgb(15));
        assert_eq!(at(&screen, 1, 0), 0);
        // Counts and heights from the guest are bounded.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1110,
                bx: 0xFF07,
                cx: 0xFFFF,
                dx: 0xFFFF,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert!(out.cost < 1000);
        assert_eq!(vga.char_height, 32);
        vga.render(&mem, &mut screen, false);
    }

    #[test]
    fn graphics_font_services() {
        let (mut vga, mut mem, _) = setup(0x13);
        mem.copy_from(BUFFER + b'K' as u32 * 4, &[0xFF, 0xFF, 0xFF, 0xFF]);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1121,
                bx: 0x0002,
                cx: 4,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(vga.rows, 25);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0900 | b'K' as u16,
                bx: 0x0005,
                cx: 1,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 0, 3), 5);
        assert_eq!(vga.read_pixel(&mem, 0, 4), 0);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1130,
                bx: 0x0100,
                ..Call::default()
            },
        );
        assert_eq!((out.es, out.bp, out.cx), (BUFFER_SEGMENT, 0, 4));

        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1123,
                bx: 0x0000,
                dx: 10,
                ..Call::default()
            },
        );
        assert_eq!(vga.rows, 10);
        assert_eq!(mem.read8(BDA_BASE + 0x84), 9);
        assert_eq!(vga.gfx_cell_height(), 8);
        // Text modes ignore the graphics-only row counts.
        let (mut vga, mut mem, _) = setup(3);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1123,
                bx: 0x0000,
                dx: 10,
                ..Call::default()
            },
        );
        assert_eq!(vga.rows, 25);
    }

    // ------------------------------------------------------------------
    // AH=12h, 1Ah, 1Bh
    // ------------------------------------------------------------------

    #[test]
    fn ega_info_and_alternate_select() {
        let (mut vga, mut mem, _) = setup(3);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1200,
                bx: 0x0010,
                cx: 0xFFFF,
                ..Call::default()
            },
        );
        assert_eq!(out.bx, 0x0003);
        assert_eq!(out.cx, 0x0009);
        // An unknown BL leaves the registers.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1200,
                bx: 0x00FE,
                cx: 0xABCD,
                ..Call::default()
            },
        );
        assert_eq!((out.ax, out.bx, out.cx), (0x1200, 0x00FE, 0xABCD));
        // 350 lines: the next text mode uses them, 43 rows with the 8x8 font.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1201,
                bx: 0x0030,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, 0x12);
        assert_eq!(mem.read8(BDA_BASE + 0x89) & 0x90, 0x00);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0003,
                ..Call::default()
            },
        );
        assert_eq!(vga.display_size(), (640, 350));
        assert_eq!(vga.rows, 25);
        assert_eq!(mem.read16(BDA_BASE + 0x85), 14);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1112,
                ..Call::default()
            },
        );
        assert_eq!(vga.rows, 43);
        // Default palette loading can be turned off.
        vga.set_dac(1, 9, 9, 9);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1201,
                bx: 0x0031,
                ..Call::default()
            },
        );
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0003,
                ..Call::default()
            },
        );
        assert_eq!(vga.dac[1], [9, 9, 9]);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1200,
                bx: 0x0031,
                ..Call::default()
            },
        );
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0003,
                ..Call::default()
            },
        );
        assert_eq!(vga.dac[1], [0, 0, 0x2A]);
    }

    #[test]
    fn display_combination() {
        let (mut vga, mut mem, _) = setup(3);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1A00,
                bx: 0xFFFF,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, 0x1A);
        assert_eq!(out.bx, 0x0008);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1A01,
                bx: 0x1234,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, 0x1A);
        assert_eq!(out.bx, 0x1234);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1A02,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, 0x02);
        let (mut vga, mut mem, _) = setup(7);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1A00,
                ..Call::default()
            },
        );
        assert_eq!(out.bx, 0x0007);
    }

    #[test]
    fn functionality_table() {
        let (mut vga, mut mem, _) = setup(0x12);
        vga.set_cursor(&mut mem, 3, 4);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1B00,
                es: BUFFER_SEGMENT,
                di: 0x20,
                ..Call::default()
            },
        );
        assert_eq!(out.ax & 0xFF, 0x1B);
        let at_b = BUFFER + 0x20;
        let segment = mem.read16(at_b + 2);
        let offset = mem.read16(at_b);
        assert_eq!(segment, ROM_SEGMENT);
        let table = ((segment as u32) << 4) + offset as u32;
        assert_eq!(mem.read8(table), 0xFF);
        assert_eq!(mem.read8(table + 2), 0x0F);
        assert_eq!(mem.read8(at_b + 4), 0x12);
        assert_eq!(mem.read16(at_b + 5), 80);
        assert_eq!(mem.read16(at_b + 7), 0xA000);
        assert_eq!(mem.read8(at_b + 0x0B), 4);
        assert_eq!(mem.read8(at_b + 0x0C), 3);
        assert_eq!(mem.read16(at_b + 0x1E), 0x3D4);
        assert_eq!(mem.read8(at_b + 0x22), 30);
        assert_eq!(mem.read16(at_b + 0x23), 16);
        assert_eq!(mem.read8(at_b + 0x25), 8);
        assert_eq!(mem.read16(at_b + 0x27), 16);
        assert_eq!(mem.read8(at_b + 0x2A), 3);
        assert_eq!(mem.read8(at_b + 0x31), 3);

        // BX other than 0 is not this function.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x1B00,
                bx: 1,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x1B00);
    }

    // ------------------------------------------------------------------
    // VESA
    // ------------------------------------------------------------------

    #[test]
    fn vbe_info_block() {
        let (mut vga, mut mem, _) = setup(3);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F00,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x004F);
        assert_eq!(
            &[
                mem.read8(BUFFER),
                mem.read8(BUFFER + 1),
                mem.read8(BUFFER + 2),
                mem.read8(BUFFER + 3)
            ],
            b"VESA"
        );
        assert_eq!(mem.read16(BUFFER + 4), 0x0200);
        let oem = ((mem.read16(BUFFER + 8) as u32) << 4) + mem.read16(BUFFER + 6) as u32;
        let name: Vec<u8> = (0..8).map(|index| mem.read8(oem + index)).collect();
        assert_eq!(&name, b"FEOS-DOS");
        assert_eq!(mem.read8(oem + 8), 0);
        assert_eq!(mem.read16(BUFFER + 0x12), 16);
        let list = ((mem.read16(BUFFER + 0x10) as u32) << 4) + mem.read16(BUFFER + 0x0E) as u32;
        assert_eq!(mem.read16(list), 0x100);
        assert_eq!(mem.read16(list + 2), 0x101);
        assert_eq!(mem.read16(list + 4), 0xFFFF);
        assert!(vga.svga.is_empty());

        // A "VBE2" request fills the longer block.
        mem.copy_from(BUFFER + 0x400, b"VBE2");
        mem.write8(BUFFER + 0x400 + 0x1FF, 0x55);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F00,
                es: BUFFER_SEGMENT,
                di: 0x400,
                ..Call::default()
            },
        );
        assert_eq!(mem.read8(BUFFER + 0x400), b'V');
        assert_eq!(mem.read8(BUFFER + 0x400 + 0x1FF), 0);
    }

    #[test]
    fn vbe_mode_info() {
        let (mut vga, mut mem, _) = setup(3);
        for (mode, height, pages) in [(0x100u16, 400u16, 3u8), (0x101, 480, 2)] {
            let out = int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x4F01,
                    cx: mode,
                    es: BUFFER_SEGMENT,
                    ..Call::default()
                },
            );
            assert_eq!(out.ax, 0x004F);
            assert_eq!(mem.read16(BUFFER) & 0x0001, 1);
            assert_eq!(mem.read16(BUFFER) & 0x0080, 0);
            assert_eq!(mem.read8(BUFFER + 2), 7);
            assert_eq!(mem.read16(BUFFER + 4), 64);
            assert_eq!(mem.read16(BUFFER + 6), 64);
            assert_eq!(mem.read16(BUFFER + 8), 0xA000);
            assert_eq!(mem.read16(BUFFER + 0x10), 640);
            assert_eq!(mem.read16(BUFFER + 0x12), 640);
            assert_eq!(mem.read16(BUFFER + 0x14), height);
            assert_eq!(mem.read8(BUFFER + 0x19), 8);
            assert_eq!(mem.read8(BUFFER + 0x1B), 4);
            assert_eq!(mem.read8(BUFFER + 0x1D), pages);
        }
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F01,
                cx: 0x103,
                es: BUFFER_SEGMENT,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x014F);
        assert!(vga.svga.is_empty());
    }

    #[test]
    fn vbe_banked_mode_renders() {
        let (mut vga, mut mem, mut screen) = setup(3);
        assert!(vga.svga.is_empty());
        vga.mode_changed = false;
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F02,
                bx: 0x0101,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x004F);
        assert!(out.cost >= 20_000);
        assert!(vga.mode_changed);
        assert_eq!(vga.kind, GraphicsKind::Vesa);
        assert_eq!(vga.svga.len(), SVGA_SIZE);
        assert_eq!(vga.display_size(), (640, 480));
        assert_eq!((vga.width, vga.height), (640, 480));
        assert_eq!(
            int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x4F03,
                    ..Call::default()
                }
            )
            .bx,
            0x0101
        );
        assert!(vga.is_vram(A));
        assert!(!vga.is_vram(0xB_0000));

        // (0, 480-1) is at offset 639*480-ish: bank 4 holds 262144-327679.
        let y = 479usize;
        let offset = y * 640 + 7;
        let bank = offset >> 16;
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F05,
                bx: 0,
                dx: bank as u16,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x004F);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F05,
                bx: 0x0100,
                ..Call::default()
            },
        );
        assert_eq!(out.dx, bank as u16);
        vga.vram_write(A + (offset & 0xFFFF) as u32, 15);
        assert_eq!(vga.vram_read(A + (offset & 0xFFFF) as u32), 15);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 7, 479), rgb(15));
        assert_eq!(at(&screen, 8, 479), 0);

        // Banks and windows out of range fail cleanly.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F05,
                bx: 0,
                dx: 16,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x014F);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F05,
                bx: 1,
                dx: 0,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x014F);
        assert_eq!(vga.vesa_bank, bank as u16);

        // BIOS text and pixels work.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0E00 | 0xDB,
                bx: 0x0004,
                ..Call::default()
            },
        );
        assert_eq!(vga.read_pixel(&mem, 0, 0), 4);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x0601,
                bx: 0,
                cx: 0,
                dx: 0x1D4F,
                ..Call::default()
            },
        );

        // 640x400, and back to a VGA mode.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F02,
                bx: 0x0100,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x004F);
        assert_eq!(vga.display_size(), (640, 400));
        assert_eq!(vga.read_pixel(&mem, 0, 0), 0);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F02,
                bx: 0x0013,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x004F);
        assert_eq!(vga.mode, 0x13);
        assert_eq!(vga.kind, GraphicsKind::Vga13);
        assert_eq!(
            int10(
                &mut vga,
                &mut mem,
                Call {
                    ax: 0x4F03,
                    ..Call::default()
                }
            )
            .bx,
            0x13
        );
        // Bit 15 keeps the memory.
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F02,
                bx: 0x0100,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x004F);
        vga.vram_write(A, 9);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F02,
                bx: 0x8101,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x004F);
        assert_eq!(vga.vram_read(A), 9);
    }

    #[test]
    fn vbe_logical_width_and_display_start_move_the_picture() {
        let (mut vga, mut mem, _) = setup(3);
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F02,
                bx: 0x0101,
                ..Call::default()
            },
        );
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F06,
                bx: 0,
                cx: 1024,
                ..Call::default()
            },
        );
        assert_eq!((out.ax, out.bx, out.cx), (0x004F, 1024, 1024));
        vga.put_pixel(&mut mem, 3, 2, 9);
        assert_eq!(vga.svga[2 * 1024 + 3], 9);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F07,
                bx: 0,
                cx: 3,
                dx: 2,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x004F);
        assert_eq!(vga.vesa_start, 2 * 1024 + 3);
        assert_eq!(vga.read_pixel(&mem, 0, 0), 9);
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F07,
                bx: 1,
                ..Call::default()
            },
        );
        assert_eq!((out.cx, out.dx), (3, 2));
        // A mode set goes back to the plain layout.
        int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F02,
                bx: 0x0101,
                ..Call::default()
            },
        );
        assert_eq!((vga.vesa_pitch, vga.vesa_start), (640, 0));
    }

    #[test]
    fn vbe_failures_are_clean() {
        let (mut vga, mut mem, _) = setup(3);
        // Unknown functions, unknown modes, the linear frame buffer bit.
        for call in [
            Call {
                ax: 0x4F04,
                ..Call::default()
            },
            Call {
                ax: 0x4FFF,
                ..Call::default()
            },
            Call {
                ax: 0x4F02,
                bx: 0x0103,
                ..Call::default()
            },
            Call {
                ax: 0x4F02,
                bx: 0x4101,
                ..Call::default()
            },
            Call {
                ax: 0x4F02,
                bx: 0x0055,
                ..Call::default()
            },
            Call {
                ax: 0x4F05,
                bx: 0,
                dx: 0,
                ..Call::default()
            },
        ] {
            let out = int10(&mut vga, &mut mem, call);
            assert_eq!(out.ax, 0x014F);
            assert_eq!(vga.mode, 3);
        }
        assert!(vga.svga.is_empty());
        // Failing to allocate is reported, and the mode stays.
        vga.fail_alloc = true;
        let out = int10(
            &mut vga,
            &mut mem,
            Call {
                ax: 0x4F02,
                bx: 0x0101,
                ..Call::default()
            },
        );
        assert_eq!(out.ax, 0x014F);
        assert_eq!(vga.mode, 3);
        assert_eq!(vga.kind, GraphicsKind::Text);
    }

    // ------------------------------------------------------------------
    // Fuzz
    // ------------------------------------------------------------------

    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn byte(&mut self) -> u8 {
            (self.next() >> 24) as u8
        }
        fn word(&mut self) -> u16 {
            (self.next() >> 20) as u16
        }
    }

    const PORTS: [u16; 25] = [
        0x3B4, 0x3B5, 0x3BA, 0x3C0, 0x3C1, 0x3C2, 0x3C3, 0x3C4, 0x3C5, 0x3C6, 0x3C7, 0x3C8, 0x3C9,
        0x3CA, 0x3CB, 0x3CC, 0x3CD, 0x3CE, 0x3CF, 0x3D4, 0x3D5, 0x3D8, 0x3D9, 0x3DA, 0x3D0,
    ];

    fn fuzz(seed: u64, steps: u32) {
        let mut random = Random(seed);
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.reset(&mut mem);
        vga.install_rom(&mut mem);
        let mut screen = vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT];
        for step in 0..steps {
            match random.next() % 100 {
                0..=44 => {
                    let port = PORTS[(random.next() % 25) as usize];
                    let value = random.byte();
                    vga.port_out(port, value, &mut mem);
                }
                45..=54 => {
                    let port = PORTS[(random.next() % 25) as usize];
                    let time = random.next();
                    vga.port_in(port, &mem, time);
                }
                55..=69 => {
                    let addr = 0xA_0000 + (random.next() % 0x2_0000) as u32;
                    let value = random.byte();
                    if vga.is_vram(addr) {
                        vga.vram_write(addr, value);
                    } else {
                        mem.write8(addr, value);
                    }
                }
                70..=74 => {
                    let addr = random.next() as u32;
                    vga.vram_read(addr);
                    vga.vram_write(addr, 0);
                }
                75..=94 => {
                    let ah: u16 = match random.next() % 24 {
                        pick @ 0..=19 => pick as u16,
                        20 => 0x1A,
                        21 => 0x1B,
                        22 => 0x4F,
                        _ => random.byte() as u16,
                    };
                    let mut al = random.byte() as u16;
                    if ah == 0 && random.next() % 2 == 0 {
                        const REAL: [u16; 8] = [0, 3, 4, 6, 0x0D, 0x10, 0x12, 0x13];
                        al = REAL[(random.next() % 8) as usize];
                    }
                    if ah == 0x4F && random.next() % 2 == 0 {
                        al = random.next() as u16 % 6;
                    }
                    let call = Call {
                        ax: (ah << 8) | al,
                        bx: if ah == 0x4F && al == 2 {
                            0x100 + random.word() % 3
                        } else {
                            random.word()
                        },
                        cx: random.word(),
                        dx: random.word(),
                        bp: random.word(),
                        di: random.word(),
                        es: random.word(),
                    };
                    let mut call = call;
                    // Long strings that scroll a graphics screen are slow by
                    // nature; only a few get the full count.
                    if ah == 0x13 && random.next() % 64 != 0 {
                        call.cx &= 0xFF;
                    }
                    let out = int10(&mut vga, &mut mem, call);
                    // A service's cost is always finite and sensible.
                    assert!(
                        out.cost < 100_000_000,
                        "cost {} for {:#x}",
                        out.cost,
                        call.ax
                    );
                }
                95..=97 => vga.teletype(&mut mem, random.byte()),
                _ => {
                    let x = random.word() as usize;
                    let y = random.word() as usize;
                    let color = random.byte();
                    vga.put_pixel(&mut mem, x, y, color);
                    vga.read_pixel(&mem, x, y);
                }
            }
            if step % 37 == 0 {
                // Keep the display on most of the time, so the decoders run.
                if step % 5 != 0 {
                    vga.port_in(0x3DA, &mem, 0);
                    vga.port_out(0x3C0, 0x20, &mut mem);
                    vga.port_out(0x3C4, 1, &mut mem);
                    let clocking = vga.port_in(0x3C5, &mem, 0) & !0x20;
                    vga.port_out(0x3C5, clocking, &mut mem);
                }
                vga.render(&mem, &mut screen, step & 1 == 0);
                let (width, height) = vga.display_size();
                assert!((1..=SCREEN_WIDTH).contains(&width));
                assert!((1..=SCREEN_HEIGHT).contains(&height));
            }
            if step % 997 == 0 {
                let mut state = Vec::new();
                vga.save_state(&mut state);
                let mut position = 0;
                assert!(
                    vga.load_state(&state, &mut position),
                    "own state refused at step {step}"
                );
                assert_eq!(position, state.len());
                let mut pixels = vec![0u32; 200 * 150];
                let mut canvas = feos_gfx::Canvas::new(&mut pixels, 200, 150, 200).unwrap();
                vga.blit(&mut canvas, &screen);
            }
            if step % 64 == 0 {
                let _ = vga.text(&mem);
            }
        }
    }

    #[test]
    fn fuzz_ports_memory_and_bios_never_panic() {
        for seed in 1..=6u64 {
            fuzz(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1, 8_000);
        }
    }

    /// Mode sets and resets while everything is off the rails.
    #[test]
    fn fuzz_every_mode_with_random_registers() {
        let mut random = Random(0xDEAD_BEEF_1234_5678);
        let mut screen = vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT];
        for mode in [
            0u8, 1, 2, 3, 4, 5, 6, 7, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13,
        ] {
            let mut mem = Memory::new();
            let mut vga = Vga::new();
            vga.reset(&mut mem);
            assert!(vga.set_mode(&mut mem, mode));
            vga.render(&mem, &mut screen, true);
            for _ in 0..100 {
                let port = vga.crtc_port();
                vga.port_out(port, random.byte() & 0x1F, &mut mem);
                vga.port_out(port + 1, random.byte(), &mut mem);
                vga.port_out(0x3C4, random.byte(), &mut mem);
                vga.port_out(0x3C5, random.byte(), &mut mem);
                vga.port_out(0x3CE, random.byte(), &mut mem);
                vga.port_out(0x3CF, random.byte(), &mut mem);
                vga.port_in(0x3DA, &mem, 0);
                vga.port_out(0x3C0, random.byte() | 0x20, &mut mem);
                vga.port_out(0x3C0, random.byte(), &mut mem);
                vga.render(&mem, &mut screen, true);
                vga.teletype(&mut mem, random.byte());
                vga.scroll_up(&mut mem, 0, 0, 99, 99, 1, 7);
            }
        }
    }
}
