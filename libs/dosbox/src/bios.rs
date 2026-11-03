//! The BIOS services a DOS program expects.
//!
//! `INT 10h` drives the text adapter, `INT 16h` reads the keyboard, `INT 1Ah`
//! reports the timer and clock, and the equipment, memory, disk, serial and
//! printer calls return honest "not present" answers instead of hanging.

use crate::cpu::{CpuRegs, InterruptAction};
use crate::keyboard::{KeyEvent, Keyboard};
use crate::memory::{Memory, BDA_BASE, CONVENTIONAL_KB};
use crate::vga::Vga;

/// The BIOS state.
pub struct Bios {
    /// Timer ticks since midnight (18.2 Hz).
    pub ticks: u32,
    /// Last millisecond at which a timer tick was emitted.
    pub last_tick_ms: u64,
    /// Equipment word returned by `INT 11h`.
    pub equipment: u16,
    /// Last scan code read from the keyboard controller.
    pub last_scan: u8,
}

impl Default for Bios {
    fn default() -> Self {
        Self::new()
    }
}

impl Bios {
    pub const fn new() -> Self {
        Self {
            ticks: 0,
            last_tick_ms: 0,
            equipment: 0x0061,
            last_scan: 0,
        }
    }

    pub fn reset(&mut self) {
        self.ticks = 0;
        self.last_tick_ms = 0;
        self.last_scan = 0;
    }

    /// Advances the 18.2 Hz timer. Returns true when a tick is due, so the
    /// machine can raise `INT 1Ch`.
    pub fn tick(&mut self, mem: &mut Memory, now_ms: u64) -> bool {
        let mut ticked = false;
        while now_ms.saturating_sub(self.last_tick_ms) >= 55 {
            self.last_tick_ms += 55;
            self.ticks = self.ticks.wrapping_add(1);
            if self.ticks >= 0x0018_0000 {
                self.ticks = 0;
            }
            mem.write32(BDA_BASE + 0x6C, self.ticks);
            ticked = true;
        }
        ticked
    }

    /// `INT 10h` video services.
    pub fn int10(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga) {
        match regs.ah() {
            0x00 => vga.set_mode(mem, regs.al()),
            0x01 => vga.set_cursor_shape(mem, regs.ch(), regs.cl()),
            0x02 => vga.set_cursor(mem, regs.dh() as usize, regs.dl() as usize),
            0x03 => {
                let (row, column) = vga.cursor;
                regs.set_dx(((row as u16) << 8) | column as u16);
                regs.set_cx(u16::from(vga.cursor_start) | (u16::from(vga.cursor_end) << 8));
            }
            0x05 => vga.page = regs.al(),
            0x06 => {
                let lines = regs.al() as usize;
                let attr = regs.bh();
                vga_scroll_up(vga, mem, regs, lines, attr);
            }
            0x07 => {
                let lines = regs.al() as usize;
                let attr = regs.bh();
                vga_scroll_down(vga, mem, regs, lines, attr);
            }
            0x08 => {
                let (row, column) = vga.cursor;
                let (ch, attr) = vga.read_cell(mem, row, column);
                regs.set_ax(((attr as u16) << 8) | ch as u16);
            }
            0x09 => {
                let ch = regs.al();
                let attr = regs.bl();
                let count = regs.cx().max(1) as usize;
                let (row, column) = vga.cursor;
                for index in 0..count {
                    let target = column + index;
                    if target < vga.columns {
                        vga.write_cell(mem, row, target, ch, attr);
                    }
                }
            }
            0x0A => {
                let ch = regs.al();
                let count = regs.cx().max(1) as usize;
                let (row, column) = vga.cursor;
                for index in 0..count {
                    let target = column + index;
                    if target < vga.columns {
                        let (_, attr) = vga.read_cell(mem, row, target);
                        vga.write_cell(mem, row, target, ch, attr);
                    }
                }
            }
            0x0B => {
                if regs.bh() == 1 {
                    vga.set_cga_palette(regs.bl());
                }
            }
            0x0C => vga.put_pixel(mem, regs.cx() as usize, regs.dx() as usize, regs.al()),
            0x0D => regs.set_al(vga.read_pixel(mem, regs.cx() as usize, regs.dx() as usize)),
            0x0E => vga.teletype(mem, regs.al()),
            0x0F => {
                regs.set_al(vga.mode);
                regs.set_ah(if vga.width <= 320 { 40 } else { 80 });
                regs.set_bh(vga.page);
                regs.set_dl(0);
            }
            0x10 => palette_function(regs, mem, vga),
            0x11 => match regs.al() {
                0x30 => {
                    regs.set_cx(16);
                    regs.set_dl(vga.rows.saturating_sub(1) as u8);
                }
                _ => {
                    regs.set_cx(16);
                    regs.set_dl(vga.rows.saturating_sub(1) as u8);
                }
            },
            0x12 => {
                if regs.bl() == 0x10 {
                    regs.set_bh(0);
                    regs.set_bl(0x08);
                }
            }
            0x13 => write_string(regs, mem, vga),
            0x1A => {
                regs.set_al(0x1A);
                regs.set_bh(0);
                regs.set_bl(0x08);
            }
            0x1B => {
                regs.set_al(0x1A);
                regs.set_bx(0);
                regs.set_cx(0);
            }
            0x1C => {}
            0x4F => {
                // VESA BIOS: report "function not supported".
                regs.set_ax(0x014F);
                regs.set_bx(0);
            }
            _ => {}
        }
    }

