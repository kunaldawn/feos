//! The ROM: interrupt stubs, the BIOS data area and the non-video BIOS services.
//!
//! Every interrupt vector starts out pointing at a stub in ROM that runs a
//! callback into Rust and returns with `IRET`, so a guest can read the vector,
//! hook it and chain to it exactly as on real hardware. `INT 8` and `INT 9` have
//! their own small handlers, which is what makes the `INT 1Ch` chain and `IRQ`
//! acknowledgement happen in guest code where a program can see them.

use crate::cpu::{CpuRegs, InterruptAction, CALLBACK_MODRM, CALLBACK_OPCODE};
use crate::keyboard::{bda_peek, bda_pop, bda_push, Keyboard};
use crate::memory::{Memory, BDA_BASE, CONVENTIONAL_KB, EXTENDED_KB};

/// The segment of all the ROM code.
pub const ROM_SEGMENT: u16 = 0xF000;
const ROM_BASE: u32 = 0xF_0000;
/// Offset of the 256 four-byte callback stubs.
const STUBS_OFFSET: u16 = 0x0000;
/// Offset of the `INT 8` handler.
const INT08_OFFSET: u16 = 0x0400;
/// Offset of the `INT 9` handler.
const INT09_OFFSET: u16 = 0x0420;
/// Offset of the default handler for master PIC lines.
const IRQ_MASTER_OFFSET: u16 = 0x0440;
/// Offset of the default handler for slave PIC lines.
const IRQ_SLAVE_OFFSET: u16 = 0x0450;
/// Offset of the `INT 74h` handler that runs a mouse callback.
const INT74_OFFSET: u16 = 0x0460;
/// Offset of a far pointer to the mouse handler, in ROM.
pub const MOUSE_HANDLER_PTR: u16 = 0x0480;
/// Offset of the XMS driver's far-call entry.
pub const XMS_ENTRY_OFFSET: u16 = 0x0490;
/// Offset of a bare `IRET`.
const IRET_OFFSET: u16 = 0x04A0;
/// Offset of the system configuration table for `INT 15h AH=C0h`.
const CONFIG_OFFSET: u16 = 0x04B0;
/// The segment of the fake EMS driver, whose vector programs inspect.
pub const EMS_SEGMENT: u16 = 0xF500;
/// The callback number the XMS entry passes to the machine.
pub const CALLBACK_XMS: u8 = 0xF0;
/// The callback number for "mouse handler returned".
pub const CALLBACK_MOUSE_DONE: u8 = 0xF1;
/// The callback number for "load the mouse handler's registers".
pub const CALLBACK_MOUSE_ENTER: u8 = 0xF2;
/// The ROM's far-return callback variant, whose service must not patch flags.
pub const CALLBACK_NO_FLAGS: u8 = CALLBACK_MODRM + 1;

/// The BIOS state.
pub struct Bios {
    /// Equipment word returned by `INT 11h`.
    pub equipment: u16,
    /// Tracks the wait of `INT 15h AH=86h`, in microseconds of guest time.
    pub wait_until_us: Option<u64>,
}

impl Default for Bios {
    fn default() -> Self {
        Self::new()
    }
}

/// Assembles the far call and return around a mouse callback.
fn mouse_stub() -> [u8; 25] {
    let ptr = MOUSE_HANDLER_PTR.to_le_bytes();
    [
        0x60, // PUSHA
        0x06, // PUSH ES
        0x1E, // PUSH DS
        CALLBACK_OPCODE,
        CALLBACK_NO_FLAGS,
        CALLBACK_MOUSE_ENTER,
        0x2E, // CS:
        0xFF,
        0x1E,
        ptr[0],
        ptr[1], // CALL FAR [handler]
        CALLBACK_OPCODE,
        CALLBACK_NO_FLAGS,
        CALLBACK_MOUSE_DONE,
        0x1F, // POP DS
        0x07, // POP ES
        0x61, // POPA
        0x50, // PUSH AX
        0xB0,
        0x20, // MOV AL,20h
        0xE6,
        0xA0, // OUT A0h,AL
        0xE6,
        0x20, // OUT 20h,AL
        0x58, // POP AX... followed by IRET in the ROM image
    ]
}

impl Bios {
    pub const fn new() -> Self {
        Self {
            equipment: 0x0061,
            wait_until_us: None,
        }
    }

    pub fn reset(&mut self) {
        self.wait_until_us = None;
    }

