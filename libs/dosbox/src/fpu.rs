//! The x87 floating-point unit.
//!
//! The eight 80-bit registers and their arithmetic come from `rust-dos`'s
//! `F80` type (originally based on mwemu); this module adds the x87 register
//! stack, control/status words and the D8-DF instruction encodings. The
//! common arithmetic, comparison, load/store, BCD and control instructions are
//! implemented; transcendental instructions are accepted as no-ops, which is
//! enough for programs that probe for an FPU without relying on it.

use core::cmp::Ordering;

use crate::cpu::{Bus, ModRm, Step, FLAG_CF, FLAG_PF, FLAG_ZF};
use crate::f80::F80;

const TAG_VALID: u16 = 0;
const TAG_ZERO: u16 = 1;
const TAG_SPECIAL: u16 = 2;
const TAG_EMPTY: u16 = 3;

const DEFAULT_CONTROL: u16 = 0x037F;

/// The x87 state.
#[derive(Clone)]
pub struct Fpu {
    /// Physical registers, indexed by `top + i`.
    pub st: [F80; 8],
    pub top: u8,
    pub control: u16,
    pub status: u16,
    /// Two tag bits per physical register.
    pub tags: u16,
    pub ip: u32,
    pub cs: u16,
    pub data: u32,
    pub ds: u16,
}

impl Default for Fpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Fpu {
    pub fn new() -> Self {
        Self {
            st: core::array::from_fn(|_| F80::new()),
            top: 0,
            control: DEFAULT_CONTROL,
            status: 0,
            tags: 0xFFFF,
            ip: 0,
            cs: 0,
            data: 0,
            ds: 0,
        }
    }

    pub fn init(&mut self) {
        self.top = 0;
        self.control = DEFAULT_CONTROL;
        self.status = 0;
        self.tags = 0xFFFF;
        self.ip = 0;
        self.cs = 0;
        self.data = 0;
        self.ds = 0;
    }

    fn set_tag_phys(&mut self, physical: u8, tag: u16) {
        let shift = (physical & 7) * 2;
        self.tags = (self.tags & !(3 << shift)) | ((tag & 3) << shift);
    }

    fn classify(value: F80) -> u16 {
        if value.is_zero() {
            TAG_ZERO
        } else if value.is_nan() || value.is_infinite() || value.is_denormal() {
            TAG_SPECIAL
        } else {
            TAG_VALID
        }
    }

    pub fn st(&self, index: u8) -> F80 {
        self.st[((self.top.wrapping_add(index)) & 7) as usize]
    }

    pub fn set_st(&mut self, index: u8, value: F80) {
        let physical = self.top.wrapping_add(index) & 7;
        self.st[physical as usize] = value;
        self.set_tag_phys(physical, Self::classify(value));
    }

    pub fn push(&mut self, value: F80) {
        self.top = self.top.wrapping_sub(1) & 7;
        self.st[self.top as usize] = value;
        self.set_tag_phys(self.top, Self::classify(value));
    }

    pub fn pop(&mut self) {
        let physical = self.top & 7;
        self.set_tag_phys(physical, TAG_EMPTY);
        self.st[physical as usize] = F80::new();
        self.top = self.top.wrapping_add(1) & 7;
    }

    pub fn status_word(&self) -> u16 {
        (self.status & !0x3800) | ((self.top as u16 & 7) << 11)
    }

    fn set_cc(&mut self, c0: bool, c1: bool, c2: bool, c3: bool) {
        self.status &= !(0x0100 | 0x0200 | 0x0400 | 0x4000);
        if c0 {
            self.status |= 0x0100;
        }
        if c1 {
            self.status |= 0x0200;
        }
        if c2 {
            self.status |= 0x0400;
        }
        if c3 {
            self.status |= 0x4000;
        }
    }

    fn compare(&mut self, other: F80, pop: bool) {
        let left = self.st(0);
        match left.compare(&other) {
            Some(Ordering::Greater) => self.set_cc(false, false, false, false),
            Some(Ordering::Less) => self.set_cc(true, false, false, false),
            Some(Ordering::Equal) => self.set_cc(false, false, false, true),
            None => self.set_cc(true, false, true, true),
        }
        if pop {
            self.pop();
        }
    }