    /// `INT 16h` keyboard services.
    pub fn int16(&mut self, regs: &mut CpuRegs<'_>, keyboard: &mut Keyboard) -> InterruptAction {
        match regs.ah() {
            0x00 | 0x10 => {
                if let Some(event) = keyboard.pop() {
                    self.last_scan = event.scan;
                    regs.set_ax(event.word());
                    InterruptAction::Continue
                } else {
                    InterruptAction::Blocked
                }
            }
            0x01 | 0x11 => {
                if let Some(event) = keyboard.peek() {
                    self.last_scan = event.scan;
                    regs.set_ax(event.word());
                    regs.set_zero(false);
                } else {
                    regs.set_ax(0);
                    regs.set_zero(true);
                }
                InterruptAction::Continue
            }
            0x02 | 0x12 => {
                regs.set_al(keyboard.flags());
                InterruptAction::Continue
            }
            0x03 | 0x13 => InterruptAction::Continue,
            0x05 | 0x15 => {
                keyboard.push_back(KeyEvent::new(regs.al(), regs.ah()));
                InterruptAction::Continue
            }
            _ => InterruptAction::Continue,
        }
    }

    /// `INT 1Ah` timer and real-time clock services.
    pub fn int1a(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, now_ms: u64) {
        match regs.ah() {
            0x00 => {
                regs.set_cx((self.ticks >> 16) as u16);
                regs.set_dx(self.ticks as u16);
                regs.set_al(0);
            }
            0x01 => {
                self.ticks = ((regs.cx() as u32) << 16) | regs.dx() as u32;
                mem.write32(BDA_BASE + 0x6C, self.ticks);
            }
            0x02 => {
                let (hour, minute, second) = clock_time(now_ms);
                regs.set_ch(hour);
                regs.set_cl(minute);
                regs.set_dh(second);
                regs.set_dl(0);
                regs.set_carry(false);
            }
            0x04 => {
                let (century, year, month, day) = clock_date(now_ms);
                regs.set_ch(century);
                regs.set_cl(year);
                regs.set_dh(month);
                regs.set_dl(day);
                regs.set_carry(false);
            }
            0x0A => {
                let days = now_ms / 86_400_000;
                regs.set_cx(days as u16);
                regs.set_dx(0);
            }
            _ => {}
        }
    }

    /// `INT 11h` equipment list.
    pub fn int11(&mut self, regs: &mut CpuRegs<'_>) {
        regs.set_ax(self.equipment);
    }