    /// Builds the ROM and seeds the interrupt vector table and BIOS data area.
    pub fn install(&mut self, mem: &mut Memory) {
        // The callback stubs: <callback n>; IRET.
        for vector in 0..=255u32 {
            let at = ROM_BASE + STUBS_OFFSET as u32 + vector * 4;
            mem.write8(at, CALLBACK_OPCODE);
            mem.write8(at + 1, CALLBACK_MODRM);
            mem.write8(at + 2, vector as u8);
            mem.write8(at + 3, 0xCF);
        }
        // INT 8: count the tick, chain INT 1Ch, then acknowledge the PIC.
        mem.copy_from(
            ROM_BASE + INT08_OFFSET as u32,
            &[
                CALLBACK_OPCODE,
                CALLBACK_NO_FLAGS,
                0x08,
                0xCD,
                0x1C, // INT 1Ch
                0x50, // PUSH AX
                0xB0,
                0x20, // MOV AL,20h
                0xE6,
                0x20, // OUT 20h,AL
                0x58, // POP AX
                0xCF, // IRET
            ],
        );
        // INT 9: the callback reads port 60h, then acknowledge the PIC.
        mem.copy_from(
            ROM_BASE + INT09_OFFSET as u32,
            &[
                CALLBACK_OPCODE,
                CALLBACK_NO_FLAGS,
                0x09,
                0x50, // PUSH AX
                0xB0,
                0x20, // MOV AL,20h
                0xE6,
                0x20, // OUT 20h,AL
                0x58, // POP AX
                0xCF, // IRET
            ],
        );
        mem.copy_from(
            ROM_BASE + IRQ_MASTER_OFFSET as u32,
            &[0x50, 0xB0, 0x20, 0xE6, 0x20, 0x58, 0xCF],
        );
        mem.copy_from(
            ROM_BASE + IRQ_SLAVE_OFFSET as u32,
            &[0x50, 0xB0, 0x20, 0xE6, 0xA0, 0xE6, 0x20, 0x58, 0xCF],
        );
        let stub = mouse_stub();
        mem.copy_from(ROM_BASE + INT74_OFFSET as u32, &stub);
        mem.write8(ROM_BASE + INT74_OFFSET as u32 + stub.len() as u32, 0xCF);
        mem.copy_from(
            ROM_BASE + XMS_ENTRY_OFFSET as u32,
            &[CALLBACK_OPCODE, CALLBACK_NO_FLAGS, CALLBACK_XMS, 0xCB],
        );
        mem.write8(ROM_BASE + IRET_OFFSET as u32, 0xCF);
        // The configuration table for INT 15h AH=C0h: an AT-class machine.
        mem.copy_from(
            ROM_BASE + CONFIG_OFFSET as u32,
            &[8, 0, 0xFC, 0x01, 0x00, 0x70, 0x00, 0x00, 0x00, 0x00],
        );
        // The reset vector, BIOS date and model byte.
        mem.copy_from(
            0xF_FFF0,
            &[0xEA, 0x5B, 0xE0, 0x00, 0xF0, b'0', b'6', b'/', b'0', b'1', b'/', b'9', b'2', 0, 0xFC, 0],
        );

        // The EMS driver's header, which programs look for at vector 67h.
        let ems = (EMS_SEGMENT as u32) << 4;
        mem.copy_from(ems + 0x0A, b"EMMXXXX0");
        mem.copy_from(ems + 0x12, &[CALLBACK_OPCODE, CALLBACK_MODRM, 0x67, 0xCF]);

        // Seed every vector with its stub.
        for vector in 0..=255u16 {
            self.set_vector(mem, vector as u8, ROM_SEGMENT, STUBS_OFFSET + vector * 4);
        }
        self.set_vector(mem, 0x08, ROM_SEGMENT, INT08_OFFSET);
        self.set_vector(mem, 0x09, ROM_SEGMENT, INT09_OFFSET);
        for vector in [0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F] {
            self.set_vector(mem, vector, ROM_SEGMENT, IRQ_MASTER_OFFSET);
        }
        for vector in 0x70..=0x77 {
            self.set_vector(mem, vector, ROM_SEGMENT, IRQ_SLAVE_OFFSET);
        }
        self.set_vector(mem, 0x74, ROM_SEGMENT, INT74_OFFSET);
        self.set_vector(mem, 0x67, EMS_SEGMENT, 0x12);
        for vector in [0x01, 0x03, 0x04, 0x05, 0x1C, 0x28, 0x4A, 0x1B] {
            self.set_vector(mem, vector, ROM_SEGMENT, IRET_OFFSET);
        }
        // Data pointers, not code. The video BIOS sets 1Fh and 43h to its fonts.
        for vector in [0x1D, 0x1E, 0x1F, 0x41, 0x43, 0x46] {
            self.set_vector(mem, vector, 0, 0);
        }

        // The BIOS data area.
        mem.write16(BDA_BASE + 0x10, self.equipment);
        mem.write16(BDA_BASE + 0x13, CONVENTIONAL_KB);
        mem.write32(BDA_BASE + 0x6C, 0);
        mem.write8(BDA_BASE + 0x70, 0);
        mem.write8(BDA_BASE + 0x71, 0);
        mem.write16(BDA_BASE + 0x72, 0x1234);
        // No serial or parallel ports are present, so 400h-40Fh stay zero.
        crate::keyboard::bda_init(mem);
    }