    fn compare_flags(&mut self, other: F80, flags: &mut u16, pop: bool) {
        let left = self.st(0);
        *flags &= !(FLAG_CF | FLAG_PF | FLAG_ZF);
        match left.compare(&other) {
            Some(Ordering::Less) => *flags |= FLAG_CF,
            Some(Ordering::Equal) => *flags |= FLAG_ZF,
            Some(Ordering::Greater) => {}
            None => *flags |= FLAG_CF | FLAG_PF | FLAG_ZF,
        }
        if pop {
            self.pop();
        }
    }

    fn arithmetic(&mut self, op: u8, left: F80, right: F80) -> F80 {
        match op {
            0 => add(left, right),
            1 => mul(left, right),
            2 => sub(left, right),
            3 => sub(right, left),
            4 => div(left, right),
            _ => div(right, left),
        }
    }

    /// Executes one D8-DF opcode.
    pub(crate) fn execute<B: Bus>(
        &mut self,
        bus: &mut B,
        opcode: u8,
        m: &ModRm,
        flags: &mut u16,
    ) -> Step {
        let address = ((m.seg as u32) << 4).wrapping_add(m.addr as u32) & 0xF_FFFF;
        match opcode {
            0xD8 => {
                if m.is_reg {
                    let other = self.st(m.rm);
                    let result = self.arithmetic(m.reg, self.st(0), other);
                    if m.reg >= 2 && m.reg <= 3 {
                        self.compare(other, m.reg == 3);
                    } else {
                        self.set_st(0, result);
                    }
                } else {
                    let other = from_f32(read32(bus, address));
                    if m.reg == 2 || m.reg == 3 {
                        self.compare(other, m.reg == 3);
                    } else {
                        let result = self.arithmetic(m.reg, self.st(0), other);
                        self.set_st(0, result);
                    }
                }
            }
            0xD9 => {
                if m.is_reg {
                    self.d9_register(m);
                } else {
                    match m.reg {
                        0 => {
                            let value = from_f32(read32(bus, address));
                            self.push(value);
                        }
                        2 => write32(bus, address, to_f32(self.st(0))),
                        3 => {
                            write32(bus, address, to_f32(self.st(0)));
                            self.pop();
                        }
                        4 => self.load_env(bus, address),
                        5 => self.control = read16(bus, address),
                        6 => self.save_env(bus, address),
                        7 => write16(bus, address, self.control),
                        _ => {}
                    }
                }
            }
            0xDA => {
                if m.is_reg {
                    self.da_register(m, flags);
                } else {
                    let other = from_i32(read32(bus, address) as i32);
                    if m.reg == 2 || m.reg == 3 {
                        self.compare(other, m.reg == 3);
                    } else {
                        let result = self.arithmetic(m.reg, self.st(0), other);
                        self.set_st(0, result);
                    }
                }
            }
            0xDB => {
                if m.is_reg {
                    match 0xC0 | (m.reg << 3) | m.rm {
                        0xE2 => self.status &= !0x80FF,
                        0xE3 => self.init(),
                        _ => {}
                    }
                } else {
                    match m.reg {
                        0 => {
                            let value = from_i32(read32(bus, address) as i32);
                            self.push(value);
                        }
                        2 => write32(bus, address, to_i32(self.st(0), self.control)),
                        3 => {
                            write32(bus, address, to_i32(self.st(0), self.control));
                            self.pop();
                        }
                        5 => {
                            let mut bytes = [0u8; 10];
                            for (index, byte) in bytes.iter_mut().enumerate() {
                                *byte = bus.read8(address.wrapping_add(index as u32));
                            }
                            let mut value = F80::new();
                            value.set_bytes(&bytes);
                            self.push(value);
                        }
                        7 => {
                            let bytes = self.st(0).get_bytes();
                            for (index, byte) in bytes.iter().enumerate() {
                                bus.write8(address.wrapping_add(index as u32), *byte);
                            }
                            self.pop();
                        }
                        _ => {}
                    }
                }
            }
            0xDC => {
                if m.is_reg {
                    let other = self.st(m.rm);
                    let result = self.arithmetic(m.reg, other, self.st(0));
                    if m.reg == 2 || m.reg == 3 {
                        self.compare(other, m.reg == 3);
                    } else {
                        self.set_st(m.rm, result);
                    }
                } else {
                    let other = from_f64_bits(read64(bus, address));
                    if m.reg == 2 || m.reg == 3 {
                        self.compare(other, m.reg == 3);
                    } else {
                        let result = self.arithmetic(m.reg, self.st(0), other);
                        self.set_st(0, result);
                    }
                }
            }
            0xDD => {
                if m.is_reg {
                    match 0xC0 | (m.reg << 3) | m.rm {
                        0xC0..=0xC7 => {
                            let index = (m.rm & 7) as u8;
                            self.set_tag_phys((self.top.wrapping_add(index)) & 7, TAG_EMPTY);
                        }
                        0xD0..=0xD7 => {
                            let value = self.st(m.rm);
                            self.set_st(m.rm, value);
                        }
                        0xD8..=0xDF => {
                            let value = self.st(m.rm);
                            self.set_st(m.rm, value);
                            self.pop();
                        }
                        0xE0..=0xE7 => self.compare(self.st(m.rm), false),
                        0xE8..=0xEF => self.compare(self.st(m.rm), true),
                        _ => {}
                    }
                } else {
                    match m.reg {
                        0 => {
                            let value = from_f64_bits(read64(bus, address));
                            self.push(value);
                        }
                        2 => write64(bus, address, to_f64(self.st(0))),
                        3 => {
                            write64(bus, address, to_f64(self.st(0)));
                            self.pop();
                        }
                        4 => self.restore_state(bus, address),
                        6 => self.save_state(bus, address),
                        7 => write16(bus, address, self.status_word()),
                        _ => {}
                    }
                }
            }
            0xDE => {
                if m.is_reg {
                    let index = m.rm;
                    let left = self.st(index);
                    let right = self.st(0);
                    match m.reg {
                        0 => {
                            let result = add(left, right);
                            self.set_st(index, result);
                            self.pop();
                        }
                        1 => {
                            let result = mul(left, right);
                            self.set_st(index, result);
                            self.pop();
                        }
                        2 => {}
                        3 => self.compare(self.st(1), true),
                        4 => {
                            let result = sub(right, left);
                            self.set_st(index, result);
                            self.pop();
                        }
                        5 => {
                            let result = sub(left, right);
                            self.set_st(index, result);
                            self.pop();
                        }
                        6 => {
                            let result = div(right, left);
                            self.set_st(index, result);
                            self.pop();
                        }
                        7 => {
                            let result = div(left, right);
                            self.set_st(index, result);
                            self.pop();
                        }
                        _ => {}
                    }
                } else {
                    let other = from_i32(read16(bus, address) as i16 as i32);
                    if m.reg == 2 || m.reg == 3 {
                        self.compare(other, m.reg == 3);
                    } else {
                        let result = self.arithmetic(m.reg, self.st(0), other);
                        self.set_st(0, result);
                    }
                }
            }
            0xDF => {
                if m.is_reg {
                    match 0xC0 | (m.reg << 3) | m.rm {
                        0xC0..=0xC7 => {
                            let index = (m.rm & 7) as u8;
                            self.set_tag_phys((self.top.wrapping_add(index)) & 7, TAG_EMPTY);
                            self.pop();
                        }
                        0xE0 => {
                            // FNSTSW AX
                            // The caller writes AX from `status_word`.
                            // Handled by the CPU's D8-DF arm below.
                        }
                        0xE8..=0xEF => self.compare_flags(self.st(m.rm), flags, true),
                        0xF0..=0xF7 => self.compare_flags(self.st(m.rm), flags, true),
                        _ => {}
                    }
                } else {
                    match m.reg {
                        0 => {
                            let value = from_i32(read16(bus, address) as i16 as i32);
                            self.push(value);
                        }
                        2 => write16(bus, address, to_i16(self.st(0), self.control)),
                        3 => {
                            write16(bus, address, to_i16(self.st(0), self.control));
                            self.pop();
                        }
                        4 => {
                            let mut bytes = [0u8; 10];
                            for (index, byte) in bytes.iter_mut().enumerate() {
                                *byte = bus.read8(address.wrapping_add(index as u32));
                            }
                            let mut value = F80::new();
                            value.from_bcd_packed(&bytes);
                            self.push(value);
                        }
                        5 => {
                            let value = from_i64(read64(bus, address) as i64);
                            self.push(value);
                        }
                        6 => {
                            if let Some(bytes) = self.st(0).to_bcd_packed(self.control) {
                                for (index, byte) in bytes.iter().enumerate() {
                                    bus.write8(address.wrapping_add(index as u32), *byte);
                                }
                            }
                            self.pop();
                        }
                        7 => {
                            write64(bus, address, to_i64(self.st(0), self.control) as u64);
                            self.pop();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Step::Ok
    }

    fn d9_register(&mut self, m: &ModRm) {
        let byte = 0xC0 | (m.reg << 3) | m.rm;
        match byte {
            0xC0..=0xC7 => {
                let value = self.st(byte & 7);
                self.push(value);
            }
            0xC8..=0xCF => {
                let index = byte & 7;
                let first = self.st(0);
                let other = self.st(index);
                self.set_st(0, other);
                self.set_st(index, first);
            }
            0xD0 => {}
            0xD8..=0xDF => {
                let index = byte & 7;
                let value = self.st(0);
                self.set_st(index, value);
                self.pop();
            }
            0xE0 => {
                let mut value = self.st(0);
                value.neg();
                self.set_st(0, value);
            }
            0xE1 => {
                let mut value = self.st(0);
                value.set_sign(false);
                self.set_st(0, value);
            }
            0xE4 => self.compare(F80::new(), false),
            0xE5 => {
                let value = self.st(0);
                let (c3, c2, c0) = if value.is_zero() {
                    (true, false, false)
                } else if value.is_nan() || value.is_infinite() {
                    (false, false, false)
                } else if value.is_denormal() {
                    (false, true, false)
                } else {
                    (false, false, true)
                };
                self.set_cc(c0, false, c2, c3);
            }
            0xE8 => self.push(from_f64(1.0)),
            0xE9 => self.push(from_f64(core::f64::consts::LOG2_10)),
            0xEA => self.push(from_f64(core::f64::consts::LOG2_E)),
            0xEB => self.push(F80::PI()),
            0xEC => self.push(from_f64(core::f64::consts::LOG10_2)),
            0xED => self.push(from_f64(core::f64::consts::LN_2)),
            0xEE => self.push(F80::new()),
            0xF6 => self.top = self.top.wrapping_sub(1) & 7,
            0xF7 => self.top = self.top.wrapping_add(1) & 7,
            0xF8 | 0xF5 => {
                let a = self.st(0).get_f64();
                let b = self.st(1).get_f64();
                if b != 0.0 {
                    let quotient = (a / b) as i64;
                    self.set_st(0, from_f64(a - quotient as f64 * b));
                }
            }
            0xFA => {
                let value = self.st(0).get_f64();
                self.set_st(0, from_f64(sqrt(value)));
            }
            0xFC => {
                let value = self.st(0).round_int(self.control).unwrap_or(0) as f64;
                self.set_st(0, from_f64(value));
            }
            0xFD => {
                let exponent = self.st(1).get_f64() as i32;
                let value = self.st(0).get_f64() * pow2(exponent);
                self.set_st(0, from_f64(value));
            }
            _ => {}
        }
    }

    fn da_register(&mut self, m: &ModRm, flags: &u16) {
        let byte = 0xC0 | (m.reg << 3) | m.rm;
        let take = match byte & 0x38 {
            0x00 => *flags & FLAG_CF != 0,
            0x08 => *flags & FLAG_ZF != 0,
            0x10 => *flags & FLAG_CF != 0 || *flags & FLAG_ZF != 0,
            0x18 => *flags & FLAG_PF != 0,
            0x20 => *flags & FLAG_CF == 0,
            0x28 => *flags & FLAG_ZF == 0,
            0x30 => *flags & FLAG_CF == 0 && *flags & FLAG_ZF == 0,
            0x38 => *flags & FLAG_PF == 0,
            _ => false,
        };
        if take {
            let value = self.st(m.rm);
            self.set_st(0, value);
        }
    }

    fn save_env(&mut self, bus: &mut dyn Bus, address: u32) {
        write16(bus, address, self.control);
        write16(bus, address + 4, self.status_word());
        write16(bus, address + 8, self.tags);
        write32(bus, address + 12, self.ip);
        write16(bus, address + 16, self.cs);
        write32(bus, address + 20, self.data);
        write16(bus, address + 24, self.ds);
    }

    fn load_env(&mut self, bus: &mut dyn Bus, address: u32) {
        self.control = read16(bus, address);
        self.status = read16(bus, address + 4);
        self.top = ((self.status >> 11) & 7) as u8;
        self.tags = read16(bus, address + 8);
        self.ip = read32(bus, address + 12);
        self.cs = read16(bus, address + 16);
        self.data = read32(bus, address + 20);
        self.ds = read16(bus, address + 24);
    }

    fn save_state(&mut self, bus: &mut dyn Bus, address: u32) {
        self.save_env(bus, address);
        for index in 0..8 {
            let bytes = self.st[index].get_bytes();
            for (offset, byte) in bytes.iter().enumerate() {
                bus.write8(address + 28 + (index * 10 + offset) as u32, *byte);
            }
        }
        self.init();
    }

    fn restore_state(&mut self, bus: &mut dyn Bus, address: u32) {
        self.load_env(bus, address);
        for index in 0..8 {
            let mut bytes = [0u8; 10];
            for (offset, byte) in bytes.iter_mut().enumerate() {
                *byte = bus.read8(address + 28 + (index * 10 + offset) as u32);
            }
            self.st[index].set_bytes(&bytes);
        }
    }
}

// The memory helpers take `&mut dyn Bus` so both the CPU's generic bus and the
// FPU's own calls share them.
fn read16(bus: &mut dyn Bus, address: u32) -> u16 {
    bus.read16(address)
}

fn read32(bus: &mut dyn Bus, address: u32) -> u32 {
    read16(bus, address) as u32 | ((read16(bus, address + 2) as u32) << 16)
}

fn read64(bus: &mut dyn Bus, address: u32) -> u64 {
    read32(bus, address) as u64 | ((read32(bus, address + 4) as u64) << 32)
}

fn write16(bus: &mut dyn Bus, address: u32, value: u16) {
    bus.write16(address, value);
}

fn write32(bus: &mut dyn Bus, address: u32, value: u32) {
    write16(bus, address, value as u16);
    write16(bus, address + 2, (value >> 16) as u16);
}

fn write64(bus: &mut dyn Bus, address: u32, value: u64) {
    write32(bus, address, value as u32);
    write32(bus, address + 4, (value >> 32) as u32);
}

fn from_f32(bits: u32) -> F80 {
    from_f64(f32::from_bits(bits) as f64)
}

fn from_f64(value: f64) -> F80 {
    let mut result = F80::new();
    result.set_f64(value);
    result
}

fn from_f64_bits(bits: u64) -> F80 {
    from_f64(f64::from_bits(bits))
}

fn from_i32(value: i32) -> F80 {
    from_f64(value as f64)
}

fn from_i64(value: i64) -> F80 {
    from_f64(value as f64)
}

fn to_f32(value: F80) -> u32 {
    (value.get_f64() as f32).to_bits()
}

fn to_f64(value: F80) -> u64 {
    value.get_f64().to_bits()
}

fn to_i16(value: F80, control: u16) -> u16 {
    value.round_int(control).unwrap_or(0) as i16 as u16
}

fn to_i32(value: F80, control: u16) -> u32 {
    value.round_int(control).unwrap_or(0) as i32 as u32
}

fn to_i64(value: F80, control: u16) -> u64 {
    value.round_int(control).unwrap_or(0) as i64 as u64
}

fn add(left: F80, right: F80) -> F80 {
    let mut result = left;
    result.add(right);
    result
}

fn sub(left: F80, right: F80) -> F80 {
    let mut result = left;
    result.sub(right);
    result
}

fn mul(left: F80, right: F80) -> F80 {
    from_f64(left.get_f64() * right.get_f64())
}

fn div(left: F80, right: F80) -> F80 {
    from_f64(left.get_f64() / right.get_f64())
}

fn sqrt(value: f64) -> f64 {
    if !(value > 0.0) {
        return 0.0;
    }
    let mut result = value;
    for _ in 0..32 {
        result = 0.5 * (result + value / result);
    }
    result
}

fn pow2(exponent: i32) -> f64 {
    if exponent >= -1022 && exponent <= 1023 {
        f64::from_bits(((exponent + 1023) as u64) << 52)
    } else if exponent < -1074 {
        0.0
    } else if exponent < -1022 {
        f64::from_bits(1u64 << (exponent + 1074))
    } else {
        f64::INFINITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::Cpu;

    struct Mem {
        data: alloc::vec::Vec<u8>,
    }

    impl Mem {
        fn new() -> Self {
            Self {
                data: alloc::vec![0; 0x10_0000],
            }
        }
    }

    impl Bus for Mem {
        fn read8(&mut self, addr: u32) -> u8 {
            self.data[(addr & 0xF_FFFF) as usize]
        }
        fn write8(&mut self, addr: u32, value: u8) {
            self.data[(addr & 0xF_FFFF) as usize] = value;
        }
    }

    fn run(cpu: &mut Cpu, mem: &mut Mem, bytes: &[u8]) {
        cpu.sregs[crate::cpu::CS] = 0x1000;
        cpu.sregs[crate::cpu::DS] = 0;
        cpu.sregs[crate::cpu::SS] = 0x2000;
        cpu.ip = 0;
        cpu.r[crate::cpu::SP] = 0xFFFE;
        mem.data[0x10000..0x10000 + bytes.len()].copy_from_slice(bytes);
        for _ in 0..bytes.len() {
            if cpu.step(mem) != Step::Ok {
                break;
            }
        }
    }

    #[test]
    fn fld1_faddp_and_fstp() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        run(
            &mut cpu,
            &mut mem,
            &[
                0xD9, 0xE8, // FLD1
                0xD9, 0xE8, // FLD1
                0xDE, 0xC1, // FADDP ST(1), ST(0)
                0xD9, 0x1E, 0x00, 0x01, // FSTP dword [0x0100]
                0xF4,
            ],
        );
        let value = f32::from_le_bytes(mem.data[0x100..0x104].try_into().unwrap());
        assert_eq!(value, 2.0);
        assert_eq!(cpu.fpu.top, 0);
    }

    #[test]
    fn fild_and_fistp_round_trip() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        mem.data[0x100..0x102].copy_from_slice(&(-5i16).to_le_bytes());
        run(
            &mut cpu,
            &mut mem,
            &[
                0xDF, 0x06, 0x00, 0x01, // FILD word [0x0100]
                0xDB, 0x1E, 0x02, 0x01, // FISTP dword [0x0102]
                0xF4,
            ],
        );
        let value = i32::from_le_bytes(mem.data[0x102..0x106].try_into().unwrap());
        assert_eq!(value, -5);
    }

    #[test]
    fn fnstsw_reports_top_of_stack() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        run(
            &mut cpu,
            &mut mem,
            &[
                0xD9, 0xE8, // FLD1
                0xDF, 0xE0, // FNSTSW AX
                0xF4,
            ],
        );
        assert_eq!((cpu.r[crate::cpu::AX] >> 11) & 7, 7);
    }
}