    /// `INT 12h` conventional memory size.
    pub fn int12(&mut self, regs: &mut CpuRegs<'_>) {
        regs.set_ax(CONVENTIONAL_KB);
    }

    /// `INT 15h` miscellaneous system services.
    pub fn int15(&mut self, regs: &mut CpuRegs<'_>) {
        match regs.ah() {
            0x24 => {}
            0x86 => {}
            0x88 => {
                regs.set_ax(0);
                regs.set_carry(true);
            }
            0xC0 => {
                regs.set_carry(true);
                regs.set_ah(0x86);
            }
            0xE8 => {
                regs.set_ax(CONVENTIONAL_KB);
                regs.set_bx(0);
                regs.set_cx(0);
                regs.set_dx(0);
            }
            _ => {
                regs.set_carry(true);
                regs.set_ah(0x86);
            }
        }
    }

    /// `INT 13h` disk services: no floppy or hard disk controller is emulated.
    pub fn int13(&mut self, regs: &mut CpuRegs<'_>) {
        regs.set_carry(true);
        regs.set_ah(0x01);
    }

    /// `INT 14h` serial services: no UART.
    pub fn int14(&mut self, regs: &mut CpuRegs<'_>) {
        regs.set_ah(0x00);
        regs.set_al(0x00);
    }

    /// `INT 17h` printer services: no printer.
    pub fn int17(&mut self, regs: &mut CpuRegs<'_>) {
        regs.set_ah(0x90);
    }

    /// `INT 33h` mouse services: report "not installed".
    pub fn int33(&mut self, regs: &mut CpuRegs<'_>) {
        regs.set_ax(0);
        regs.set_bx(0);
    }

    /// `INT 67h` EMS services: no expanded memory.
    pub fn int67(&mut self, regs: &mut CpuRegs<'_>) {
        regs.set_ah(0x84);
    }
}

fn vga_scroll_up(vga: &mut Vga, mem: &mut Memory, regs: &mut CpuRegs<'_>, lines: usize, attr: u8) {
    let top = regs.ch() as usize;
    let left = regs.cl() as usize;
    let bottom = regs.dh() as usize;
    let right = regs.dl() as usize;
    if lines == 0 {
        for row in top..=bottom.min(vga.rows.saturating_sub(1)) {
            for column in left..=right.min(vga.columns.saturating_sub(1)) {
                vga.write_cell(mem, row, column, b' ', attr);
            }
        }
    } else {
        vga.scroll_up(mem, top, left, bottom, right, lines, attr);
    }
}

fn vga_scroll_down(
    vga: &mut Vga,
    mem: &mut Memory,
    regs: &mut CpuRegs<'_>,
    lines: usize,
    attr: u8,
) {
    let top = regs.ch() as usize;
    let left = regs.cl() as usize;
    let bottom = regs.dh() as usize;
    let right = regs.dl() as usize;
    if lines == 0 {
        for row in top..=bottom.min(vga.rows.saturating_sub(1)) {
            for column in left..=right.min(vga.columns.saturating_sub(1)) {
                vga.write_cell(mem, row, column, b' ', attr);
            }
        }
    } else {
        vga.scroll_down(mem, top, left, bottom, right, lines, attr);
    }
}