    fn set_vector(&self, mem: &mut Memory, vector: u8, segment: u16, offset: u16) {
        let at = (vector as u32) * 4;
        mem.write16(at, offset);
        mem.write16(at + 2, segment);
    }

    /// The `INT 8` service: advances the tick count at `0040:006C`.
    pub fn int08(&mut self, mem: &mut Memory) {
        let mut ticks = mem.read32(BDA_BASE + 0x6C).wrapping_add(1);
        if ticks >= 0x0018_00B0 {
            ticks = 0;
            mem.write8(BDA_BASE + 0x70, 1);
        }
        mem.write32(BDA_BASE + 0x6C, ticks);
    }

    /// `INT 16h` keyboard services, over the BDA ring.
    pub fn int16(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        keyboard: &mut Keyboard,
    ) -> InterruptAction {
        let extended = regs.ah() & 0x10 != 0;
        match regs.ah() {
            0x00 | 0x10 => loop {
                let Some(word) = bda_pop(mem) else {
                    return InterruptAction::Blocked;
                };
                if let Some(word) = filter_key(word, extended) {
                    regs.set_ax(word);
                    return InterruptAction::Continue;
                }
            },
            0x01 | 0x11 => loop {
                let Some(word) = bda_peek(mem) else {
                    regs.set_zero(true);
                    return InterruptAction::Continue;
                };
                if let Some(word) = filter_key(word, extended) {
                    regs.set_ax(word);
                    regs.set_zero(false);
                    return InterruptAction::Continue;
                }
                bda_pop(mem);
            },
            0x02 | 0x12 => {
                let _ = keyboard;
                regs.set_al(mem.read8(BDA_BASE + 0x17));
                if regs.ah() == 0x12 {
                    regs.set_ah(mem.read8(BDA_BASE + 0x18));
                }
                InterruptAction::Continue
            }
            0x03 => InterruptAction::Continue,
            0x05 => {
                let full = !bda_push(mem, regs.cx());
                regs.set_al(u8::from(full));
                InterruptAction::Continue
            }
            0x09 => {
                regs.set_al(0x20);
                InterruptAction::Continue
            }
            _ => InterruptAction::Continue,
        }
    }

    /// `INT 1Ah` timer and real-time clock services.
    pub fn int1a(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, now_ms: u64) {
        match regs.ah() {
            0x00 => {
                let ticks = mem.read32(BDA_BASE + 0x6C);
                regs.set_cx((ticks >> 16) as u16);
                regs.set_dx(ticks as u16);
                regs.set_al(mem.read8(BDA_BASE + 0x70));
                mem.write8(BDA_BASE + 0x70, 0);
            }
            0x01 => {
                let ticks = ((regs.cx() as u32) << 16) | regs.dx() as u32;
                mem.write32(BDA_BASE + 0x6C, ticks);
            }
            0x02 => {
                let (hour, minute, second) = clock_time(now_ms);
                regs.set_ch(to_bcd(hour));
                regs.set_cl(to_bcd(minute));
                regs.set_dh(to_bcd(second));
                regs.set_dl(0);
                regs.set_carry(false);
            }
            0x04 => {
                let (century, year, month, day) = clock_date(now_ms);
                regs.set_ch(to_bcd(century));
                regs.set_cl(to_bcd(year));
                regs.set_dh(to_bcd(month));
                regs.set_dl(to_bcd(day));
                regs.set_carry(false);
            }
            0x03 | 0x05 | 0x06 | 0x07 => regs.set_carry(false),
            _ => regs.set_carry(true),
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

    /// `INT 15h` miscellaneous system services. The caller handles the wait and
    /// the block move, which need the machine.
    pub fn int15(&mut self, regs: &mut CpuRegs<'_>, xms_active: bool, a20: &mut bool) {
        match regs.ah() {
            0x24 => {
                match regs.al() {
                    0 => *a20 = false,
                    1 => *a20 = true,
                    2 => regs.set_al(u8::from(*a20)),
                    3 => regs.set_bx(3),
                    _ => {
                        regs.set_ah(0x86);
                        regs.set_carry(true);
                        return;
                    }
                }
                regs.set_ah(0);
                regs.set_carry(false);
            }
            0x88 => {
                // A loaded XMS driver owns extended memory.
                regs.set_ax(if xms_active {
                    0
                } else {
                    EXTENDED_KB.min(0xFFFF) as u16
                });
                regs.set_carry(false);
            }
            0xC0 => {
                regs.set_es(ROM_SEGMENT);
                regs.set_bx(CONFIG_OFFSET);
                regs.set_ah(0);
                regs.set_carry(false);
            }
            0xE8 => {
                if regs.al() == 0x01 && !xms_active {
                    let kb = EXTENDED_KB.min(15 * 1024) as u16;
                    regs.set_ax(kb);
                    regs.set_cx(kb);
                    regs.set_bx(0);
                    regs.set_dx(0);
                    regs.set_carry(false);
                } else {
                    regs.set_ah(0x86);
                    regs.set_carry(true);
                }
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
        regs.set_ah(0x80);
        regs.set_al(0x00);
    }

    /// `INT 17h` printer services: no printer.
    pub fn int17(&mut self, regs: &mut CpuRegs<'_>) {
        regs.set_ah(0x30);
    }
}

/// What `INT 16h` returns for a stored key word: the older functions hide keys
/// the 84-key keyboard didn't have.
fn filter_key(word: u16, extended: bool) -> Option<u16> {
    if extended {
        return Some(word);
    }
    if word as u8 == 0xE0 {
        return Some(word & 0xFF00);
    }
    Some(word)
}

/// A value in binary-coded decimal.
pub fn to_bcd(value: u8) -> u8 {
    ((value / 10) << 4) | (value % 10)
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
    let (year, month, day) = calendar(now_ms);
    (
        (year / 100) as u8,
        (year % 100) as u8,
        month as u8,
        day as u8,
    )
}

fn calendar(now_ms: u64) -> (u32, u32, u32) {
    let mut year = 2026u32;
    let mut month = 1u32;
    let mut days = ((now_ms / 86_400_000).min(3_000_000)) as u32;
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
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
    (year, month, days + 1)
}

/// The weekday, with 0 meaning Sunday. 2026-01-01 was a Thursday.
pub fn day_of_week(now_ms: u64) -> u8 {
    (((now_ms / 86_400_000) + 4) % 7) as u8
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_counts_from_the_epoch() {
        assert_eq!(clock_date(0), (20, 26, 1, 1));
        assert_eq!(clock_date(31 * 86_400_000), (20, 26, 2, 1));
        assert_eq!(clock_date(365 * 86_400_000), (20, 27, 1, 1));
        assert_eq!(day_of_week(0), 4);
        assert_eq!(day_of_week(3 * 86_400_000), 0);
    }

    #[test]
    fn rtc_values_are_bcd() {
        let mut mem = Memory::new();
        let mut r = [0u32; 8];
        let mut sregs = [0u16; 6];
        let mut flags = 0u32;
        r[0] = 0x0200;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        let mut bios = Bios::new();
        // 12:34:56
        bios.int1a(&mut regs, &mut mem, (12 * 3600 + 34 * 60 + 56) * 1000);
        assert_eq!(regs.ch(), 0x12);
        assert_eq!(regs.cl(), 0x34);
        assert_eq!(regs.dh(), 0x56);
    }

    #[test]
    fn stubs_cover_every_vector_with_a_callback_and_iret() {
        let mut mem = Memory::new();
        Bios::new().install(&mut mem);
        assert_eq!(mem.read16(0x21 * 4 + 2), ROM_SEGMENT);
        let at = ROM_BASE + mem.read16(0x21 * 4) as u32;
        assert_eq!(mem.read8(at), CALLBACK_OPCODE);
        assert_eq!(mem.read8(at + 1), CALLBACK_MODRM);
        assert_eq!(mem.read8(at + 2), 0x21);
        assert_eq!(mem.read8(at + 3), 0xCF);
    }
}