fn write_string(regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga) {
    let mode = regs.al();
    let attr = regs.bl();
    let count = regs.cx() as usize;
    let row = regs.dh() as usize;
    let column = regs.dl() as usize;
    let source = ((regs.es() as u32) << 4).wrapping_add(regs.bp() as u32);
    let mut target_column = column;
    let mut target_row = row;
    let mut index = 0;
    while index < count {
        let ch = mem.read8(source.wrapping_add(index as u32));
        index += 1;
        let cell_attr = if mode & 0x02 != 0 {
            let value = mem.read8(source.wrapping_add(index as u32));
            index += 1;
            value
        } else {
            attr
        };
        if vga.is_text() {
            if target_column < vga.columns && target_row < vga.rows {
                vga.write_cell(mem, target_row, target_column, ch, cell_attr);
            }
        } else {
            vga.draw_char(mem, target_column * 8, target_row * 8, ch, cell_attr & 0x0F);
        }
        target_column += 1;
        let columns = if vga.is_text() {
            vga.columns
        } else {
            vga.width / 8
        };
        if target_column >= columns {
            target_column = 0;
            target_row += 1;
        }
    }
    if mode & 0x01 != 0 && vga.is_text() {
        vga.set_cursor(mem, target_row.min(vga.rows - 1), target_column);
    }
}

fn palette_function(regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga) {
    match regs.al() {
        0x00 => vga.set_ega_register(regs.bl(), regs.bh()),
        0x01 => {}
        0x02 => {
            let pointer = ((regs.es() as u32) << 4).wrapping_add(regs.dx() as u32);
            for index in 0..16u32 {
                let value = mem.read8(pointer + index);
                vga.set_ega_register(index as u8, value);
            }
        }
        0x03 => vga.blink = regs.bl() & 0x01 == 0,
        0x07 => {
            let index = (regs.bl() & 0x0F) as usize;
            let entry = vga.palette[index];
            regs.set_bh(entry[0] & 0x3F);
        }
        0x08 => regs.set_bh(0),
        0x10 => {
            let index = regs.bx() as usize & 0xFF;
            vga.palette[index] = [regs.dh() & 0x3F, regs.ch() & 0x3F, regs.cl() & 0x3F];
            vga.dirty = true;
        }
        0x12 => {
            let count = regs.cx() as usize;
            let first = regs.dx() as usize;
            let pointer = ((regs.es() as u32) << 4).wrapping_add(regs.bp() as u32);
            for index in 0..count {
                let target = first + index;
                if target >= 256 {
                    break;
                }
                let at = pointer + (index * 3) as u32;
                vga.palette[target] = [
                    mem.read8(at) & 0x3F,
                    mem.read8(at + 1) & 0x3F,
                    mem.read8(at + 2) & 0x3F,
                ];
            }
            vga.dirty = true;
        }
        0x13 => {
            let index = regs.bx() as usize & 0xFF;
            let entry = vga.palette[index];
            regs.set_dh(entry[0]);
            regs.set_ch(entry[1]);
            regs.set_cl(entry[2]);
        }
        0x17 => {
            let count = regs.cx() as usize;
            let first = regs.dx() as usize;
            let pointer = ((regs.es() as u32) << 4).wrapping_add(regs.bp() as u32);
            for index in 0..count {
                let target = first + index;
                if target >= 16 {
                    break;
                }
                let value = mem.read8(pointer + index as u32);
                vga.set_ega_register(target as u8, value);
            }
        }
        _ => {}
    }
}

/// Returns `(hour, minute, second)` from milliseconds since the DOS epoch.
pub fn clock_time(now_ms: u64) -> (u8, u8, u8) {
    let seconds = (now_ms / 1000) % 86_400;
    (
        (seconds / 3600) as u8,
        ((seconds / 60) % 60) as u8,
        (seconds % 60) as u8,
    )
}

/// Returns `(century, year, month, day)` from milliseconds since the DOS
/// epoch (2026-01-01).
pub fn clock_date(now_ms: u64) -> (u8, u8, u8, u8) {
    let mut year = 2026u32;
    let mut month = 1u32;
    let mut day = 1u32;
    let mut days = (now_ms / 86_400_000) as u32;
    loop {
        let leap = is_leap(year);
        let year_days = if leap { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }
    loop {
        let month_days = days_in_month(year, month);
        if days < month_days {
            break;
        }
        days -= month_days;
        month += 1;
    }
    day += days;
    (
        (year / 100) as u8,
        (year % 100) as u8,
        month as u8,
        day as u8,
    )
}

fn is_leap(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 30,
    }
}
